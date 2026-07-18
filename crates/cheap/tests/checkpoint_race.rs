use keel_cbuffer::PageCache;
use keel_cheap::{Heap, Rid};
use keel_vfs::{BlockFile, MemDisk};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const THREADS: u64 = 4;
const PER: u64 = 700;
const FRAMES: usize = 4;

fn collect_ids(recs: &[(Rid, Vec<u8>)]) -> HashSet<u64> {
    let mut out = HashSet::new();
    for (_rid, rec) in recs {
        let a = u64::from_le_bytes(rec[..8].try_into().unwrap());
        let b = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        assert_eq!(a, b, "torn record: {a} != {b}");
        assert!(out.insert(a), "duplicate id {a}");
    }
    out
}

#[test]
fn concurrent_checkpoint_loses_no_insert() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = Arc::new(PageCache::open(disk.clone(), FRAMES));
    let heap = Arc::new(Heap::open(cache));
    let stop = Arc::new(AtomicBool::new(false));

    let ck = {
        let heap = heap.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                heap.checkpoint().expect("checkpoint");
            }
        })
    };

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let heap = heap.clone();
        handles.push(thread::spawn(move || {
            for i in 0..PER {
                let gid = t * PER + i;
                let mut rec = vec![0u8; 16];
                rec[..8].copy_from_slice(&gid.to_le_bytes());
                rec[8..].copy_from_slice(&gid.to_le_bytes());
                heap.insert(&rec).expect("insert");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    ck.join().unwrap();

    let expect: HashSet<u64> = (0..THREADS * PER).collect();
    assert_eq!(
        collect_ids(&heap.scan().expect("scan")),
        expect,
        "a concurrent checkpoint marked an inserted record clean and it was evicted away"
    );

    heap.checkpoint().unwrap();
    let rep = heap.verify().expect("verify");
    assert!(
        rep.is_clean(),
        "concurrent checkpointing left the heap structurally invalid: {rep:?}"
    );
    let heap2 = Heap::open(Arc::new(PageCache::open(disk, FRAMES)));
    assert_eq!(
        collect_ids(&heap2.scan().expect("scan")),
        expect,
        "records did not survive checkpoint + reopen"
    );
}
