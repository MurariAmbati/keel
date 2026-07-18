use keel_cbuffer::{CacheError, NoWal, PageCache, PageFormat};
use keel_page::{PageType, SlottedPage, PAGE_SIZE};
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

fn keel_cache(disk: Arc<dyn BlockFile>, cap: usize) -> PageCache {
    PageCache::open_formatted(disk, cap, Arc::new(NoWal), PageFormat::keel_page())
}

#[test]
fn the_cache_stamps_so_the_caller_never_has_to() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let cache = keel_cache(disk.clone(), 2);
        for _ in 0..6 {
            let p = cache.new_page().unwrap();
            let mut b = p.write();
            let mut sp = SlottedPage::init(&mut b[..], PageType::Heap);
            sp.insert(b"a record").unwrap();
        }
        cache.checkpoint().unwrap();
    }

    let mut page = vec![0u8; PAGE_SIZE];
    let pages = disk.size().unwrap() / PAGE_SIZE as u64;
    assert!(pages >= 6, "expected the allocations to reach disk");
    for pid in 0..pages {
        disk.read_at(&mut page, pid * PAGE_SIZE as u64).unwrap();
        assert!(
            SlottedPage::from_bytes(&page[..]).verify_checksum(),
            "page {pid} was written without a valid checksum — the cache did not stamp it"
        );
    }
}

#[test]
fn a_corrupt_page_is_surfaced_not_returned_as_data() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let cache = keel_cache(disk.clone(), 2);
        for _ in 0..3 {
            let p = cache.new_page().unwrap();
            let mut b = p.write();
            let mut sp = SlottedPage::init(&mut b[..], PageType::Heap);
            sp.insert(b"payload").unwrap();
        }
        cache.checkpoint().unwrap();
    }

    let mut page = vec![0u8; PAGE_SIZE];
    disk.read_at(&mut page, PAGE_SIZE as u64).unwrap();
    page[100] ^= 0xFF;
    disk.write_at(&page, PAGE_SIZE as u64).unwrap();

    let cache = keel_cache(disk.clone(), 2);
    assert!(
        matches!(cache.fetch(1), Err(CacheError::Corrupt(1))),
        "a damaged page was published as data instead of surfacing Corrupt"
    );
    assert!(cache.fetch(0).is_ok(), "an intact page must still load");
    assert!(cache.fetch(2).is_ok(), "an intact page must still load");
}

#[test]
fn the_opaque_format_leaves_bytes_untouched() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    const MARK: u8 = 0xAB;
    {
        let cache = PageCache::open(disk.clone(), 2);
        let p = cache.new_page().unwrap();
        p.write().iter_mut().for_each(|x| *x = MARK);
        drop(p);
        cache.checkpoint().unwrap();
    }
    let mut page = vec![0u8; PAGE_SIZE];
    disk.read_at(&mut page, 0).unwrap();
    assert!(
        page.iter().all(|&x| x == MARK),
        "the opaque format must not rewrite any byte of the page"
    );
}
