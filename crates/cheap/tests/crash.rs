use keel_cbuffer::PageCache;
use keel_faultfs::{FaultConfig, FaultDisk};
use keel_page::{PageType, SlottedPage};
use std::collections::HashSet;
use std::sync::Arc;

const N: u64 = 900;
const M: u64 = 2400;
const FRAMES: usize = 3;

fn rec(key: u64) -> Vec<u8> {
    let mut v = vec![0u8; 16];
    v[..8].copy_from_slice(&key.to_le_bytes());
    v[8..].copy_from_slice(&key.to_le_bytes());
    v
}

#[test]
fn checkpoint_barrier_and_torn_detection_under_crash() {
    let mut total_pending = 0usize;
    let mut total_torn = 0usize;
    let mut seeds_with_surviving_tail = 0usize;

    for seed in 0..24u64 {
        let disk = FaultDisk::new(FaultConfig::default(), seed);
        {
            let cache = Arc::new(PageCache::open(Arc::new(disk.handle()), FRAMES));
            let heap = keel_cheap::Heap::open(cache);
            for k in 0..N {
                heap.insert(&rec(k)).unwrap();
            }
            heap.checkpoint().unwrap();
            heap.seal();
            for k in N..N + M {
                heap.insert(&rec(k)).unwrap();
            }
        }

        let report = disk.crash();
        total_pending += report.pending_ops;

        let disk2 = FaultDisk::from_image(FaultConfig::benign(), seed, disk.durable_image());
        let cache = PageCache::open(Arc::new(disk2.handle()), FRAMES);
        let mut found: HashSet<u64> = HashSet::new();

        for pid in 0..cache.page_count() {
            let p = cache.fetch(pid).unwrap();
            let b = p.read();
            let sp = SlottedPage::from_bytes(&b[..]);
            if !sp.verify_checksum() {
                total_torn += 1;
                continue;
            }
            if sp.page_type() != Some(PageType::Heap) {
                continue;
            }
            for slot in 0..sp.slot_count() {
                if let Some(r) = sp.get(slot) {
                    let a = u64::from_le_bytes(r[..8].try_into().unwrap());
                    let b2 = u64::from_le_bytes(r[8..16].try_into().unwrap());
                    assert_eq!(a, b2, "seed {seed}: an intact page yielded a torn record");
                    assert!(
                        a < N + M,
                        "seed {seed}: intact page yielded garbage key {a}"
                    );
                    found.insert(a);
                }
            }
        }

        for k in 0..N {
            assert!(
                found.contains(&k),
                "seed {seed}: committed key {k} missing after crash — the sealed \
                 checkpoint barrier leaked"
            );
        }
        if found.iter().any(|&k| k >= N) {
            seeds_with_surviving_tail += 1;
        }
    }

    assert!(
        total_pending > 0,
        "no un-synced writes were in flight — the crash never exercised the adversary"
    );
    let _ = (total_torn, seeds_with_surviving_tail);
}
