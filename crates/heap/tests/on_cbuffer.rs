//! The heap runs on the **concurrent** page cache (engine-swap slice 4b).
//!
//! `HeapFile` is now generic over `keel_pager::Pager`, defaulting to `BufferPool` so
//! `db`, `wal`, and `dbcheck` compile untouched. This drives the *same* heap over
//! both pools and requires exact agreement.
//!
//! The workload deliberately hits the paths that made `heap` the riskier half of
//! this slice: the **FSM rebuild** in `open` (which scans every page), and the
//! **forwarding-stub** path, where an update that no longer fits in place relocates
//! the record to another page and leaves a stub behind — the one place the heap
//! touches two pages for one logical operation. Those were the sites most likely to
//! hold a page guard across another fetch, which closure-scoped access forbids; the
//! crate turned out to already copy bytes out and drop the guard first, which is why
//! the conversion was mechanical. This test is the evidence for that claim.

use keel_buffer::BufferPool;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_heap::HeapFile;
use keel_pager::Pager;
use keel_rng::Rng;
use keel_vfs::{BlockFile, MemDisk};
use std::collections::BTreeMap;
use std::sync::Arc;

const FRAMES: usize = 6;
const N: u64 = 300;

/// Everything observable: the ordered live set, per-RID probes, and (forward hops,
/// distinct pages spanned) — the latter pair proving the interesting paths ran.
type Observed = (Vec<(u32, u16, Vec<u8>)>, Vec<Option<Vec<u8>>>, (u64, u64));

fn exercise<P: Pager>(bp: &P) -> Observed {
    let heap = HeapFile::open(bp).expect("open");
    let mut model: BTreeMap<(u32, u16), Vec<u8>> = BTreeMap::new();
    let mut rids = Vec::new();

    for i in 0..N {
        let rec = format!("rec-{i:04}").into_bytes();
        let rid = heap.insert(&rec).expect("insert");
        model.insert((rid.page, rid.slot), rec);
        rids.push(rid);
    }

    let mut rng = Rng::seed(0xF02_2A2D);
    for (n, &rid) in rids.iter().enumerate() {
        if n % 3 != 0 {
            continue;
        }
        let big = vec![b'x'; 900 + rng.below(600) as usize];
        assert!(heap.update(rid, &big).expect("update"), "update {rid:?}");
        model.insert((rid.page, rid.slot), big);
    }

    for (n, &rid) in rids.iter().enumerate() {
        if n % 5 == 0 {
            assert!(heap.delete(rid).expect("delete"));
            model.remove(&(rid.page, rid.slot));
        }
    }

    let heap = HeapFile::open(bp).expect("reopen");

    let mut scan: Vec<(u32, u16, Vec<u8>)> = heap
        .scan()
        .expect("scan")
        .into_iter()
        .map(|(rid, rec)| (rid.page, rid.slot, rec))
        .collect();
    scan.sort();

    let probes: Vec<Option<Vec<u8>>> = rids.iter().map(|&r| heap.get(r).expect("get")).collect();

    let hops = heap.stats().forward_hops;
    let pages_allocated = Pager::page_count(bp) as u64;
    let model_scan: Vec<(u32, u16, Vec<u8>)> =
        model.into_iter().map(|((p, s), v)| (p, s, v)).collect();
    assert_eq!(scan, model_scan, "heap disagrees with the model");

    (scan, probes, (hops, pages_allocated))
}

#[test]
fn the_same_heap_agrees_on_both_pools() {
    let disk_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let pool = BufferPool::open_default(disk_a, FRAMES).unwrap();
    let via_buffer = exercise(&pool);

    let disk_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk_b, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    let via_cache = exercise(&cache);

    assert_eq!(
        via_buffer.0, via_cache.0,
        "heap scans differ between the two pools"
    );
    assert_eq!(
        via_buffer.1, via_cache.1,
        "per-RID reads differ between the two pools"
    );
    assert!(
        via_buffer.2 .0 > 0,
        "no forward hops: the stub path never ran"
    );
    assert!(
        via_buffer.2 .1 > 1,
        "expected the relocations to have grown the heap over many pages"
    );
}

#[test]
fn a_heap_built_on_the_concurrent_cache_survives_reopen() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let cache = PageCache::open_formatted(
            disk.clone(),
            FRAMES,
            Arc::new(NoWal),
            PageFormat::keel_page(),
        );
        let heap = HeapFile::open(&cache).unwrap();
        for i in 0..N {
            heap.insert(format!("durable-{i:04}").as_bytes()).unwrap();
        }
        Pager::checkpoint(&cache).unwrap();
    }
    let cache = PageCache::open_formatted(disk, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    let heap = HeapFile::open(&cache).unwrap();
    let rows = heap.scan().unwrap();
    assert_eq!(
        rows.len() as u64,
        N,
        "records lost across checkpoint+reopen"
    );
    assert!(rows.iter().all(|(_, r)| r.starts_with(b"durable-")));
}
