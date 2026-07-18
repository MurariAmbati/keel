use keel_page::{raw, PageType, SlottedPage};

pub type PageId = u32;

#[derive(Debug)]
pub enum PagerError {
    Io(std::io::Error),
    Corrupt(PageId),
    Exhausted,
}

impl std::fmt::Display for PagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PagerError::Io(e) => write!(f, "io: {e}"),
            PagerError::Corrupt(p) => write!(f, "page {p} failed its checksum"),
            PagerError::Exhausted => write!(f, "pager exhausted (no evictable frame)"),
        }
    }
}
impl std::error::Error for PagerError {}

impl From<keel_buffer::BufferError> for PagerError {
    fn from(e: keel_buffer::BufferError) -> Self {
        match e {
            keel_buffer::BufferError::Io(e) => PagerError::Io(e),
            keel_buffer::BufferError::Corrupt(p) => PagerError::Corrupt(p),
            keel_buffer::BufferError::Exhausted => PagerError::Exhausted,
        }
    }
}

impl From<std::io::Error> for PagerError {
    fn from(e: std::io::Error) -> Self {
        PagerError::Io(e)
    }
}

impl From<keel_cbuffer::CacheError> for PagerError {
    fn from(e: keel_cbuffer::CacheError) -> Self {
        match e {
            keel_cbuffer::CacheError::Io(e) => PagerError::Io(e),
            keel_cbuffer::CacheError::Corrupt(p) => PagerError::Corrupt(p),
            keel_cbuffer::CacheError::Exhausted => PagerError::Exhausted,
        }
    }
}

pub trait Pager {
    fn page_count(&self) -> PageId;

    fn with_page<R>(&self, pid: PageId, f: impl FnOnce(&[u8]) -> R) -> Result<R, PagerError>;

    fn with_page_mut<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError>;

    fn alloc_slotted(&self, pt: PageType) -> Result<PageId, PagerError>;

    fn alloc_raw(&self, pt: PageType) -> Result<PageId, PagerError>;

    fn checkpoint(&self) -> Result<(), PagerError>;
}

pub trait RecoveryPager: Pager {
    fn set_no_steal(&self);

    fn invalidate(&self, pid: PageId);

    fn note_dirty(&self, pid: PageId, reclsn: u64);

    fn dpt_snapshot(&self) -> Vec<(PageId, u64)>;

    fn with_page_for_redo<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError>;

    fn flush_page(&self, pid: PageId) -> Result<(), PagerError>;

    fn sync(&self) -> Result<(), PagerError>;
}

impl RecoveryPager for keel_buffer::BufferPool {
    fn set_no_steal(&self) {
        keel_buffer::BufferPool::set_no_steal(self)
    }
    fn invalidate(&self, pid: PageId) {
        keel_buffer::BufferPool::invalidate(self, pid)
    }
    fn note_dirty(&self, pid: PageId, reclsn: u64) {
        keel_buffer::BufferPool::note_dirty(self, pid, reclsn)
    }
    fn dpt_snapshot(&self) -> Vec<(PageId, u64)> {
        keel_buffer::BufferPool::dpt_snapshot(self)
    }
    fn with_page_for_redo<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError> {
        let mut g = self.fetch_write_for_redo(pid)?;
        Ok(f(g.bytes_mut()))
    }
    fn flush_page(&self, pid: PageId) -> Result<(), PagerError> {
        keel_buffer::BufferPool::flush_page(self, pid)?;
        Ok(())
    }
    fn sync(&self) -> Result<(), PagerError> {
        keel_buffer::BufferPool::sync(self)?;
        Ok(())
    }
}

impl RecoveryPager for keel_cbuffer::PageCache {
    fn set_no_steal(&self) {
        keel_cbuffer::PageCache::set_no_steal(self)
    }
    fn invalidate(&self, pid: PageId) {
        keel_cbuffer::PageCache::invalidate(self, pid)
    }
    fn note_dirty(&self, pid: PageId, reclsn: u64) {
        keel_cbuffer::PageCache::note_dirty(self, pid, reclsn)
    }
    fn dpt_snapshot(&self) -> Vec<(PageId, u64)> {
        keel_cbuffer::PageCache::dpt_snapshot(self)
    }
    fn with_page_for_redo<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError> {
        let p = self.fetch_for_redo(pid)?;
        let mut b = p.write();
        Ok(f(&mut b))
    }
    fn flush_page(&self, pid: PageId) -> Result<(), PagerError> {
        keel_cbuffer::PageCache::flush_page(self, pid)?;
        Ok(())
    }
    fn sync(&self) -> Result<(), PagerError> {
        keel_cbuffer::PageCache::sync(self)?;
        Ok(())
    }
}

impl Pager for keel_buffer::BufferPool {
    fn page_count(&self) -> PageId {
        keel_buffer::BufferPool::page_count(self)
    }

    fn with_page<R>(&self, pid: PageId, f: impl FnOnce(&[u8]) -> R) -> Result<R, PagerError> {
        let g = self.fetch_read(pid)?;
        Ok(f(g.bytes()))
    }

    fn with_page_mut<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError> {
        let mut g = self.fetch_write(pid)?;
        Ok(f(g.bytes_mut()))
    }

    fn alloc_slotted(&self, pt: PageType) -> Result<PageId, PagerError> {
        let (pid, _g) = keel_buffer::BufferPool::new_page(self, pt)?;
        Ok(pid)
    }

    fn alloc_raw(&self, pt: PageType) -> Result<PageId, PagerError> {
        let (pid, _g) = self.new_page_raw(pt)?;
        Ok(pid)
    }

    fn checkpoint(&self) -> Result<(), PagerError> {
        keel_buffer::BufferPool::checkpoint(self)?;
        Ok(())
    }
}

impl Pager for keel_cbuffer::PageCache {
    fn page_count(&self) -> PageId {
        keel_cbuffer::PageCache::page_count(self)
    }

    fn with_page<R>(&self, pid: PageId, f: impl FnOnce(&[u8]) -> R) -> Result<R, PagerError> {
        let p = self.fetch(pid)?;
        let b = p.read();
        Ok(f(&b))
    }

    fn with_page_mut<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError> {
        let p = self.fetch(pid)?;
        let mut b = p.write();
        Ok(f(&mut b))
    }

    fn alloc_slotted(&self, pt: PageType) -> Result<PageId, PagerError> {
        let p = keel_cbuffer::PageCache::new_page(self)?;
        {
            let mut b = p.write();
            SlottedPage::init(&mut b[..], pt);
        }
        Ok(p.pid())
    }

    fn alloc_raw(&self, pt: PageType) -> Result<PageId, PagerError> {
        let p = keel_cbuffer::PageCache::new_page(self)?;
        {
            let mut b = p.write();
            raw::init_header(&mut b[..], pt);
        }
        Ok(p.pid())
    }

    fn checkpoint(&self) -> Result<(), PagerError> {
        keel_cbuffer::PageCache::checkpoint(self)?;
        Ok(())
    }
}
