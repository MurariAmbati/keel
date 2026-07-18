//! Page-latch primitives for the phase-7 concurrency protocol (D-LATCH-0).
//!
//! The buffer pool's fetch path is correct only single-threaded, because
//! find-or-load, victim choice, and evict-and-load are each multi-step read-
//! modify-writes (see `buffer`'s `choose_victim`/`evict_and_load`). Real
//! concurrency cannot be bought by mechanically swapping the pool's `RefCell`s
//! for `Mutex`es — that compiles but corrupts, because the *protocol* is what
//! races, not the cells. This crate builds the seam that a concurrent buffer
//! will stand on, in isolation and with its own race oracle, the way `lockmgr`
//! and `mvcc` were built and proven before being wired into the engine:
//!
//! * [`LatchTable`] — an **atomic find-or-install** directory from page id to a
//!   latch. Two threads racing on the same absent page get the *same* latch, so
//!   there is never a double-install (the fix for the find-or-load TOCTOU).
//! * [`PageLatch`] — a reader/writer **latch** over a page's contents: many
//!   readers or one writer, never both. This is the per-frame mutual exclusion
//!   the single-threaded design gets for free from `RefCell`'s panic.
//! * [`write_two_ordered`] — acquire two latches for writing in a globally
//!   consistent order (lower id first), so two threads latching the same pair
//!   from opposite sides can never form a cycle. Deadlock-freedom by discipline,
//!   which the buffer must preserve when it holds two pages at once (a B-tree
//!   node split, say).
//!
//! What this crate deliberately does **not** do yet: pin counts, eviction, or
//! any coupling to `BufferPool`. Those compose *on top* of a tested latch; this
//! is the tested latch.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A page number — the directory key. Matches `buffer::PageId`, kept local so
/// this primitive depends on nothing.
pub type PageId = u32;

/// A reader/writer latch guarding one page's value of type `T`.
///
/// A latch is *not* a lock-manager lock: it is short-duration, protects physical
/// consistency of a single page, and is released the moment the operation on
/// that page finishes (latch-coupling), whereas a 2PL lock is held to
/// transaction end. The `lockmgr` crate owns the latter; this owns the former.
pub struct PageLatch<T> {
    id: PageId,
    inner: RwLock<T>,
}

impl<T> PageLatch<T> {
    fn new(id: PageId, value: T) -> Self {
        Self {
            id,
            inner: RwLock::new(value),
        }
    }

    /// The page this latch guards.
    pub fn id(&self) -> PageId {
        self.id
    }

    /// Acquire shared (read) access. Coexists with other readers; blocks writers.
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.inner.read().expect("page latch poisoned")
    }

    /// Acquire exclusive (write) access. Blocks all other readers and writers.
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.inner.write().expect("page latch poisoned")
    }

    /// Try to acquire exclusive access without blocking. `None` if held.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.inner.try_write().ok()
    }
}

/// An atomic find-or-install directory from page id to its latch.
///
/// The directory mutex covers the check-then-insert as one indivisible step, so
/// concurrent callers converge on a single latch per page. This is the exact
/// property the buffer's `resident(pid)`-then-load path lacks.
pub struct LatchTable<T> {
    dir: Mutex<HashMap<PageId, Arc<PageLatch<T>>>>,
    installs: AtomicU64,
}

impl<T> Default for LatchTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LatchTable<T> {
    pub fn new() -> Self {
        Self {
            dir: Mutex::new(HashMap::new()),
            installs: AtomicU64::new(0),
        }
    }

    /// Return the latch for `pid`, installing one built by `make` if absent.
    ///
    /// Atomic: two threads that both find `pid` absent still return the *same*
    /// `Arc<PageLatch<T>>`, and `make` runs exactly once. `make` is a closure so
    /// the (possibly expensive) initial value is built only on a real install.
    pub fn get_or_install<F: FnOnce() -> T>(&self, pid: PageId, make: F) -> Arc<PageLatch<T>> {
        let mut dir = self.dir.lock().expect("latch table poisoned");
        if let Some(l) = dir.get(&pid) {
            return l.clone();
        }
        let l = Arc::new(PageLatch::new(pid, make()));
        dir.insert(pid, l.clone());
        self.installs.fetch_add(1, Ordering::Relaxed);
        l
    }

    /// The latch for `pid` if resident, without installing.
    pub fn get(&self, pid: PageId) -> Option<Arc<PageLatch<T>>> {
        self.dir
            .lock()
            .expect("latch table poisoned")
            .get(&pid)
            .cloned()
    }

    /// Drop `pid` from the directory (an eviction would call this while holding
    /// the frame's write latch). Any thread still holding the returned `Arc`
    /// keeps the latch alive, so an in-flight latch-holder is never invalidated.
    pub fn remove(&self, pid: PageId) -> Option<Arc<PageLatch<T>>> {
        self.dir.lock().expect("latch table poisoned").remove(&pid)
    }

