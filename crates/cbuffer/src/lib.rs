//! A concurrent page cache over [`vfs::BlockFile`] — the assembly piece the
//! [`latch::ClockPool`] reference deliberately simplified: **page I/O is done
//! outside the directory lock**, and (this slice) **dirty pages are flushed on
//! eviction under WAL-before-data, correctly under concurrency**.
//!
//! `ClockPool` proved the residency/pin/CLOCK handshake but built page contents
//! while holding the pool mutex. A real engine can't: a page read or a dirty-page
//! flush is slow I/O, and holding the one mutex across it serializes the whole
//! pool on the disk. So a miss is split into **reserve → I/O → publish**, with
//! the directory `Mutex` released across the I/O and a per-frame `busy` flag so
//! concurrent accessors of a page mid-transition wait rather than issue a
//! duplicate or read stale bytes.
//!
//! The subtle part is **dirty eviction**. Reusing a frame that holds a dirty
//! page P for a new page Q means (1) flushing P to disk first (WAL-before-data)
//! and (2) reading Q — two I/Os with the lock released. The hazard is a *second*
//! copy of P: if P were removed from the directory before its flush landed, a
//! concurrent `fetch(P)` would miss and read the stale on-disk P. The fix is to
//! keep **both** keys resolvable to the busy victim across the whole transition —
//! `fetch(P)` and `fetch(Q)` both find the frame `busy` and wait — and to swap
//! the directory from P to Q only at publish, after the flush is durable. The
//! reserve (miss-check, victim choice, and both directory inserts) happens under
//! one lock hold, so two threads can never both start loading the same page.
//!
//! WAL-before-data lives in exactly one place: [`PageCache`] calls
//! `wal.flush_until(page_lsn)` before writing a dirty page, so no data page
//! reaches disk ahead of the log record that describes it — the same invariant
//! `buffer::flush_frame` enforces serially, here under the reserve/publish
//! protocol.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use keel_page::PAGE_SIZE;
use keel_vfs::BlockFile;

/// A page number within the backing file.
pub type PageId = u32;

#[inline]
fn page_offset(pid: PageId) -> u64 {
    pid as u64 * PAGE_SIZE as u64
}

/// The seam through which the cache asks "is the log durable enough to write this
/// page?" — the WAL-before-data gate (§2.3), the same contract as
/// `buffer::WalSync`, made `Send + Sync` for concurrent use.
pub trait WalSync: Send + Sync {
    fn flushed_lsn(&self) -> u64;
    fn flush_until(&self, lsn: u64) -> io::Result<()>;
}

/// No log yet: every page is trivially safe to write. Used by the clean-cache
/// constructor and in read-only settings.
pub struct NoWal;
impl WalSync for NoWal {
    fn flushed_lsn(&self) -> u64 {
        u64::MAX
    }
    fn flush_until(&self, _lsn: u64) -> io::Result<()> {
        Ok(())
    }
}

/// How the cache reads a page's LSN out of its bytes, so it can enforce
/// WAL-before-data without knowing the page layout. Returns 0 for the clean/no-WAL
/// path (nothing to order against).
pub type LsnOf = fn(&[u8]) -> u64;

/// Stamp a page's integrity field immediately before it is written to disk.
pub type StampOf = fn(&mut [u8]);

/// Check a page's integrity field immediately after it is read from disk.
pub type VerifyOf = fn(&[u8]) -> bool;

fn no_stamp(_: &mut [u8]) {}
fn always_valid(_: &[u8]) -> bool {
    true
}

