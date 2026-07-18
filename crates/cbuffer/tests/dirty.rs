//! Race oracle for dirty-page eviction under WAL-before-data (D-LATCH-4, slice 2).
//!
//! Two invariants are checked at once, under real concurrency with disk I/O
//! outside the directory lock:
//!
//! 1. **WAL-before-data** — a `GuardDisk` wraps the backing file and, on every
//!    write, reads the page's embedded LSN and asserts the shared WAL is already
//!    durable through it. If the cache ever wrote a dirty page before forcing the
//!    log, the guard records a violation. (This is the same check
//!    `buffer::flush_frame` makes with a `debug_assert`, here promoted to a live
//!    counter across threads.)
//! 2. **No wrong page under a pin** — every page keeps its id stamp through
//!    countless dirty evictions and reloads; the two-copies-of-a-dirty-page race
//!    would surface as a mismatched stamp.

use keel_cbuffer::{PageCache, WalSync};
use keel_page::PAGE_SIZE;
use keel_vfs::{BlockFile, MemDisk};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

/// Shared WAL state: a single global high-water LSN, the log's durable frontier.
struct SharedWal {
    flushed: AtomicU64,
}
impl WalSync for SharedWal {
    fn flushed_lsn(&self) -> u64 {
        self.flushed.load(Ordering::SeqCst)
    }
    fn flush_until(&self, lsn: u64) -> io::Result<()> {
        self.flushed.fetch_max(lsn, Ordering::SeqCst);
        Ok(())
    }
}

/// A backing file that enforces WAL-before-data: every write must find the log
/// already durable through the page's LSN, or it is a counted violation.
struct GuardDisk {
    inner: Arc<dyn BlockFile>,
    wal: Arc<SharedWal>,
    violations: AtomicU64,
}
impl BlockFile for GuardDisk {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let lsn = u64::from_le_bytes(buf[4..12].try_into().unwrap());
        if self.wal.flushed_lsn() < lsn {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.write_at(buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
}

fn lsn_at_8(buf: &[u8]) -> u64 {
    u64::from_le_bytes(buf[4..12].try_into().unwrap())
}

#[test]
fn concurrent_dirty_eviction_honours_wal_before_data() {
    const PAGES: u32 = 16;
    const CAP: usize = 6;
    const THREADS: usize = 4;
    const ITERS: usize = 20_000;

    let mem = {
        let disk = MemDisk::new();
        let mut page = vec![0u8; PAGE_SIZE];
        for pid in 0..PAGES {
            page[..4].copy_from_slice(&pid.to_le_bytes());
            page[4..12].copy_from_slice(&0u64.to_le_bytes());
            disk.write_at(&page, pid as u64 * PAGE_SIZE as u64).unwrap();
        }
        Arc::new(disk) as Arc<dyn BlockFile>
    };
    let wal = Arc::new(SharedWal {
        flushed: AtomicU64::new(0),
    });
    let guard: Arc<GuardDisk> = Arc::new(GuardDisk {
        inner: mem,
        wal: wal.clone(),
        violations: AtomicU64::new(0),
    });
    let cache = Arc::new(PageCache::open_wal(
        guard.clone() as Arc<dyn BlockFile>,
        CAP,
        wal.clone(),
        lsn_at_8,
    ));
    let next_lsn = Arc::new(AtomicU64::new(1));

    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let cache = cache.clone();
        let next_lsn = next_lsn.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0x1234_9E37_79B9_0001u64 ^ ((tid as u64 + 1) << 32);
            for _ in 0..ITERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let pid = (s % PAGES as u64) as u32;
                let p = cache.fetch(pid).expect("THREADS < CAP: never exhausted");
                assert_eq!(
                    u32::from_le_bytes(p.read()[..4].try_into().unwrap()),
                    pid,
                    "a pinned frame held the wrong page"
                );
                if s & 1 == 0 {
                    let lsn = next_lsn.fetch_add(1, Ordering::SeqCst);
                    let mut b = p.write();
                    b[4..12].copy_from_slice(&lsn.to_le_bytes());
                }
                drop(p);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    cache.flush_all().unwrap();

    assert_eq!(
        guard.violations.load(Ordering::SeqCst),
        0,
        "a dirty page was written to disk before the WAL was durable through its LSN"
    );
    assert!(
        cache.flushes() > 0,
        "no dirty flush happened — the WAL path was never exercised"
    );
    assert!(cache.evictions() > 0, "no eviction happened");
    assert_eq!(cache.live_pins(), 0, "all pins released");
}
