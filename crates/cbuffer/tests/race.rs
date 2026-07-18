use keel_cbuffer::PageCache;
use keel_page::PAGE_SIZE;
use keel_vfs::{BlockFile, MemDisk};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

fn stamped_disk(n: u32) -> Arc<dyn BlockFile> {
    let disk = MemDisk::new();
    let mut page = vec![0u8; PAGE_SIZE];
    for pid in 0..n {
        page[..4].copy_from_slice(&pid.to_le_bytes());
        disk.write_at(&page, pid as u64 * PAGE_SIZE as u64).unwrap();
    }
    Arc::new(disk)
}

#[test]
fn concurrent_fetches_never_return_the_wrong_page() {
    const PAGES: u32 = 24;
    const CAP: usize = 8;
    const THREADS: usize = 6;
    const ITERS: usize = 30_000;

    let cache = Arc::new(PageCache::open(stamped_disk(PAGES), CAP));
    let checked = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let cache = cache.clone();
        let checked = checked.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0xC2B2_AE3D_27D4_EB4Fu64 ^ ((tid as u64 + 1) << 40);
            for _ in 0..ITERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let pid = (s % PAGES as u64) as u32;
                let p = cache
                    .fetch(pid)
                    .expect("THREADS < CAP: an unpinned victim always exists");
                let stamp = u32::from_le_bytes(p.read()[..4].try_into().unwrap());
                assert_eq!(stamp, pid, "a pinned frame held the wrong page");
                assert_eq!(p.pid(), pid);
                checked.fetch_add(1, Ordering::Relaxed);
                drop(p);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        checked.load(Ordering::Relaxed),
        (THREADS * ITERS) as u64,
        "every fetch succeeded and was verified"
    );
    assert!(
        cache.loads() > CAP as u64,
        "eviction pressure never materialised — the test would be vacuous"
    );
    assert!(cache.evictions() > 0, "replacement was never exercised");
    assert_eq!(cache.live_pins(), 0, "all pins released");
}