/// How the cache reads, stamps, and checks a page's format-level fields.
///
/// Centralising this is the point: `keel-buffer` stamps the checksum in exactly one
/// place (its flush path) and verifies it in exactly one place (its load path), so no
/// caller can forget. `cbuffer` originally had neither, which is *why* KEEL-0004
/// happened — `cheap` had to recompute the checksum by hand at every mutating site,
/// and the one path that forgot persisted a stale checksum.
///
/// The invariant this establishes is **"a page's checksum is correct on disk"**, not
/// in memory: a dirty cached page legitimately carries a stale checksum until it is
/// flushed. That is the same contract `keel-buffer` provides.
#[derive(Clone, Copy)]
pub struct PageFormat {
    pub lsn_of: LsnOf,
    pub stamp: StampOf,
    pub verify: VerifyOf,
}

impl PageFormat {
    /// Treat pages as opaque bytes: no LSN ordering, no stamping, no checking. This
    /// is right for a caller that owns its own integrity scheme (as `ckv` does, with
    /// a CRC at its own offset) or holds non-page data.
    pub fn opaque() -> Self {
        Self {
            lsn_of: no_lsn,
            stamp: no_stamp,
            verify: always_valid,
        }
    }

    /// The `keel_page` layout: page-LSN for WAL-before-data, and a CRC stamped on
    /// every write and verified on every read. Works for slotted *and* raw pages,
    /// since the checksum covers `[OFF_PAGE_LSN, PAGE_SIZE)` either way.
    pub fn keel_page() -> Self {
        Self {
            lsn_of: keel_page::raw::page_lsn,
            stamp: keel_page::raw::stamp_checksum,
            verify: keel_page::raw::verify_checksum,
        }
    }
}

/// Which read discipline a fetch uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fetch {
    /// Normal operation: a read error or a failed integrity check is an error.
    Normal,
    /// Recovery redo: a missing or damaged page is rebuilt blank instead.
    Redo,
}

/// How to undo a failed reservation — see [`PageCache::abort_reservation`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Abort {
    /// The flush failed: keep the old page resident and dirty so it is retried.
    KeepOldDirty,
    /// The read failed: the buffer may be half-written, so empty the frame.
    Discard,
}

fn no_lsn(_: &[u8]) -> u64 {
    0
}

/// Errors from the cache.
#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    /// Every frame is pinned; no victim could be found. An honest signal, never a
    /// silent stall (house law: no silent caps).
    Exhausted,
    /// A page failed its format's integrity check on load. Corruption is surfaced,
    /// never returned as data (D4 / the crash-campaign contract).
    Corrupt(PageId),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Io(e) => write!(f, "io: {e}"),
            CacheError::Exhausted => write!(f, "page cache exhausted (all frames pinned)"),
            CacheError::Corrupt(p) => write!(f, "page {p} failed its integrity check"),
        }
    }
}
impl std::error::Error for CacheError {}

/// One cache frame's *metadata*. The page bytes live in a separate `RwLock`
/// (behind an `Arc`) so a thread can read or fill them with the directory mutex
/// released.
struct Frame {
    pid: Option<PageId>,
    pins: u32,
    ref_bit: bool,
    dirty: bool,
    /// Mid-transition: the frame is reserved and doing I/O. While set, both the
    /// old and new page ids resolve here and callers wait.
    busy: bool,
    buf: Arc<RwLock<Vec<u8>>>,
}

struct Dir {
    cap: usize,
    map: HashMap<PageId, usize>,
    frames: Vec<Frame>,
    hand: usize,
    /// The next page id to hand out from `new_page` — one past the highest page
    /// the file currently holds, bumped under this lock so allocations never
    /// collide.
    next_page: PageId,
    /// Steal policy (D5). When false a dirty page is never written during
    /// eviction, so the log is its only durable record — rung 1 no-steal.
    steal: bool,
}

impl Dir {
    /// CLOCK: advance past pinned and busy frames, clearing ref-bits for a second
    /// chance, until a reusable frame is found. `None` if none is evictable.
    fn choose_victim(&mut self) -> Option<usize> {
        let n = self.cap;
        let steal = self.steal;
        let mut scanned = 0usize;
        loop {
            let h = self.hand;
            self.hand = (h + 1) % n;
            let f = &mut self.frames[h];
            if f.pins == 0 && !f.busy && (steal || !f.dirty) {
                if f.ref_bit {
                    f.ref_bit = false;
                } else {
                    return Some(h);
                }
            }
            scanned += 1;
            if scanned > 2 * n {
                return None;
            }
        }
    }
}

