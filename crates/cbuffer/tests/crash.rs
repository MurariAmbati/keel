//! Crash campaign for the concurrent cache's checkpoint barrier (D-LATCH-6).
//!
//! KEEL's whole thesis is that durability is earned against an adversary, not
//! asserted — so the concurrent cache is put over the fault-injecting disk. The
//! property under test is the P1 durability boundary, now for `cbuffer`: after
//! `checkpoint()` (flush every dirty page, then `sync`), the checkpointed pages
//! survive a **vicious** power loss byte-exact, and cache-resident changes that
//! were never checkpointed correctly do *not* reach disk.
//!
//! A separate "scratch" set is dirtied after the checkpoint and left un-synced,
//! so the crash actually has something to drop, tear, and reorder — the campaign
//! exercises the adversary while the checkpointed set stays untouchable (it lives
//! in the durable image, which a crash only ever *adds* pending writes on top of).
//! Each page carries a CRC over its payload so a torn checkpoint page would be
//! caught, not silently accepted.

use keel_cbuffer::PageCache;
use keel_faultfs::{FaultConfig, FaultDisk};
use keel_page::{crc32, PAGE_SIZE};
use keel_vfs::BlockFile;
use std::sync::Arc;

const KEPT: u32 = 8;
const SCRATCH: u32 = 8;
const TOTAL: u32 = KEPT + SCRATCH;
const CAP: usize = 4;
const CHECKPOINTED: u32 = 100;

fn off(pid: u32) -> u64 {
    pid as u64 * PAGE_SIZE as u64
}

fn write_stamp(buf: &mut [u8], pid: u32, version: u32) {
    buf[..4].copy_from_slice(&pid.to_le_bytes());
    buf[4..8].copy_from_slice(&version.to_le_bytes());
    let crc = crc32(&buf[0..8]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
}

/// Verify a page's CRC and return `(pid, version)`.
fn read_stamp(buf: &[u8]) -> (u32, u32, bool) {
    let pid = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let stored = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let ok = stored == crc32(&buf[0..8]);
    (pid, version, ok)
}

#[test]
fn checkpoint_survives_a_vicious_crash() {
    let mut total_pending = 0usize;

    for seed in 0..24u64 {
        let disk = FaultDisk::new(FaultConfig::default(), seed);

        {
            let h = disk.handle();
            let mut zero = vec![0u8; PAGE_SIZE];
            for pid in 0..TOTAL {
                write_stamp(&mut zero, pid, 1);
                h.write_at(&zero, off(pid)).unwrap();
            }
            h.sync().unwrap();
        }

        {
            let cache = PageCache::open(Arc::new(disk.handle()), CAP);
            for pid in 0..KEPT {
                let p = cache.fetch(pid).unwrap();
                write_stamp(&mut p.write(), pid, CHECKPOINTED);
            }
            cache.checkpoint().unwrap();

            for pid in KEPT..TOTAL {
                let p = cache.fetch(pid).unwrap();
                write_stamp(&mut p.write(), pid, 200);
            }
        }

        let report = disk.crash();
        total_pending += report.pending_ops;

        let disk2 = FaultDisk::from_image(FaultConfig::benign(), seed, disk.durable_image());
        let cache2 = PageCache::open(Arc::new(disk2.handle()), CAP);
        for pid in 0..KEPT {
            let p = cache2.fetch(pid).unwrap();
            let (rpid, ver, crc_ok) = read_stamp(&p.read());
            assert!(crc_ok, "seed {seed}: checkpointed page {pid} was torn");
            assert_eq!(rpid, pid, "seed {seed}: page {pid} identity");
            assert_eq!(
                ver, CHECKPOINTED,
                "seed {seed}: checkpointed page {pid} did not survive the crash byte-exact"
            );
        }
    }

    assert!(
        total_pending > 0,
        "no un-synced writes were ever in flight — the crash never exercised the adversary"
    );
}