    /// How many latches have ever been installed — the double-install detector.
    pub fn installs(&self) -> u64 {
        self.installs.load(Ordering::Relaxed)
    }

    /// Current resident latch count.
    pub fn len(&self) -> usize {
        self.dir.lock().expect("latch table poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Acquire two distinct latches for writing in a globally consistent order
/// (lower page id first), returning the guards positionally as `(for a, for b)`.
///
/// Because every caller acquires in id order regardless of argument order, two
/// threads that want the same pair from opposite sides request the *same*
/// underlying latch first, so no wait-for cycle can form. This is the discipline
/// the buffer must follow whenever an operation pins two pages at once.
pub fn write_two_ordered<'a, T>(
    a: &'a PageLatch<T>,
    b: &'a PageLatch<T>,
) -> (RwLockWriteGuard<'a, T>, RwLockWriteGuard<'a, T>) {
    assert_ne!(a.id, b.id, "a page cannot be latched against itself");
    if a.id < b.id {
        let ga = a.write();
        let gb = b.write();
        (ga, gb)
    } else {
        let gb = b.write();
        let ga = a.write();
        (ga, gb)
    }
}

/// A buffer frame's **pin discipline** — the piece D-LATCH-1 left to compose on
/// top of the latch. A frame couples a page-content latch with a pin count and
/// an `evicted` tombstone, kept consistent *with each other* under one small
/// mutex so the two decisions are atomic:
///
/// * a frame can **never be evicted while pinned** (`try_evict` fails if
///   `pins > 0`), and
/// * a frame can **never be pinned after its eviction has begun** (`pin` returns
///   `None` once `evicted`), so a page can't be resurrected in a frame that is
///   already being replaced.
///
/// This is exactly the ordering the buffer's `choose_victim`/`evict_and_load`
/// lacks: there, pin, dirty, and the CLOCK bits are separate `Cell`s read in
/// sequence, so a victim can be chosen the instant another thread pins it. Here
/// the pin/evict handshake is one critical section. The page *contents* are
/// still guarded by the separate reader/writer [`PageLatch`] — pins gate
/// *residency*, latches gate *access*, and they are deliberately different locks
/// (you hold a pin for the whole time a page is in use, but latch-couple only
/// across a single operation).
pub struct Frame<T> {
    latch: PageLatch<T>,
    state: Mutex<PinState>,
}

#[derive(Default)]
struct PinState {
    pins: u32,
    evicted: bool,
}

/// Proof that a frame is pinned: while this guard lives, the frame cannot be
/// evicted. Unpins on drop.
pub struct PinGuard<'a, T> {
    frame: &'a Frame<T>,
}

impl<T> Frame<T> {
    /// A fresh, unpinned, resident frame holding `value` for page `id`.
    pub fn new(id: PageId, value: T) -> Self {
        Self {
            latch: PageLatch::new(id, value),
            state: Mutex::new(PinState::default()),
        }
    }

    /// The page this frame currently holds.
    pub fn id(&self) -> PageId {
        self.latch.id()
    }

    /// Pin the frame so it cannot be evicted until the guard drops. Returns
    /// `None` if eviction has already begun — the caller must re-look-up the
    /// page rather than touch a frame that is being replaced.
    pub fn pin(&self) -> Option<PinGuard<'_, T>> {
        let mut s = self.state.lock().expect("frame poisoned");
        if s.evicted {
            return None;
        }
        s.pins += 1;
        Some(PinGuard { frame: self })
    }

    /// Attempt to evict: succeeds only if the frame is unpinned and not already
    /// evicted, in which case it is tombstoned so no further pins are granted.
    /// Returns whether this call performed the eviction.
    pub fn try_evict(&self) -> bool {
        let mut s = self.state.lock().expect("frame poisoned");
        if s.pins == 0 && !s.evicted {
            s.evicted = true;
            true
        } else {
            false
        }
    }

    /// Whether eviction has begun on this frame.
    pub fn is_evicted(&self) -> bool {
        self.state.lock().expect("frame poisoned").evicted
    }

    /// The current pin count (for assertions and CLOCK bookkeeping).
    pub fn pin_count(&self) -> u32 {
        self.state.lock().expect("frame poisoned").pins
    }
}

impl<'a, T> PinGuard<'a, T> {
    /// The pinned frame's content latch — take a read or write latch through it.
    /// Safe to hold across the pin because the frame can't be evicted while this
    /// guard is alive.
    pub fn latch(&self) -> &'a PageLatch<T> {
        &self.frame.latch
    }

    /// The pinned frame's page id.
    pub fn id(&self) -> PageId {
        self.frame.id()
    }
}

