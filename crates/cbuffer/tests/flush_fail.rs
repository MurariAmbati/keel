use keel_cbuffer::PageCache;
use keel_vfs::{BlockFile, MemDisk};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct FailNextWrite {
    inner: MemDisk,
    armed: AtomicBool,
}
impl FailNextWrite {
    fn new() -> Self {
        Self {
            inner: MemDisk::new(),
            armed: AtomicBool::new(false),
        }
    }
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}
impl BlockFile for FailNextWrite {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.armed.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected flush failure"));
        }
        self.inner.write_at(buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
}

#[test]
fn flush_failure_keeps_page_dirty_so_checkpoint_still_persists_it() {
    let disk = Arc::new(FailNextWrite::new());
    const MARK: u64 = 0xA11CE5;

    {
        let cache = Arc::new(PageCache::open(disk.clone(), 1));
        let p0 = cache.new_page().unwrap();
        assert_eq!(p0.pid(), 0);
        p0.write()[..8].copy_from_slice(&MARK.to_le_bytes());
        drop(p0);

        disk.arm();
        let r = cache.new_page();
        assert!(
            r.is_err(),
            "the injected flush failure should surface as an error"
        );

        assert_eq!(
            cache.page_count(),
            1,
            "a failed new_page left a hole below page_count()"
        );
        for pid in 0..cache.page_count() {
            cache
                .fetch(pid)
                .expect("every page below page_count must be readable");
        }

        cache.checkpoint().unwrap();
    }

    let cache2 = PageCache::open(disk.clone() as Arc<dyn BlockFile>, 4);
    assert!(cache2.page_count() >= 1, "page 0 was never written to disk");
    let p0 = cache2.fetch(0).unwrap();
    let got = u64::from_le_bytes(p0.read()[..8].try_into().unwrap());
    assert_eq!(
        got, MARK,
        "committed record lost: a failed flush marked the page clean and checkpoint skipped it"
    );
}
