//! Regression for the stale-checksum bug an adversarial review surfaced: when an
//! insert takes a page's write latch and `SlottedPage::insert` internally runs
//! `compact()` but still returns `PageFull`, the page's header bytes changed yet
//! the stored checksum was NOT recomputed — so a checkpoint persisted a page whose
//! checksum is stale, and a checksum-verifying reader (crash recovery / dbcheck)
//! would drop the *intact* committed records on it as "torn".
//!
//! On a `MemDisk` (which never tears), EVERY page must pass `verify_checksum()`
//! after a checkpoint. Both tests below would fail before the fix.

use keel_cbuffer::PageCache;
use keel_cheap::Heap;
use keel_page::{SlottedPage, MAX_TUPLE_SIZE};
use keel_rng::Rng;
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

fn rec_sized(key: u64, len: usize) -> Vec<u8> {
    let len = len.max(8);
    let mut v = vec![(key & 0xff) as u8; len];
    v[..8].copy_from_slice(&key.to_le_bytes());
    v
}

fn all_pages_verify(disk: &Arc<dyn BlockFile>) -> (u64, u64) {
    let cache = PageCache::open(disk.clone(), 8);
    let mut ok = 0;
    let mut bad = 0;
    for pid in 0..cache.page_count() {
        let p = cache.fetch(pid).unwrap();
        let b = p.read();
        if SlottedPage::from_bytes(&b[..]).verify_checksum() {
            ok += 1;
        } else {
            bad += 1;
        }
    }
    (ok, bad)
}

#[test]
fn pagefull_after_compaction_keeps_checksum_valid() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let big = MAX_TUPLE_SIZE - 200;
    {
        let cache = Arc::new(PageCache::open(disk.clone(), 4));
        let heap = Heap::open(cache);
        heap.insert(&rec_sized(0, big)).unwrap();
        let small = heap.insert(&rec_sized(1, 50)).unwrap();
        assert!(heap.delete(small).unwrap());
        heap.insert(&rec_sized(2, big / 2)).unwrap();
        heap.checkpoint().unwrap();
    }
    let (ok, bad) = all_pages_verify(&disk);
    assert_eq!(
        bad, 0,
        "{bad} page(s) failed checksum after checkpoint (stale checksum)"
    );
    assert!(
        ok >= 2,
        "expected the record to have spilled onto a second page"
    );

    let cache = Arc::new(PageCache::open(disk, 8));
    let heap = Heap::open(cache);
    let keys: Vec<u64> = heap
        .scan()
        .unwrap()
        .iter()
        .map(|(_, r)| u64::from_le_bytes(r[..8].try_into().unwrap()))
        .collect();
    assert!(keys.contains(&0), "committed record 0 lost");
    assert!(keys.contains(&2), "record 2 lost");
}

#[test]
fn every_page_self_verifies_after_random_insert_delete() {
    for seed in 0..12u64 {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        {
            let cache = Arc::new(PageCache::open(disk.clone(), 3));
            let heap = Heap::open(cache);
            let mut rng = Rng::seed(0xA11CE ^ seed);
            let mut live = Vec::new();
            for k in 0..400u64 {
                if !live.is_empty() && rng.below(3) == 0 {
                    let i = rng.below(live.len() as u64) as usize;
                    let rid = live.swap_remove(i);
                    heap.delete(rid).unwrap();
                } else {
                    let len = 1 + rng.below((MAX_TUPLE_SIZE as u64) / 2) as usize;
                    live.push(heap.insert(&rec_sized(k, len)).unwrap());
                }
            }
            heap.checkpoint().unwrap();
        }
        let (_, bad) = all_pages_verify(&disk);
        assert_eq!(
            bad, 0,
            "seed {seed}: {bad} page(s) failed checksum after checkpoint"
        );
    }
}