impl<T> Drop for PinGuard<'_, T> {
    fn drop(&mut self) {
        let mut s = self.frame.state.lock().expect("frame poisoned");
        s.pins -= 1;
    }
}

/// A fixed-capacity **concurrent page cache with CLOCK replacement** — the
/// reference assembly of every primitive above, and the last piece before the
/// real `BufferPool` refactor (D-LATCH-0). It exists to prove the whole protocol
/// end to end in a controlled, disk-free setting: `BufferPool` will mirror this
/// shape, swapping the in-memory `make` closure for a `vfs` read + WAL-before-data
/// flush.
///
/// The load-bearing decision: **all** residency, pin, ref-bit, and CLOCK-hand
/// state lives under one `Mutex`, so choosing a victim and taking it are a single
/// atomic step. A frame cannot be pinned by another thread between being selected
/// as a victim and being replaced, and a pinned frame is never a candidate — the
/// two races (`choose_victim` picking a just-pinned frame; find-or-load loading a
/// page twice) that a bare `RefCell → Mutex` swap of the current pool would leave
/// open. What this reference deliberately simplifies versus the real pool: it
/// builds page contents under the pool lock (`make` runs while held), whereas the
/// engine must do its I/O *outside* the lock — the added complexity the
/// `BufferPool` assembly owns, called out here so the gap is explicit.
pub struct ClockPool<T> {
    inner: Mutex<Clock<T>>,
    hits: AtomicU64,
    loads: AtomicU64,
    evictions: AtomicU64,
}

struct Clock<T> {
    cap: usize,
    dir: HashMap<PageId, usize>,
    frames: Vec<Slot<T>>,
    hand: usize,
}

struct Slot<T> {
    pid: Option<PageId>,
    pins: u32,
    ref_bit: bool,
    val: Option<T>,
}

impl<T> Slot<T> {
    fn empty() -> Self {
        Self {
            pid: None,
            pins: 0,
            ref_bit: false,
            val: None,
        }
    }
}

/// A pinned handle into a [`ClockPool`] frame. While it lives the frame cannot be
/// evicted, so the page it names stays resident. Unpins on drop.
pub struct Pinned<'a, T> {
    pool: &'a ClockPool<T>,
    idx: usize,
    pid: PageId,
}

