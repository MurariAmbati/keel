//! `pager` — the seam that lets `heap` and `btree` run over **either** the
//! single-threaded [`keel_buffer::BufferPool`] or the concurrent
//! [`keel_cbuffer::PageCache`] (engine-swap slice 3).
//!
//! ## Why closure-scoped access rather than a returned guard
//! The two pools hand out page bytes through incompatible shapes. `BufferPool`
//! returns one self-contained `WriteGuard<'a>` holding a `RefMut`. `PageCache`
//! returns a `PageRef`, and the lock guard **borrows that `PageRef`** — so a trait
//! method returning a single owning guard would be self-referential, which is not
//! expressible safely (and `unsafe` is quarantined to `page`, D4).
//!
//! So the trait hands the bytes to a closure instead: the pool keeps ownership of
//! whatever guards it needs for exactly the duration of the call. That is uniform,
//! safe, and costs nothing beyond a closure call.
//!
//! ## Scope
//! Deliberately only the operations `heap` and `btree` actually use (see the
//! migration map): page count, read, write, allocate slotted, allocate raw, and
//! checkpoint. The recovery- and policy-level surface (`no_steal`, `invalidate`,
//! DPT, `fetch_for_redo`) stays pool-specific for now — only `wal` needs it, and it
//! is the last crate to migrate.

use keel_page::{raw, PageType, SlottedPage};

pub type PageId = u32;

/// What can go wrong behind the seam, uniform across pools.
#[derive(Debug)]
pub enum PagerError {
    Io(std::io::Error),
    /// A page failed its checksum on load — surfaced, never returned as data.
    Corrupt(PageId),
    /// No frame could be freed (every one pinned, or dirty under no-steal).
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

/// The page-access surface `heap` and `btree` need, satisfied by both pools.
pub trait Pager {
    /// One past the highest page the pool will hand out.
    fn page_count(&self) -> PageId;

    /// Read page `pid`, handing its bytes to `f` for the duration of the call.
    fn with_page<R>(&self, pid: PageId, f: impl FnOnce(&[u8]) -> R) -> Result<R, PagerError>;

    /// Write page `pid`, handing its bytes to `f`. The page is marked dirty.
    fn with_page_mut<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError>;

    /// Allocate a fresh page initialised as a **slotted** page of `pt`.
    fn alloc_slotted(&self, pt: PageType) -> Result<PageId, PagerError>;

    /// Allocate a fresh page initialised with only a **raw** header (B-tree nodes,
    /// which are not slotted).
    fn alloc_raw(&self, pt: PageType) -> Result<PageId, PagerError>;

    /// Flush everything dirty and make it durable.
    fn checkpoint(&self) -> Result<(), PagerError>;
}

/// The extra surface a **recovery/transaction** layer needs on top of [`Pager`]:
/// steal policy, abort-time invalidation, the Dirty Page Table, a fault-tolerant
/// redo fetch, and single-page durability.
///
/// This is split from `Pager` deliberately. `heap` and `btree` need none of it, so
/// keeping it separate means they are generic over the *small* surface and cannot
/// accidentally reach for a recovery primitive. Only `wal` (and `db`, for its
/// no-steal logged mode) depends on this one.
///
/// `with_page_for_redo` is closure-scoped for the same reason as `Pager`'s accessors:
/// `PageCache`'s lock guard borrows its `PageRef`, so a returned guard would be
/// self-referential.
pub trait RecoveryPager: Pager {
    /// Switch to no-steal: a dirty page is never written during eviction.
    fn set_no_steal(&self);

    /// Drop a resident page *without* flushing it — the abort path.
    fn invalidate(&self, pid: PageId);

    /// Record that `pid` became dirty at `reclsn`; the oldest wins.
    fn note_dirty(&self, pid: PageId, reclsn: u64);

    /// A sorted `(page, recLSN)` snapshot for a checkpoint record.
    fn dpt_snapshot(&self) -> Vec<(PageId, u64)>;

    /// Fetch a page for redo, rebuilding it blank if it is missing or fails its
    /// integrity check — during recovery the log is the source of truth.
    fn with_page_for_redo<R>(
        &self,
        pid: PageId,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, PagerError>;

    /// Flush one page if resident and dirty (the commit-force path).
    fn flush_page(&self, pid: PageId) -> Result<(), PagerError>;

    /// Make everything already written durable, without flushing the cache.
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