/// A fixed-capacity concurrent page cache over one backing file.
pub struct PageCache {
    file: Arc<dyn BlockFile>,
    wal: Arc<dyn WalSync>,
    fmt: PageFormat,
    dir: Mutex<Dir>,
    /// Dirty Page Table: page -> recLSN, the oldest log record that dirtied it.
    /// ARIES analysis uses it to bound where redo must start.
    dpt: Mutex<HashMap<PageId, u64>>,
    ready: Condvar,
    hits: AtomicU64,
    loads: AtomicU64,
    evictions: AtomicU64,
    flushes: AtomicU64,
    allocations: AtomicU64,
}

/// A pinned handle to a resident page. While it lives the frame cannot be
/// evicted, so the bytes it names stay put. Unpins on drop.
pub struct PageRef<'a> {
    cache: &'a PageCache,
    idx: usize,
    pid: PageId,
    buf: Arc<RwLock<Vec<u8>>>,
}

impl PageCache {
    /// A clean cache with no WAL — pages are read but never dirtied, so eviction
    /// never flushes.
    pub fn open(file: Arc<dyn BlockFile>, cap: usize) -> Self {
        Self::open_formatted(file, cap, Arc::new(NoWal), PageFormat::opaque())
    }

    /// A cache with a WAL seam and a page-LSN reader, so dirty pages are flushed
    /// on eviction under WAL-before-data.
    pub fn open_wal(
        file: Arc<dyn BlockFile>,
        cap: usize,
        wal: Arc<dyn WalSync>,
        lsn_of: LsnOf,
    ) -> Self {
        Self::open_formatted(
            file,
            cap,
            wal,
            PageFormat {
                lsn_of,
                ..PageFormat::opaque()
            },
        )
    }

    /// A cache with a WAL seam and a full [`PageFormat`], so the cache itself stamps
    /// each page's checksum before writing and verifies it after reading — no caller
    /// can forget (see `PageFormat` for why that matters).
    pub fn open_formatted(
        file: Arc<dyn BlockFile>,
        cap: usize,
        wal: Arc<dyn WalSync>,
        fmt: PageFormat,
    ) -> Self {
        assert!(cap > 0, "a cache needs at least one frame");
        let next_page = (file.size().unwrap_or(0) / PAGE_SIZE as u64) as PageId;
        let frames = (0..cap)
            .map(|_| Frame {
                pid: None,
                pins: 0,
                ref_bit: false,
                dirty: false,
                busy: false,
                buf: Arc::new(RwLock::new(vec![0u8; PAGE_SIZE])),
            })
            .collect();
        Self {
            file,
            wal,
            fmt,
            dir: Mutex::new(Dir {
                cap,
                map: HashMap::new(),
                frames,
                hand: 0,
                next_page,
                steal: true,
            }),
            dpt: Mutex::new(HashMap::new()),
            ready: Condvar::new(),
            hits: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            allocations: AtomicU64::new(0),
        }
    }

