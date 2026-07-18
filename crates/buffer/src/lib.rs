//! The buffer pool — frames, a page table, CLOCK replacement, and RAII guards.
//!
//! Access is only ever through a [`ReadGuard`] or [`WriteGuard`], which pin the
//! frame on creation and unpin on drop (§2.3). A pinned frame can't be evicted,
//! so a live guard's page can't move under it; a `RefCell` per frame turns any
//! aliasing mistake (two writers, or a writer beside a reader) into a loud
//! runtime panic rather than silent corruption. That is the same discipline
//! latches will enforce in phase 7 — brought forward cheaply for the
//! single-threaded stage (D3).
//!
//! The one invariant this module exists to guard is **WAL-before-data**: a dirty
//! page may not be written to disk until the log is durable through that page's
//! LSN. It lives in exactly one place ([`BufferPool::flush_frame`]) as one
//! check, behind the [`WalSync`] seam. With [`NoWal`] (phase 1, before the log
//! exists) the check is vacuously true; rung-1 WAL swaps in a real implementation
//! at P3 without touching this file.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use keel_page::{PageBuf, PageType, SlottedPage, PAGE_SIZE};
use keel_vfs::BlockFile;

/// A page number within a data file. `RID = (PageId, SlotId)`.
pub type PageId = u32;

#[inline]
fn page_offset(pid: PageId) -> u64 {
    pid as u64 * PAGE_SIZE as u64
}

/// The seam through which the buffer pool asks "is the log durable enough to
/// write this page?" (§2.3). The whole recovery story rests on the honest answer
/// to that question.
pub trait WalSync {
    /// The highest LSN guaranteed durable on the log.
    fn flushed_lsn(&self) -> u64;
    /// Force the log durable through at least `lsn`.
    fn flush_until(&self, lsn: u64) -> io::Result<()>;
}

/// The phase-1 stand-in: there is no log yet, so every page is trivially safe to
/// write. Replaced by real WAL at P3.
pub struct NoWal;
impl WalSync for NoWal {
    fn flushed_lsn(&self) -> u64 {
        u64::MAX
    }
    fn flush_until(&self, _lsn: u64) -> io::Result<()> {
        Ok(())
    }
}

