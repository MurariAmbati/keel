use keel_btree::BTree;
use keel_buffer::BufferPool;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_keys::encode_value;
use keel_pager::Pager;
use keel_rng::Rng;
use keel_types::{ColumnType, Value};
use keel_vfs::{BlockFile, MemDisk};
use std::collections::BTreeMap;
use std::sync::Arc;

const FRAMES: usize = 8;
const KEYS: u64 = 3000;

fn key_for(i: u64) -> Vec<u8> {
    encode_value(ColumnType::BigInt, &Value::BigInt(i as i64))
}

type Observed = (
    Vec<(Vec<u8>, u64)>,
    Vec<(Vec<u8>, u64)>,
    Vec<bool>,
    (u64, u64),
);

fn exercise<P: Pager>(bp: &P) -> Observed {
    let tree = BTree::create_rooted(bp).expect("create");
    let mut model: BTreeMap<Vec<u8>, u64> = BTreeMap::new();

    let mut order: Vec<u64> = (0..KEYS).collect();
    let mut rng = Rng::seed(0x5EED);
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    for &i in &order {
        let k = key_for(i);
        let rid = keel_heap::Rid {
            page: (i / 10) as u32,
            slot: (i % 10) as u16,
        };
        tree.insert(&k, rid).expect("insert");
        model.insert(k, i);
    }

    for &i in order.iter().filter(|i| *i % 7 == 0) {
        let k = key_for(i);
        assert!(tree.delete(&k).expect("delete"), "delete missed key {i}");
        model.remove(&k);
    }

    let scan: Vec<(Vec<u8>, u64)> = tree
        .scan_all()
        .expect("scan")
        .into_iter()
        .map(|(k, rid)| (k, rid.page as u64 * 10 + rid.slot as u64))
        .collect();

    let lo = key_for(100);
    let hi = key_for(200);
    let range: Vec<(Vec<u8>, u64)> = tree
        .range(&lo, Some(&hi))
        .expect("range")
        .into_iter()
        .map(|(k, rid)| (k, rid.page as u64 * 10 + rid.slot as u64))
        .collect();

    let probes: Vec<bool> = (0..KEYS)
        .step_by(3)
        .map(|i| tree.get(&key_for(i)).expect("get").is_some())
        .collect();

    let rep = tree.check().expect("check");
    assert!(rep.ok(), "B-tree invariants violated: {rep:?}");

    let model_scan: Vec<(Vec<u8>, u64)> = model.into_iter().collect();
    assert_eq!(scan, model_scan, "tree disagrees with the BTreeMap model");

    (scan, range, probes, (rep.leaves, rep.entries))
}

#[test]
fn the_same_btree_agrees_on_both_pools() {
    let disk_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let pool = BufferPool::open_default(disk_a, FRAMES).unwrap();
    let via_buffer = exercise(&pool);

    let disk_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk_b, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    let via_cache = exercise(&cache);

    assert_eq!(via_buffer.0, via_cache.0, "full scans differ between pools");
    assert_eq!(
        via_buffer.1, via_cache.1,
        "range scans differ between pools"
    );
    assert_eq!(
        via_buffer.2, via_cache.2,
        "point lookups differ between pools"
    );
    assert_eq!(
        via_buffer.3, via_cache.3,
        "tree shape (leaves, entries) differs between pools"
    );
    assert!(via_buffer.3 .0 > 1, "expected a multi-leaf tree");
}

#[test]
fn a_btree_built_on_the_concurrent_cache_survives_reopen() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let root = {
        let cache = PageCache::open_formatted(
            disk.clone(),
            FRAMES,
            Arc::new(NoWal),
            PageFormat::keel_page(),
        );
        let tree = BTree::create_rooted(&cache).unwrap();
        for i in 0..KEYS {
            tree.insert(
                &key_for(i),
                keel_heap::Rid {
                    page: i as u32,
                    slot: 0,
                },
            )
            .unwrap();
        }
        Pager::checkpoint(&cache).unwrap();
        tree.root()
    };

    let cache = PageCache::open_formatted(disk, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    let tree = BTree::open_rooted(&cache, root);
    assert_eq!(tree.scan_all().unwrap().len() as u64, KEYS);
    assert!(tree.get(&key_for(123)).unwrap().is_some());
    assert!(tree.check().unwrap().ok(), "invariants broken after reopen");
}
