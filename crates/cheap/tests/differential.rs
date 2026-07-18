//! Differential oracle: a random insert / get / delete / scan sequence against a
//! `HashMap<Rid, Vec<u8>>` model, through a deliberately tiny cache so pages
//! constantly evict and reload. After every operation the heap must agree with the
//! model; every so often the full `scan()` must equal the model exactly.
//!
//! Slot reuse (an insert reclaiming a tombstoned slot) means a fresh insert can
//! return a RID equal to a previously deleted one — the model keyed by RID stays
//! consistent because the old occupant was removed first, so this also exercises
//! that recycling path.

use keel_cheap::{Heap, Rid};
use keel_page::PAGE_SIZE;
use keel_rng::Rng;
use keel_vfs::{BlockFile, MemDisk};
use std::collections::HashMap;
use std::sync::Arc;

fn rec_for(counter: u64) -> Vec<u8> {
    let len = 8 + (counter as usize % 120);
    let mut v = vec![(counter & 0xff) as u8; len];
    v[..8].copy_from_slice(&counter.to_le_bytes());
    v
}

#[test]
fn heap_matches_hashmap_model_under_eviction() {
    for seed in 0..16u64 {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let cache = Arc::new(keel_cbuffer::PageCache::open(disk, 4));
        let heap = Heap::open(cache);

        let mut model: HashMap<Rid, Vec<u8>> = HashMap::new();
        let mut all_rids: Vec<Rid> = Vec::new();
        let mut rng = Rng::seed(0xC0FFEE ^ seed);
        let mut counter = 0u64;

        for step in 0..3000u64 {
            match rng.below(10) {
                0..=5 => {
                    let rec = rec_for(counter);
                    counter += 1;
                    let rid = heap.insert(&rec).expect("insert");
                    model.insert(rid, rec);
                    all_rids.push(rid);
                }
                6..=7 if !all_rids.is_empty() => {
                    let rid = all_rids[rng.below(all_rids.len() as u64) as usize];
                    let got = heap.get(rid).expect("get");
                    assert_eq!(
                        got.as_ref(),
                        model.get(&rid),
                        "seed {seed} step {step}: get {rid:?} disagrees with model"
                    );
                }
                8 if !all_rids.is_empty() => {
                    let rid = all_rids[rng.below(all_rids.len() as u64) as usize];
                    let model_had = model.remove(&rid).is_some();
                    let heap_had = heap.delete(rid).expect("delete");
                    assert_eq!(
                        heap_had, model_had,
                        "seed {seed} step {step}: delete {rid:?} existence disagrees"
                    );
                }
                _ => {}
            }

            if step % 250 == 0 {
                let got: HashMap<Rid, Vec<u8>> = heap.scan().expect("scan").into_iter().collect();
                assert_eq!(got, model, "seed {seed} step {step}: scan != model");
            }
        }

        let got: HashMap<Rid, Vec<u8>> = heap.scan().expect("scan").into_iter().collect();
        assert_eq!(got, model, "seed {seed}: final scan != model");

        let rep = heap.verify().expect("verify");
        assert!(
            rep.is_clean(),
            "seed {seed}: heap invariants violated: {rep:?}"
        );
        assert_eq!(
            rep.live_records as usize,
            model.len(),
            "seed {seed}: verify counted a different number of live records than the model"
        );

        assert!(
            got.len() as u64 * 8 < PAGE_SIZE as u64 * 200,
            "sanity bound only"
        );
    }
}