/// Errors from the buffer pool.
#[derive(Debug)]
pub enum BufferError {
    Io(io::Error),
    /// A page failed checksum verification on load — a torn or rotted page.
    Corrupt(PageId),
    /// Every frame is pinned; no victim could be found.
    Exhausted,
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::Io(e) => write!(f, "io: {e}"),
            BufferError::Corrupt(p) => write!(f, "page {p} failed checksum"),
            BufferError::Exhausted => write!(f, "buffer pool exhausted (all frames pinned)"),
        }
    }
}
impl std::error::Error for BufferError {}
impl From<io::Error> for BufferError {
    fn from(e: io::Error) -> Self {
        BufferError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, BufferError>;

/// Buffer-pool counters — every stat before every explanation (house law).
#[derive(Clone, Copy, Debug, Default)]
pub struct BufStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub flushes: u64,
    pub reads: u64,
    pub writes: u64,
    pub new_pages: u64,
}

impl BufStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

struct Frame {
    page: RefCell<PageBuf>,
    page_id: Cell<Option<PageId>>,
    pin: Cell<u32>,
    dirty: Cell<bool>,
    ref_bit: Cell<bool>,
}

impl Frame {
    fn empty() -> Self {
        Self {
            page: RefCell::new(PageBuf::zeroed()),
            page_id: Cell::new(None),
            pin: Cell::new(0),
            dirty: Cell::new(false),
            ref_bit: Cell::new(false),
        }
    }
}

/// A fixed-size pool of frames caching pages of one data file.
pub struct BufferPool {
    frames: Box<[Frame]>,
    table: RefCell<HashMap<PageId, usize>>,
    clock: Cell<usize>,
    file: Arc<dyn BlockFile>,
    wal: Box<dyn WalSync + Send>,
    next_page: Cell<PageId>,
    stats: Cell<BufStats>,
    /// Dirty Page Table (D5 rung 3): `page -> recLSN`, the LSN that first dirtied
    /// the page since it was last written. Populated by `note_dirty`, cleared on
    /// flush. Snapshotted at a fuzzy checkpoint to bound recovery's redo start.
    dpt: RefCell<HashMap<PageId, u64>>,
    /// Steal policy (D5 rung 1). When `false` (no-steal), a dirty page is never
    /// written to disk during eviction — only via an explicit flush at commit or
    /// checkpoint — so uncommitted changes can't reach disk. Defaults to `true`
    /// (steal), which is what the heap/B-tree layers assume.
    steal: Cell<bool>,
}

impl BufferPool {
    /// Open a pool over `file` with `n_frames` frames and a WAL seam. The next
    /// page id is inferred from the file's current length.
    pub fn open(
        file: Arc<dyn BlockFile>,
        n_frames: usize,
        wal: Box<dyn WalSync + Send>,
    ) -> Result<Self> {
        assert!(n_frames > 0, "buffer pool needs at least one frame");
        let size = file.size()?;
        let next_page = (size / PAGE_SIZE as u64) as PageId;
        let frames = (0..n_frames)
            .map(|_| Frame::empty())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            frames,
            table: RefCell::new(HashMap::new()),
            clock: Cell::new(0),
            file,
            wal,
            next_page: Cell::new(next_page),
            stats: Cell::new(BufStats::default()),
            dpt: RefCell::new(HashMap::new()),
            steal: Cell::new(true),
        })
    }

    /// Record that `pid` became dirty at `reclsn` (its recLSN), if not already
    /// tracked. The WAL layer calls this from `log_and_apply`.
    pub fn note_dirty(&self, pid: PageId, reclsn: u64) {
        self.dpt.borrow_mut().entry(pid).or_insert(reclsn);
    }

    /// A snapshot of the Dirty Page Table, `(page, recLSN)`, for a checkpoint.
    pub fn dpt_snapshot(&self) -> Vec<(PageId, u64)> {
        let mut v: Vec<(PageId, u64)> = self.dpt.borrow().iter().map(|(&p, &r)| (p, r)).collect();
        v.sort_unstable();
        v
    }

    /// Convenience: open with no WAL (phase 1).
    pub fn open_default(file: Arc<dyn BlockFile>, n_frames: usize) -> Result<Self> {
        Self::open(file, n_frames, Box::new(NoWal))
    }

    /// Switch to the no-steal policy (D5 rung 1): dirty pages are never written
    /// during eviction. Set once at construction of a transactional store.
    pub fn set_no_steal(&self) {
        self.steal.set(false);
    }

    pub fn stats(&self) -> BufStats {
        self.stats.get()
    }

    pub fn page_count(&self) -> PageId {
        self.next_page.get()
    }

    fn bump<F: FnOnce(&mut BufStats)>(&self, f: F) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }

    fn resident(&self, pid: PageId) -> Option<usize> {
        self.table.borrow().get(&pid).copied()
    }

    /// CLOCK: advance the hand, giving second chances via the reference bit,
    /// until an unpinned frame is found. `Exhausted` if all frames are pinned.
    fn choose_victim(&self) -> Result<usize> {
        let n = self.frames.len();
        let mut scanned = 0usize;
        loop {
            let h = self.clock.get();
            self.clock.set((h + 1) % n);
            let f = &self.frames[h];
            let evictable = f.pin.get() == 0 && (self.steal.get() || !f.dirty.get());
            if evictable {
                if f.ref_bit.get() {
                    f.ref_bit.set(false);
                } else {
                    return Ok(h);
                }
            }
            scanned += 1;
            if scanned > 2 * n {
                return Err(BufferError::Exhausted);
            }
        }
    }

    /// Write a dirty frame to disk, enforcing WAL-before-data. Computes the
    /// checksum on a scratch copy so it never needs a mutable borrow of the
    /// frame — flushing a page that a reader has pinned is therefore safe.
    fn flush_frame(&self, idx: usize) -> Result<()> {
        let f = &self.frames[idx];
        if !f.dirty.get() {
            return Ok(());
        }
        let pid = f.page_id.get().expect("dirty frame must have a page id");

        let mut scratch = [0u8; PAGE_SIZE];
        {
            let page = f.page.borrow();
            scratch.copy_from_slice(page.as_ref());
        }
        let lsn = {
            let sp = SlottedPage::from_bytes(&scratch[..]);
            sp.page_lsn()
        };

        if lsn > self.wal.flushed_lsn() {
            self.wal.flush_until(lsn)?;
        }
        debug_assert!(
            lsn <= self.wal.flushed_lsn(),
            "WAL-before-data violated: page {pid} lsn {lsn} > flushed_lsn {}",
            self.wal.flushed_lsn()
        );

        {
            let mut sp = SlottedPage::from_bytes(&mut scratch[..]);
            sp.recompute_checksum();
        }
        self.file.write_at(&scratch, page_offset(pid))?;
        f.dirty.set(false);
        self.dpt.borrow_mut().remove(&pid);
        self.bump(|s| {
            s.flushes += 1;
            s.writes += 1;
        });
        Ok(())
    }

    /// Evict whatever `idx` currently holds (flushing if dirty) and load `pid`
    /// from disk into it, verifying the checksum.
    fn evict_and_load(&self, idx: usize, pid: PageId) -> Result<()> {
        {
            let f = &self.frames[idx];
            if let Some(old) = f.page_id.get() {
                if f.dirty.get() {
                    self.flush_frame(idx)?;
                }
                self.table.borrow_mut().remove(&old);
                self.bump(|s| s.evictions += 1);
            }
        }
        let f = &self.frames[idx];
        {
            let mut page = f.page.borrow_mut();
            self.file.read_at(page.as_mut(), page_offset(pid))?;
        }
        {
            let page = f.page.borrow();
            let sp = SlottedPage::from_bytes(page.as_ref());
            if !sp.verify_checksum() {
                return Err(BufferError::Corrupt(pid));
            }
        }
        f.page_id.set(Some(pid));
        f.dirty.set(false);
        f.ref_bit.set(true);
        self.table.borrow_mut().insert(pid, idx);
        self.bump(|s| {
            s.misses += 1;
            s.reads += 1;
        });
        Ok(())
    }

    /// Resolve `pid` to a resident frame, loading it if necessary. Does not pin.
    fn frame_for(&self, pid: PageId) -> Result<usize> {
        if let Some(idx) = self.resident(pid) {
            self.bump(|s| s.hits += 1);
            self.frames[idx].ref_bit.set(true);
            return Ok(idx);
        }
        let idx = self.choose_victim()?;
        self.evict_and_load(idx, pid)?;
        Ok(idx)
    }

    /// Pin `pid` for shared reading.
    pub fn fetch_read(&self, pid: PageId) -> Result<ReadGuard<'_>> {
        let idx = self.frame_for(pid)?;
        let f = &self.frames[idx];
        f.pin.set(f.pin.get() + 1);
        f.ref_bit.set(true);
        Ok(ReadGuard {
            pool: self,
            frame: idx,
            guard: f.page.borrow(),
        })
    }

    /// Pin `pid` for exclusive writing.
    pub fn fetch_write(&self, pid: PageId) -> Result<WriteGuard<'_>> {
        let idx = self.frame_for(pid)?;
        let f = &self.frames[idx];
        f.pin.set(f.pin.get() + 1);
        f.ref_bit.set(true);
        Ok(WriteGuard {
            pool: self,
            frame: idx,
            guard: f.page.borrow_mut(),
        })
    }

    /// Reserve the next page id and an evicted frame for it (shared by the
    /// `new_page*` variants). The frame is emptied but not yet initialized.
    fn begin_new_page(&self) -> Result<(PageId, usize)> {
        let pid = self.next_page.get();
        self.next_page.set(pid + 1);
        let idx = self.choose_victim()?;
        let f = &self.frames[idx];
        if let Some(old) = f.page_id.get() {
            if f.dirty.get() {
                self.flush_frame(idx)?;
            }
            self.table.borrow_mut().remove(&old);
            self.bump(|s| s.evictions += 1);
        }
        Ok((pid, idx))
    }

    /// Bind a freshly-initialized frame to `pid`, pin it, and hand back a guard.
    fn finish_new_page(&self, pid: PageId, idx: usize) -> WriteGuard<'_> {
        let f = &self.frames[idx];
        f.page_id.set(Some(pid));
        f.dirty.set(true);
        f.ref_bit.set(true);
        f.pin.set(f.pin.get() + 1);
        self.table.borrow_mut().insert(pid, idx);
        self.bump(|s| s.new_pages += 1);
        WriteGuard {
            pool: self,
            frame: idx,
            guard: f.page.borrow_mut(),
        }
    }

    /// Allocate a fresh **slotted** page at the end of the file, returned pinned
    /// for writing. The page is dirty from birth so it will be persisted on
    /// flush/eviction. (Heap pages, and any other slotted body.)
    pub fn new_page(&self, page_type: PageType) -> Result<(PageId, WriteGuard<'_>)> {
        let (pid, idx) = self.begin_new_page()?;
        {
            let mut page = self.frames[idx].page.borrow_mut();
            let _ = SlottedPage::init(page.as_mut(), page_type);
        }
        Ok((pid, self.finish_new_page(pid, idx)))
    }

    /// Allocate a fresh page with only the shared header stamped (via
    /// `keel_page::raw`), leaving the body zeroed for a non-slotted layout such
    /// as a B-tree node.
    pub fn new_page_raw(&self, page_type: PageType) -> Result<(PageId, WriteGuard<'_>)> {
        let (pid, idx) = self.begin_new_page()?;
        {
            let mut page = self.frames[idx].page.borrow_mut();
            keel_page::raw::init_header(page.as_mut(), page_type);
        }
        Ok((pid, self.finish_new_page(pid, idx)))
    }

    /// Flush a single page if resident and dirty.
    pub fn flush_page(&self, pid: PageId) -> Result<()> {
        if let Some(idx) = self.resident(pid) {
            self.flush_frame(idx)?;
        }
        Ok(())
    }

    /// Flush every dirty frame. Callers must hold no live `WriteGuard` (that
    /// would be an aliasing borrow); read guards are fine.
    pub fn flush_all(&self) -> Result<()> {
        for idx in 0..self.frames.len() {
            self.flush_frame(idx)?;
        }
        Ok(())
    }

    /// Flush everything and fsync the underlying file — a crude checkpoint until
    /// the real fuzzy checkpointer arrives at P3.
    pub fn checkpoint(&self) -> Result<()> {
        self.flush_all()?;
        self.file.sync()?;
        Ok(())
    }

    /// fsync the underlying file (commit-time force, after flushing the txn's
    /// pages).
    pub fn sync(&self) -> Result<()> {
        self.file.sync()?;
        Ok(())
    }

    /// Drop a resident page from the pool **without** flushing it — the abort
    /// path (D5 rung 1). Under no-steal the page's uncommitted changes were never
    /// written, so discarding the frame reverts to the durable version, which the
    /// next fetch reloads. Must not be called while a guard on `pid` is live.
    pub fn invalidate(&self, pid: PageId) {
        if let Some(idx) = self.table.borrow_mut().remove(&pid) {
            let f = &self.frames[idx];
            f.page_id.set(None);
            f.dirty.set(false);
            f.pin.set(0);
            f.ref_bit.set(false);
        }
        self.dpt.borrow_mut().remove(&pid);
    }

    /// Fetch a page for redo, reconstructing it if the on-disk copy is missing or
    /// fails its checksum (a torn/dropped page). Never errors on a bad page — the
    /// log is the source of truth and redo rebuilds it. Used only by recovery.
    pub fn fetch_write_for_redo(&self, pid: PageId) -> Result<WriteGuard<'_>> {
        if let Some(idx) = self.resident(pid) {
            let f = &self.frames[idx];
            f.pin.set(f.pin.get() + 1);
            f.ref_bit.set(true);
            return Ok(WriteGuard {
                pool: self,
                frame: idx,
                guard: f.page.borrow_mut(),
            });
        }
        let idx = self.choose_victim()?;
        {
            let f = &self.frames[idx];
            if let Some(old) = f.page_id.get() {
                if f.dirty.get() {
                    self.flush_frame(idx)?;
                }
                self.table.borrow_mut().remove(&old);
                self.bump(|s| s.evictions += 1);
            }
        }
        let f = &self.frames[idx];
        {
            let mut page = f.page.borrow_mut();
            let readable = pid < self.next_page.get()
                && self.file.read_at(page.as_mut(), page_offset(pid)).is_ok()
                && SlottedPage::from_bytes(page.as_ref()).verify_checksum();
            if !readable {
                page.as_mut().iter_mut().for_each(|b| *b = 0);
            }
        }
        f.page_id.set(Some(pid));
        f.dirty.set(true);
        f.ref_bit.set(true);
        f.pin.set(f.pin.get() + 1);
        if pid >= self.next_page.get() {
            self.next_page.set(pid + 1);
        }
        self.table.borrow_mut().insert(pid, idx);
        Ok(WriteGuard {
            pool: self,
            frame: idx,
            guard: f.page.borrow_mut(),
        })
    }
}

