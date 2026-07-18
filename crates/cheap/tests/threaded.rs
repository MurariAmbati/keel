use keel_cbuffer::PageCache;
use keel_cheap::{Heap, Rid};
use keel_vfs::{BlockFile, MemDisk};
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

const THREADS: u64 = 6;
const PER: u64 = 400;
const FRAMES: usize = 6;

fn collect_ids(recs: &[(Rid, Vec<u8>)]) -> HashSet<u64> {
    let mut out = HashSet::new();
    for (_rid, rec) in recs {
        assert_eq!(rec.len(), 16, "record wrong length");
        let a = u64::from_le_bytes(rec[..8].try_into().unwrap());
        let b = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        assert_eq!(a, b, "record torn: halves disagree ({a} != {b})");
        assert!(out.insert(a), "duplicate global id {a} in scan");
    }
    out
}

#[test]
fn concurrent_inserts_lose_no_record_and_persist() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = Arc::new(PageCache::open(disk.clone(), FRAMES));
    let heap = Arc::new(Heap::open(cache));

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let heap = heap.clone();
        handles.push(thread::spawn(move || {
            let mut rids = Vec::with_capacity(PER as usize);
            for i in 0..PER {
                let gid = t * PER + i;
                let mut rec = vec![0u8; 16];
                rec[..8].copy_from_slice(&gid.to_le_bytes());
                rec[8..].copy_from_slice(&gid.to_le_bytes());
                rids.push(heap.insert(&rec).expect("insert"));
            }
            rids
        }));
    }
    let mut all_rids = Vec::new();
    for h in handles {
        all_rids.extend(h.join().unwrap());
    }

    let unique: HashSet<Rid> = all_rids.iter().copied().collect();
    assert_eq!(unique.len(), all_rids.len(), "two inserts shared a RID");

    let expect: HashSet<u64> = (0..THREADS * PER).collect();
    let seen = collect_ids(&heap.scan().expect("scan"));
    assert_eq!(
        seen, expect,
        "live-run scan lost / duplicated / tore a record"
    );

    heap.checkpoint().unwrap();
    let rep = heap.verify().expect("verify");
    assert!(rep.is_clean(), "heap invariants violated: {rep:?}");
    assert_eq!(rep.live_records, THREADS * PER, "verify lost a record");
    let cache2 = Arc::new(PageCache::open(disk, FRAMES));
    let heap2 = Heap::open(cache2);
    let seen2 = collect_ids(&heap2.scan().expect("scan"));
    assert_eq!(seen2, expect, "records did not survive checkpoint + reopen");
}
