use keel_ckv::PagedKv;
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;
use std::thread;

#[test]
fn concurrent_increments_lose_nothing() {
    const BUCKETS: u32 = 16;
    const FRAMES: usize = 8;
    const KEYS: u64 = 40;
    const THREADS: usize = 6;
    const ITERS: usize = 20_000;

    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let kv = Arc::new(PagedKv::create(disk, BUCKETS, FRAMES).unwrap());
    for k in 0..KEYS {
        kv.put(k, 0).unwrap();
    }

    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let kv = kv.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ ((tid as u64 + 1) << 32);
            for _ in 0..ITERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let k = s % KEYS;
                kv.update(k, 0, |v| v + 1).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        kv.total().unwrap(),
        (THREADS * ITERS) as u128,
        "grand total must equal every increment issued — no lost updates"
    );
    for bkt in 0..BUCKETS {
        assert!(
            kv.bucket_intact(bkt).unwrap(),
            "bucket {bkt} checksum after churn"
        );
    }
}