    /// Pin the frame holding `pid`, reading it from disk (over a CLOCK-chosen
    /// victim, flushing it first if dirty) if not resident. All disk I/O happens
    /// with the directory lock released.
    pub fn fetch(&self, pid: PageId) -> Result<PageRef<'_>, CacheError> {
        self.fetch_mode(pid, Fetch::Normal)
    }

    /// Fetch a page for **redo**, reconstructing it if the on-disk copy is missing
    /// or fails its integrity check (a torn or dropped page). It never errors on a
    /// bad page: during recovery the log is the source of truth and redo rebuilds
    /// the contents, so a blank frame is the correct starting point. It also
    /// extends the allocation watermark past `pid`, since redo may legitimately
    /// touch a page that a truncated file has never held. Recovery only.
    pub fn fetch_for_redo(&self, pid: PageId) -> Result<PageRef<'_>, CacheError> {
        self.fetch_mode(pid, Fetch::Redo)
    }

    fn fetch_mode(&self, pid: PageId, mode: Fetch) -> Result<PageRef<'_>, CacheError> {
        loop {
            let mut d = self.dir.lock().expect("cache poisoned");

            if let Some(&idx) = d.map.get(&pid) {
                if d.frames[idx].busy {
                    drop(self.ready.wait(d).expect("cache poisoned"));
                    continue;
                }
                d.frames[idx].pins += 1;
                d.frames[idx].ref_bit = true;
                let buf = d.frames[idx].buf.clone();
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(PageRef {
                    cache: self,
                    idx,
                    pid,
                    buf,
                });
            }

            let victim = match d.choose_victim() {
                Some(v) => v,
                None => return Err(CacheError::Exhausted),
            };
            let old = d.frames[victim].pid;
            let old_dirty = d.frames[victim].dirty;
            d.frames[victim].busy = true;
            d.frames[victim].pins = 1;
            d.frames[victim].ref_bit = true;
            d.map.insert(pid, victim);
            let buf = d.frames[victim].buf.clone();
            drop(d);

            if old_dirty {
                if let Some(oldpid) = old {
                    if let Err(e) = self.flush_old(&buf, oldpid) {
                        self.abort_reservation(victim, pid, Abort::KeepOldDirty);
                        return Err(CacheError::Io(e));
                    }
                    self.flushes.fetch_add(1, Ordering::Relaxed);
                }
            }

            let io = {
                let mut b = buf.write().expect("page buffer poisoned");
                self.file.read_at(&mut b, page_offset(pid))
            };
            match mode {
                Fetch::Normal => {
                    if let Err(e) = io {
                        self.abort_reservation(victim, pid, Abort::Discard);
                        return Err(CacheError::Io(e));
                    }
                    let b = buf.read().expect("page buffer poisoned");
                    if !(self.fmt.verify)(&b) {
                        drop(b);
                        self.abort_reservation(victim, pid, Abort::Discard);
                        return Err(CacheError::Corrupt(pid));
                    }
                }
                Fetch::Redo => {
                    let mut b = buf.write().expect("page buffer poisoned");
                    if io.is_err() || !(self.fmt.verify)(&b) {
                        b.iter_mut().for_each(|x| *x = 0);
                    }
                }
            }

            let mut d = self.dir.lock().expect("cache poisoned");
            if let Some(oldpid) = old {
                d.map.remove(&oldpid);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
            d.frames[victim].pid = Some(pid);
            d.frames[victim].dirty = matches!(mode, Fetch::Redo);
            d.frames[victim].busy = false;
            if matches!(mode, Fetch::Redo) && d.next_page <= pid {
                d.next_page = pid + 1;
            }
            self.loads.fetch_add(1, Ordering::Relaxed);
            drop(d);
            self.ready.notify_all();
            return Ok(PageRef {
                cache: self,
                idx: victim,
                pid,
                buf,
            });
        }
    }

    /// Allocate a **fresh** page: hand out the next unused page id, place a zeroed
    /// frame for it (dirty, so `checkpoint`/eviction materializes it on disk), and
    /// return it pinned. The id is bumped under the directory lock, so concurrent
    /// allocations never collide. Unlike `fetch`, there is no disk read — the page
    /// does not exist yet — but a dirty victim is still flushed first
    /// (WAL-before-data), exactly as in a miss.
    pub fn new_page(&self) -> Result<PageRef<'_>, CacheError> {
        let mut d = self.dir.lock().expect("cache poisoned");
        let pid = d.next_page;
        let victim = match d.choose_victim() {
            Some(v) => v,
            None => return Err(CacheError::Exhausted),
        };
        d.next_page += 1;
        let old = d.frames[victim].pid;
        let old_dirty = d.frames[victim].dirty;
        d.frames[victim].busy = true;
        d.frames[victim].pins = 1;
        d.frames[victim].ref_bit = true;
        d.map.insert(pid, victim);
        let buf = d.frames[victim].buf.clone();
        drop(d);

        if old_dirty {
            if let Some(oldpid) = old {
                if let Err(e) = self.flush_old(&buf, oldpid) {
                    self.abort_reservation(victim, pid, Abort::KeepOldDirty);
                    {
                        let mut d = self.dir.lock().expect("cache poisoned");
                        if d.next_page == pid + 1 {
                            d.next_page = pid;
                        }
                    }
                    return Err(CacheError::Io(e));
                }
                self.flushes.fetch_add(1, Ordering::Relaxed);
            }
        }

        {
            let mut b = buf.write().expect("page buffer poisoned");
            b.iter_mut().for_each(|x| *x = 0);
        }

        let mut d = self.dir.lock().expect("cache poisoned");
        if let Some(oldpid) = old {
            d.map.remove(&oldpid);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        d.frames[victim].pid = Some(pid);
        d.frames[victim].dirty = true;
        d.frames[victim].busy = false;
        self.allocations.fetch_add(1, Ordering::Relaxed);
        drop(d);
        self.ready.notify_all();
        Ok(PageRef {
            cache: self,
            idx: victim,
            pid,
            buf,
        })
    }

    /// Flush a dirty page to disk under WAL-before-data: force the log durable
    /// through the page's LSN, then write the bytes.
    fn flush_old(&self, buf: &Arc<RwLock<Vec<u8>>>, oldpid: PageId) -> io::Result<()> {
        let b = buf.read().expect("page buffer poisoned");
        let lsn = (self.fmt.lsn_of)(&b);
        if lsn > self.wal.flushed_lsn() {
            self.wal.flush_until(lsn)?;
        }
        debug_assert!(
            lsn <= self.wal.flushed_lsn(),
            "WAL-before-data violated: page {oldpid} lsn {lsn} > flushed {}",
            self.wal.flushed_lsn()
        );
        self.write_stamped(&b, oldpid)
    }

    /// Write a page, stamping its integrity field first. The stamp is applied to a
    /// scratch copy rather than in place: the flush paths deliberately hold only the
    /// buffer *read* guard (so a disk write never blocks concurrent readers, and so
    /// the frame's identity stays pinned across the write — see KEEL-0007), and
    /// stamping in place would require the write guard. The copy costs one page
    /// memcpy against a disk write, which dominates it.
    fn write_stamped(&self, bytes: &[u8], pid: PageId) -> io::Result<()> {
        let mut out = bytes.to_vec();
        (self.fmt.stamp)(&mut out);
        self.file.write_at(&out, page_offset(pid))
    }

    /// Undo a failed reservation so the frame is reusable and no one waits forever
    /// on a page that never loaded.
    ///
    /// The two failure points need *different* undo, and conflating them loses or
    /// corrupts data:
    ///
    /// * [`Abort::KeepOldDirty`] — the **flush** failed. The old page's changes are
    ///   still only in memory, so it must stay resident AND dirty, or a later
    ///   checkpoint would skip it and lose committed data across a power loss.
    /// * [`Abort::Discard`] — the **read** failed. `read_at` may have partially
    ///   filled the buffer before erroring, so the frame's bytes are indeterminate:
    ///   they are neither the old page nor the new one. Restoring the old identity
    ///   would serve those bytes as a cache *hit*. The frame is emptied instead, so
    ///   the next fetch re-reads from disk.
    fn abort_reservation(&self, victim: usize, pid: PageId, how: Abort) {
        let mut d = self.dir.lock().expect("cache poisoned");
        d.frames[victim].pins = 0;
        d.frames[victim].busy = false;
        d.map.remove(&pid);
        match how {
            Abort::KeepOldDirty => {
                d.frames[victim].dirty = true;
            }
            Abort::Discard => {
                if let Some(old) = d.frames[victim].pid {
                    d.map.remove(&old);
                }
                d.frames[victim].pid = None;
                d.frames[victim].dirty = false;
                d.frames[victim].ref_bit = false;
            }
        }
        drop(d);
        self.ready.notify_all();
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }
    pub fn loads(&self) -> u64 {
        self.loads.load(Ordering::Relaxed)
    }
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }
    pub fn allocations(&self) -> u64 {
        self.allocations.load(Ordering::Relaxed)
    }
    /// Switch to the **no-steal** policy (D5 rung 1): a dirty page is never written
    /// during eviction, so the log is its only durable record until commit forces
    /// it. Set once, at construction of a transactional store.
    pub fn set_no_steal(&self) {
        self.dir.lock().expect("cache poisoned").steal = false;
    }

    /// Record that `pid` became dirty at `reclsn` (its recLSN), if not already
    /// tracked — the WAL layer calls this from `log_and_apply`. First writer wins,
    /// because analysis needs the *oldest* record that dirtied the page.
    pub fn note_dirty(&self, pid: PageId, reclsn: u64) {
        self.dpt
            .lock()
            .expect("cache poisoned")
            .entry(pid)
            .or_insert(reclsn);
    }

    /// A sorted snapshot of the Dirty Page Table, `(page, recLSN)`, for a checkpoint.
    pub fn dpt_snapshot(&self) -> Vec<(PageId, u64)> {
        let mut v: Vec<(PageId, u64)> = self
            .dpt
            .lock()
            .expect("cache poisoned")
            .iter()
            .map(|(&p, &r)| (p, r))
            .collect();
        v.sort_unstable();
        v
    }

    /// Drop a resident page **without** flushing it — the abort path (D5 rung 1).
    /// Under no-steal the uncommitted changes never reached disk, so discarding the
    /// frame reverts to the durable version, which the next fetch reloads. Must not
    /// be called while a `PageRef` for `pid` is alive.
    pub fn invalidate(&self, pid: PageId) {
        {
            let mut d = self.dir.lock().expect("cache poisoned");
            if let Some(idx) = d.map.remove(&pid) {
                let f = &mut d.frames[idx];
                f.pid = None;
                f.dirty = false;
                f.pins = 0;
                f.ref_bit = false;
            }
        }
        self.dpt.lock().expect("cache poisoned").remove(&pid);
        self.ready.notify_all();
    }

    /// Flush one page if it is resident and dirty (the commit-force path). Waits out
    /// an in-flight eviction of that frame, exactly as `flush_all` does, so the page
    /// really is on disk when this returns.
    pub fn flush_page(&self, pid: PageId) -> io::Result<()> {
        let target = {
            let d = self.dir.lock().expect("cache poisoned");
            d.map.get(&pid).copied()
        };
        if let Some(idx) = target {
            self.flush_one(idx, pid)?;
        }
        Ok(())
    }

    /// Make everything already written durable, without flushing the cache.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }

    /// The number of pages that have ever been allocated — one past the highest
    /// page id `new_page` will hand out. A reader that owns the whole file (a
    /// heap) scans `0..page_count()`.
    pub fn page_count(&self) -> PageId {
        self.dir.lock().expect("cache poisoned").next_page
    }

    /// Total pins currently held across all frames (for assertions).
    pub fn live_pins(&self) -> u32 {
        self.dir
            .lock()
            .expect("cache poisoned")
            .frames
            .iter()
            .map(|f| f.pins)
            .sum()
    }

    /// Flush every resident dirty page (a checkpoint-style barrier). Serial, and
    /// safe to run *concurrently with inserts*: it holds each page's buffer read
    /// guard across both the disk write and the dirty-flag clear, so it cannot race
    /// a writer (which sets the flag under the buffer *write* guard) into clearing a
    /// page whose newest record it didn't write. The clear is also guarded on the
    /// frame still holding this page and not being mid-eviction, so a concurrent
    /// `fetch` that repurposes the frame is deferred to rather than clobbered.
    pub fn flush_all(&self) -> io::Result<()> {
        let pending: Vec<(usize, PageId)> = {
            let d = self.dir.lock().expect("cache poisoned");
            d.frames
                .iter()
                .enumerate()
                .filter(|(_, f)| f.dirty)
                .filter_map(|(i, f)| f.pid.map(|p| (i, p)))
                .collect()
        };

        for (idx, pid) in pending {
            self.flush_one(idx, pid)?;
        }
        Ok(())
    }

    /// Flush one frame if it still holds `pid` and is still dirty. Shared by
    /// `flush_all` and `flush_page` so the delicate part exists in exactly one copy.
    fn flush_one(&self, idx: usize, pid: PageId) -> io::Result<()> {
        let buf = {
            let mut d = self.dir.lock().expect("cache poisoned");
            while d.frames[idx].busy {
                d = self.ready.wait(d).expect("cache poisoned");
            }
            if d.frames[idx].pid != Some(pid) || !d.frames[idx].dirty {
                return Ok(());
            }
            d.frames[idx].buf.clone()
        };

        let b = buf.read().expect("page buffer poisoned");
        {
            let d = self.dir.lock().expect("cache poisoned");
            let f = &d.frames[idx];
            if f.pid != Some(pid) || !f.dirty {
                return Ok(());
            }
        }
        let lsn = (self.fmt.lsn_of)(&b);
        if lsn > self.wal.flushed_lsn() {
            self.wal.flush_until(lsn)?;
        }
        debug_assert!(
            lsn <= self.wal.flushed_lsn(),
            "WAL-before-data violated: page {pid} lsn {lsn} > flushed {}",
            self.wal.flushed_lsn()
        );
        self.write_stamped(&b, pid)?;
        self.flushes.fetch_add(1, Ordering::Relaxed);
        {
            let mut d = self.dir.lock().expect("cache poisoned");
            let f = &mut d.frames[idx];
            if f.pid == Some(pid) && !f.busy {
                f.dirty = false;
            }
        }
        self.dpt.lock().expect("cache poisoned").remove(&pid);
        drop(b);
        Ok(())
    }

    /// A durability barrier: flush every dirty page (WAL-before-data) and then
    /// `sync` the backing file, so everything written before this call is durable
    /// through a power loss — the "checkpoint is a hard barrier" property, now for
    /// the concurrent cache. Un-flushed cache-resident changes are *not* made
    /// durable, exactly as intended.
    pub fn checkpoint(&self) -> io::Result<()> {
        self.flush_all()?;
        self.file.sync()
    }
}

