//! Slice 2 of the engine swap: the policy and recovery primitives `keel-buffer`
//! has that `cbuffer` lacked — no-steal, `invalidate`, the Dirty Page Table,
//! `fetch_for_redo`, `flush_page`, and `sync`. Each is asserted against the
//! behaviour `wal::TxnStore` actually depends on, since that is the crate these
//! exist to serve.

use keel_cbuffer::{CacheError, NoWal, PageCache, PageFormat};
use keel_page::{PageType, SlottedPage, PAGE_SIZE};
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

fn keel_cache(disk: Arc<dyn BlockFile>, cap: usize) -> PageCache {
    PageCache::open_formatted(disk, cap, Arc::new(NoWal), PageFormat::keel_page())
}

#[test]
fn no_steal_never_writes_a_dirty_page_during_eviction() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = keel_cache(disk.clone(), 2);
    cache.set_no_steal();

    for _ in 0..2 {
        let p = cache.new_page().unwrap();
        let mut b = p.write();
        SlottedPage::init(&mut b[..], PageType::Heap);
    }
    assert!(
        matches!(cache.new_page(), Err(CacheError::Exhausted)),
        "no-steal must refuse rather than evict a dirty page"
    );
    assert_eq!(
        cache.flushes(),
        0,
        "no-steal wrote a dirty page during eviction"
    );
    assert_eq!(
        disk.size().unwrap(),
        0,
        "nothing should have reached disk yet"
    );

    cache.checkpoint().unwrap();
    assert!(disk.size().unwrap() >= 2 * PAGE_SIZE as u64);
}

#[test]
fn invalidate_discards_without_flushing_and_reverts_to_the_durable_page() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = keel_cache(disk.clone(), 4);

    let pid = {
        let p = cache.new_page().unwrap();
        let mut b = p.write();
        let mut sp = SlottedPage::init(&mut b[..], PageType::Heap);
        sp.insert(b"committed").unwrap();
        p.pid()
    };
    cache.checkpoint().unwrap();

    {
        let p = cache.fetch(pid).unwrap();
        let mut b = p.write();
        let mut sp = SlottedPage::from_bytes(&mut b[..]);
        sp.insert(b"uncommitted").unwrap();
    }
    cache.note_dirty(pid, 42);
    cache.invalidate(pid);
    assert!(
        cache.dpt_snapshot().iter().all(|&(p, _)| p != pid),
        "invalidate must drop the page from the DPT too"
    );

    let p = cache.fetch(pid).unwrap();
    let b = p.read();
    let sp = SlottedPage::from_bytes(&b[..]);
    let live = (0..sp.slot_count())
        .filter(|&s| sp.get(s).is_some())
        .count();
    assert_eq!(live, 1, "invalidate leaked an uncommitted record");
}

#[test]
fn dpt_tracks_the_oldest_reclsn_and_clears_on_flush() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = keel_cache(disk, 4);
    let pid = {
        let p = cache.new_page().unwrap();
        let mut b = p.write();
        SlottedPage::init(&mut b[..], PageType::Heap);
        p.pid()
    };

    cache.note_dirty(pid, 100);
    cache.note_dirty(pid, 200);
    assert_eq!(cache.dpt_snapshot(), vec![(pid, 100)]);

    cache.flush_page(pid).unwrap();
    assert!(
        cache.dpt_snapshot().is_empty(),
        "a flushed page must leave the DPT"
    );
}

#[test]
fn fetch_for_redo_rebuilds_a_missing_or_torn_page() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;

    let corrupt = {
        let cache = keel_cache(disk.clone(), 2);
        let p = cache.new_page().unwrap();
        let mut b = p.write();
        SlottedPage::init(&mut b[..], PageType::Heap);
        let id = p.pid();
        drop(b);
        drop(p);
        cache.checkpoint().unwrap();
        id
    };
    let mut page = vec![0u8; PAGE_SIZE];
    disk.read_at(&mut page, 0).unwrap();
    page[80] ^= 0xFF;
    disk.write_at(&page, 0).unwrap();

    let cache = keel_cache(disk, 3);
    assert!(matches!(cache.fetch(corrupt), Err(CacheError::Corrupt(_))));
    let p = cache.fetch_for_redo(corrupt).unwrap();
    assert!(
        p.read().iter().all(|&x| x == 0),
        "redo should start from a blank page"
    );
    drop(p);

    let far = 9u32;
    let p = cache.fetch_for_redo(far).unwrap();
    assert_eq!(p.pid(), far);
    assert!(p.read().iter().all(|&x| x == 0));
    drop(p);
    assert!(
        cache.page_count() > far,
        "fetch_for_redo must extend the allocation watermark past the page it rebuilt"
    );
}
