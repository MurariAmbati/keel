use keel_cbuffer::PageCache;
use keel_vfs::{BlockFile, MemDisk};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;

#[test]
fn concurrent_new_page_allocates_unique_ids() {
    const THREADS: usize = 6;
    const PER: usize = 500;
    const FRAMES: usize = 8;

    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = Arc::new(PageCache::open(disk.clone(), FRAMES));
    let ids = Arc::new(Mutex::new(Vec::<u32>::new()));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let cache = cache.clone();
        let ids = ids.clone();
        handles.push(thread::spawn(move || {
            let mut mine = Vec::with_capacity(PER);
            for _ in 0..PER {
                let p = cache.new_page().expect("allocation");
                let pid = p.pid();
                p.write()[..4].copy_from_slice(&pid.to_le_bytes());
                mine.push(pid);
                drop(p);
            }
            ids.lock().unwrap().extend(mine);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    cache.checkpoint().unwrap();

    let ids = Arc::try_unwrap(ids).unwrap().into_inner().unwrap();
    assert_eq!(ids.len(), THREADS * PER, "every allocation returned an id");
    let unique: HashSet<u32> = ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "no two concurrent allocations shared a page id"
    );
    assert_eq!(cache.allocations(), (THREADS * PER) as u64);

    let cache2 = PageCache::open(disk, FRAMES);
    for &pid in &unique {
        let p = cache2.fetch(pid).unwrap();
        let stamp = u32::from_le_bytes(p.read()[..4].try_into().unwrap());
        assert_eq!(
            stamp, pid,
            "allocated page {pid} lost or aliased across reopen"
        );
    }
}