impl PageRef<'_> {
    /// The page id this handle pins.
    pub fn pid(&self) -> PageId {
        self.pid
    }

    /// Read the pinned page's bytes.
    pub fn read(&self) -> RwLockReadGuard<'_, Vec<u8>> {
        self.buf.read().expect("page buffer poisoned")
    }

    /// Write the pinned page's bytes, marking the frame dirty so it is flushed
    /// (under WAL-before-data) before it is ever reused.
    ///
    /// The dirty flag is set *while holding the buffer write guard*, not before
    /// acquiring it. That couples "this page has un-flushed changes" to exclusive
    /// ownership of the bytes, so a concurrent `flush_all` — which clears the flag
    /// only while holding the buffer *read* guard — can never observe the flag and
    /// the bytes out of step and clear a dirty page whose new record it didn't
    /// write. (Buffer lock is always taken before the directory lock, here and in
    /// `flush_all`, so the two never deadlock.)
    pub fn write(&self) -> RwLockWriteGuard<'_, Vec<u8>> {
        let guard = self.buf.write().expect("page buffer poisoned");
        self.cache.dir.lock().expect("cache poisoned").frames[self.idx].dirty = true;
        guard
    }
}

impl Drop for PageRef<'_> {
    fn drop(&mut self) {
        let mut d = self.cache.dir.lock().expect("cache poisoned");
        d.frames[self.idx].pins -= 1;
        drop(d);
        self.cache.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::MemDisk;

    fn stamped_disk(n: u32) -> Arc<dyn BlockFile> {
        let disk = MemDisk::new();
        let mut page = vec![0u8; PAGE_SIZE];
        for pid in 0..n {
            page[..4].copy_from_slice(&pid.to_le_bytes());
            disk.write_at(&page, page_offset(pid)).unwrap();
        }
        Arc::new(disk)
    }

    fn stamp_of(buf: &[u8]) -> u32 {
        u32::from_le_bytes(buf[..4].try_into().unwrap())
    }

    #[test]
    fn fetch_reads_the_right_page_and_caches_it() {
        let cache = PageCache::open(stamped_disk(4), 4);
        let p = cache.fetch(2).unwrap();
        assert_eq!(stamp_of(&p.read()), 2);
        assert_eq!(cache.loads(), 1);
        drop(p);
        let p = cache.fetch(2).unwrap();
        assert_eq!(stamp_of(&p.read()), 2);
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.loads(), 1);
    }

    #[test]
    fn eviction_reloads_correctly() {
        let cache = PageCache::open(stamped_disk(8), 2);
        {
            let _a = cache.fetch(0).unwrap();
            let _b = cache.fetch(1).unwrap();
        }
        let c = cache.fetch(2).unwrap();
        assert_eq!(stamp_of(&c.read()), 2);
        assert_eq!(cache.evictions(), 1);
        drop(c);
        let a = cache.fetch(0).unwrap();
        assert_eq!(stamp_of(&a.read()), 0);
    }

    #[test]
    fn exhaustion_is_reported() {
        let cache = PageCache::open(stamped_disk(4), 2);
        let _a = cache.fetch(0).unwrap();
        let _b = cache.fetch(1).unwrap();
        assert!(
            matches!(cache.fetch(2), Err(CacheError::Exhausted)),
            "all frames pinned -> Exhausted, not a stall"
        );
    }

    struct CountingWal {
        flushed: AtomicU64,
    }
    impl WalSync for CountingWal {
        fn flushed_lsn(&self) -> u64 {
            self.flushed.load(Ordering::Relaxed)
        }
        fn flush_until(&self, lsn: u64) -> io::Result<()> {
            self.flushed.fetch_max(lsn, Ordering::Relaxed);
            Ok(())
        }
    }

    fn lsn_at_8(buf: &[u8]) -> u64 {
        u64::from_le_bytes(buf[4..12].try_into().unwrap())
    }

    #[test]
    fn dirty_page_is_flushed_before_reuse_under_wal() {
        let disk = stamped_disk(8);
        let wal: Arc<CountingWal> = Arc::new(CountingWal {
            flushed: AtomicU64::new(0),
        });
        let cache = PageCache::open_wal(disk.clone(), 2, wal.clone(), lsn_at_8);

        {
            let p = cache.fetch(0).unwrap();
            let mut b = p.write();
            b[4..12].copy_from_slice(&42u64.to_le_bytes());
        }
        drop(cache.fetch(1).unwrap());
        drop(cache.fetch(2).unwrap());

        assert!(
            cache.flushes() >= 1,
            "the dirty page was flushed on eviction"
        );
        assert!(
            wal.flushed_lsn() >= 42,
            "WAL was forced durable through the page LSN before its data write"
        );
        let p = cache.fetch(0).unwrap();
        assert_eq!(stamp_of(&p.read()), 0);
        assert_eq!(lsn_at_8(&p.read()), 42, "the dirty write reached disk");
    }
}