impl<T> ClockPool<T> {
    /// A pool of `cap` frames, all initially empty.
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "a pool needs at least one frame");
        let frames = (0..cap).map(|_| Slot::empty()).collect();
        Self {
            inner: Mutex::new(Clock {
                cap,
                dir: HashMap::new(),
                frames,
                hand: 0,
            }),
            hits: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Pin the frame holding `pid`, loading it (over a CLOCK-chosen unpinned
    /// victim) if not resident. Returns `None` only when every frame is pinned —
    /// the honest "pool exhausted" signal, never a silent stall.
    pub fn acquire<F: FnOnce() -> T>(&self, pid: PageId, make: F) -> Option<Pinned<'_, T>> {
        let mut c = self.inner.lock().expect("pool poisoned");

        if let Some(&idx) = c.dir.get(&pid) {
            c.frames[idx].pins += 1;
            c.frames[idx].ref_bit = true;
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(Pinned {
                pool: self,
                idx,
                pid,
            });
        }

        let victim = c.choose_victim()?;
        if let Some(old) = c.frames[victim].pid.take() {
            c.dir.remove(&old);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        let val = make();
        c.frames[victim] = Slot {
            pid: Some(pid),
            pins: 1,
            ref_bit: true,
            val: Some(val),
        };
        c.dir.insert(pid, victim);
        self.loads.fetch_add(1, Ordering::Relaxed);
        Some(Pinned {
            pool: self,
            idx: victim,
            pid,
        })
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

    /// Total pins currently held across all frames (for assertions).
    pub fn live_pins(&self) -> u32 {
        self.inner
            .lock()
            .expect("pool poisoned")
            .frames
            .iter()
            .map(|f| f.pins)
            .sum()
    }
}

impl<T> Clock<T> {
    /// CLOCK: advance the hand past pinned frames, clearing ref-bits for a second
    /// chance, until an unpinned frame is found. `None` if all frames are pinned.
    fn choose_victim(&mut self) -> Option<usize> {
        let n = self.cap;
        let mut scanned = 0usize;
        loop {
            let h = self.hand;
            self.hand = (h + 1) % n;
            if self.frames[h].pins == 0 {
                if self.frames[h].ref_bit {
                    self.frames[h].ref_bit = false;
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

impl<T: Clone> Pinned<'_, T> {
    /// The page id this handle pins.
    pub fn pid(&self) -> PageId {
        self.pid
    }

    /// A clone of the pinned page's value. The frame can't be evicted while this
    /// handle lives, so the value is always the one for `pid`.
    pub fn value(&self) -> T {
        let c = self.pool.inner.lock().expect("pool poisoned");
        c.frames[self.idx]
            .val
            .clone()
            .expect("pinned frame holds a value")
    }
}

impl<T> Drop for Pinned<'_, T> {
    fn drop(&mut self) {
        let mut c = self.pool.inner.lock().expect("pool poisoned");
        c.frames[self.idx].pins -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_then_get_returns_the_same_latch() {
        let t: LatchTable<i64> = LatchTable::new();
        let a = t.get_or_install(7, || 100);
        let b = t.get_or_install(7, || 999);
        assert!(
            Arc::ptr_eq(&a, &b),
            "same page id must yield the same latch"
        );
        assert_eq!(*a.read(), 100, "the second install's value is ignored");
        assert_eq!(t.installs(), 1);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn read_and_write_guards_see_and_mutate() {
        let latch = PageLatch::new(3, 10i64);
        assert_eq!(*latch.read(), 10);
        *latch.write() += 5;
        assert_eq!(*latch.read(), 15);
        assert_eq!(latch.id(), 3);
    }

    #[test]
    fn remove_drops_from_directory_but_keeps_holder_alive() {
        let t: LatchTable<i64> = LatchTable::new();
        let held = t.get_or_install(1, || 42);
        let removed = t.remove(1).expect("was resident");
        assert!(Arc::ptr_eq(&held, &removed));
        assert!(t.get(1).is_none(), "gone from the directory");
        assert_eq!(*held.read(), 42, "the holder's latch is still valid");
    }

    #[test]
    fn try_write_fails_while_write_held() {
        let latch = PageLatch::new(0, 0i64);
        let g = latch.write();
        assert!(latch.try_write().is_none(), "already exclusively held");
        drop(g);
        assert!(latch.try_write().is_some(), "free again");
    }

    #[test]
    fn write_two_ordered_returns_guards_positionally() {
        let a = PageLatch::new(5, 1i64);
        let b = PageLatch::new(2, 2i64);
        let (mut ga, mut gb) = write_two_ordered(&a, &b);
        *ga += 10;
        *gb += 20;
        drop((ga, gb));
        assert_eq!(*a.read(), 11);
        assert_eq!(*b.read(), 22);
    }

    #[test]
    fn pinned_frame_cannot_be_evicted() {
        let frame = Frame::new(4, 0i64);
        let g = frame.pin().expect("fresh frame pins");
        assert_eq!(frame.pin_count(), 1);
        assert!(!frame.try_evict(), "pinned frame refuses eviction");
        assert!(!frame.is_evicted());
        drop(g);
        assert_eq!(frame.pin_count(), 0);
        assert!(frame.try_evict(), "unpinned frame evicts");
    }

    #[test]
    fn eviction_tombstones_against_new_pins() {
        let frame = Frame::new(4, 0i64);
        assert!(frame.try_evict());
        assert!(frame.is_evicted());
        assert!(
            frame.pin().is_none(),
            "no pin may be granted once eviction has begun"
        );
        assert!(!frame.try_evict(), "double eviction is refused");
    }

    #[test]
    fn pin_guard_exposes_the_content_latch() {
        let frame = Frame::new(9, 100i64);
        let g = frame.pin().expect("pins");
        assert_eq!(g.id(), 9);
        *g.latch().write() += 1;
        assert_eq!(*g.latch().read(), 101);
    }

    #[test]
    fn clock_pool_hits_loads_and_evicts() {
        let pool: ClockPool<i64> = ClockPool::new(2);
        let a = pool.acquire(10, || 10).unwrap();
        let b = pool.acquire(20, || 20).unwrap();
        assert_eq!(a.value(), 10);
        assert_eq!(b.value(), 20);
        assert_eq!(pool.loads(), 2);
        assert_eq!(pool.hits(), 0);
        let a2 = pool.acquire(10, || -1).unwrap();
        assert_eq!(a2.value(), 10, "the make closure must not have run");
        assert_eq!(pool.hits(), 1);
        drop((a, a2));
        drop(b);
        let c = pool.acquire(30, || 30).unwrap();
        assert_eq!(c.value(), 30);
        assert_eq!(pool.loads(), 3);
        assert_eq!(pool.evictions(), 1);
    }

    #[test]
    fn clock_pool_reports_exhaustion_not_stall() {
        let pool: ClockPool<i64> = ClockPool::new(2);
        let _a = pool.acquire(1, || 1).unwrap();
        let _b = pool.acquire(2, || 2).unwrap();
        assert!(
            pool.acquire(3, || 3).is_none(),
            "exhaustion is signalled, never a silent stall"
        );
        assert_eq!(pool.live_pins(), 2);
    }
}