/// A shared, pinned view of a page. Unpins on drop.
pub struct ReadGuard<'a> {
    pool: &'a BufferPool,
    frame: usize,
    guard: std::cell::Ref<'a, PageBuf>,
}

impl<'a> ReadGuard<'a> {
    pub fn page_id(&self) -> PageId {
        self.pool.frames[self.frame].page_id.get().unwrap()
    }
    /// A read-only slotted-page view over this frame.
    pub fn page(&self) -> SlottedPage<&[u8]> {
        SlottedPage::from_bytes(self.guard.as_ref())
    }
    pub fn bytes(&self) -> &[u8] {
        self.guard.as_ref()
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let f = &self.pool.frames[self.frame];
        f.pin.set(f.pin.get() - 1);
    }
}

/// An exclusive, pinned view of a page. Marks the frame dirty and unpins on drop.
pub struct WriteGuard<'a> {
    pool: &'a BufferPool,
    frame: usize,
    guard: std::cell::RefMut<'a, PageBuf>,
}

impl<'a> WriteGuard<'a> {
    pub fn page_id(&self) -> PageId {
        self.pool.frames[self.frame].page_id.get().unwrap()
    }
    /// A read-only view (does not consume the exclusive borrow).
    pub fn page(&self) -> SlottedPage<&[u8]> {
        SlottedPage::from_bytes(self.guard.as_ref())
    }
    /// The mutable slotted-page view — the only way to change page contents.
    pub fn page_mut(&mut self) -> SlottedPage<&mut [u8]> {
        SlottedPage::from_bytes(self.guard.as_mut())
    }
    /// Stamp the page LSN (called by `log_and_apply` once the WAL exists).
    pub fn set_page_lsn(&mut self, lsn: u64) {
        self.page_mut().set_page_lsn(lsn);
    }
    /// Raw page bytes — for non-slotted bodies (B-tree nodes) that manage their
    /// own layout via `keel_page::raw`.
    pub fn bytes(&self) -> &[u8] {
        self.guard.as_ref()
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.guard.as_mut()
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        let f = &self.pool.frames[self.frame];
        f.dirty.set(true);
        f.pin.set(f.pin.get() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_faultfs::{FaultConfig, FaultDisk};
    use keel_rng::Rng;
    use keel_vfs::MemDisk;

    fn pool(n_frames: usize) -> (Arc<MemDisk>, BufferPool) {
        let disk = Arc::new(MemDisk::new());
        let bp = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, n_frames).unwrap();
        (disk, bp)
    }

    #[test]
    fn new_read_write_roundtrip() {
        let (_disk, bp) = pool(8);
        let pid = {
            let (pid, mut g) = bp.new_page(PageType::Heap).unwrap();
            g.page_mut().insert(b"hello").unwrap();
            pid
        };
        {
            let g = bp.fetch_read(pid).unwrap();
            assert_eq!(g.page().get(0), Some(&b"hello"[..]));
        }
    }

    #[test]
    fn survives_eviction_and_reload() {
        let (_disk, bp) = pool(2);
        let mut pids = Vec::new();
        for i in 0..20 {
            let (pid, mut g) = bp.new_page(PageType::Heap).unwrap();
            g.page_mut().insert(format!("page-{i}").as_bytes()).unwrap();
            pids.push(pid);
        }
        for (i, &pid) in pids.iter().enumerate() {
            let g = bp.fetch_read(pid).unwrap();
            assert_eq!(g.page().get(0), Some(format!("page-{i}").as_bytes()));
        }
        assert!(
            bp.stats().evictions > 0,
            "the workload should have forced evictions"
        );
    }

    #[test]
    fn checksum_verified_on_load() {
        let (_disk, bp) = pool(1);
        let pid = {
            let (pid, mut g) = bp.new_page(PageType::Heap).unwrap();
            g.page_mut().insert(b"data").unwrap();
            pid
        };
        bp.checkpoint().unwrap();
        let (_p2, _g2) = bp.new_page(PageType::Heap).unwrap();
        drop(_g2);
        let g = bp.fetch_read(pid).unwrap();
        assert!(g.page().verify_checksum());
        assert_eq!(g.page().get(0), Some(&b"data"[..]));
    }

    #[test]
    fn corrupt_page_detected_on_load() {
        let disk = Arc::new(MemDisk::new());
        {
            let bp = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, 4).unwrap();
            let (_pid, mut g) = bp.new_page(PageType::Heap).unwrap();
            g.page_mut().insert(b"important").unwrap();
            drop(g);
            bp.checkpoint().unwrap();
        }
        let mut image = disk.snapshot();
        image[PAGE_SIZE - 1] ^= 0xFF;
        disk.install(image);
        let bp2 = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, 4).unwrap();
        let res = bp2.fetch_read(0);
        match &res {
            Err(BufferError::Corrupt(0)) => {}
            Err(e) => panic!("expected Corrupt(0), got error {e}"),
            Ok(_) => panic!("expected Corrupt(0), got a live guard"),
        }
    }

    #[test]
    fn exhaustion_when_all_pinned() {
        let (_disk, bp) = pool(2);
        let (_p0, g0) = bp.new_page(PageType::Heap).unwrap();
        let (_p1, g1) = bp.new_page(PageType::Heap).unwrap();
        match bp.new_page(PageType::Heap) {
            Err(BufferError::Exhausted) => {}
            other => panic!("expected Exhausted, got {:?}", other.map(|(p, _)| p)),
        }
        drop(g0);
        drop(g1);
    }

    #[test]
    fn persists_across_reopen_over_fault_disk() {
        let disk = FaultDisk::new(FaultConfig::benign(), 1);
        let file: Arc<dyn BlockFile> = Arc::new(disk.handle());
        let pid;
        {
            let bp = BufferPool::open_default(file.clone(), 4).unwrap();
            let (p, mut g) = bp.new_page(PageType::Heap).unwrap();
            g.page_mut().insert(b"durable-across-reopen").unwrap();
            drop(g);
            bp.checkpoint().unwrap();
            pid = p;
        }
        let bp2 = BufferPool::open_default(file.clone(), 4).unwrap();
        let g = bp2.fetch_read(pid).unwrap();
        assert_eq!(g.page().get(0), Some(&b"durable-across-reopen"[..]));
    }

    #[test]
    fn stress_random_access_small_pool() {
        let (_disk, bp) = pool(4);
        let mut rng = Rng::seed(123);
        let mut expected: Vec<Vec<u8>> = Vec::new();
        let mut pids: Vec<PageId> = Vec::new();
        for i in 0..40 {
            let (pid, mut g) = bp.new_page(PageType::Heap).unwrap();
            let payload = format!("init-{i}").into_bytes();
            g.page_mut().insert(&payload).unwrap();
            pids.push(pid);
            expected.push(payload);
        }
        for _ in 0..5000 {
            let k = rng.below(pids.len() as u64) as usize;
            let pid = pids[k];
            if rng.chance(0.4) {
                let mut g = bp.fetch_write(pid).unwrap();
                let payload = format!("v{}", rng.next_u32()).into_bytes();
                g.page_mut().set(0, &payload).unwrap();
                expected[k] = payload;
            } else {
                let g = bp.fetch_read(pid).unwrap();
                assert_eq!(g.page().get(0), Some(expected[k].as_slice()));
            }
        }
    }
}
