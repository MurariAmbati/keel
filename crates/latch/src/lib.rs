use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub type PageId = u32;

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

    pub fn id(&self) -> PageId {
        self.id
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.inner.read().expect("page latch poisoned")
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.inner.write().expect("page latch poisoned")
    }

    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.inner.try_write().ok()
    }
}

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

    pub fn get(&self, pid: PageId) -> Option<Arc<PageLatch<T>>> {
        self.dir
            .lock()
            .expect("latch table poisoned")
            .get(&pid)
            .cloned()
    }

    pub fn remove(&self, pid: PageId) -> Option<Arc<PageLatch<T>>> {
        self.dir.lock().expect("latch table poisoned").remove(&pid)
    }

    pub fn installs(&self) -> u64 {
        self.installs.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.dir.lock().expect("latch table poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

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

pub struct Frame<T> {
    latch: PageLatch<T>,
    state: Mutex<PinState>,
}

#[derive(Default)]
struct PinState {
    pins: u32,
    evicted: bool,
}

pub struct PinGuard<'a, T> {
    frame: &'a Frame<T>,
}

impl<T> Frame<T> {
    pub fn new(id: PageId, value: T) -> Self {
        Self {
            latch: PageLatch::new(id, value),
            state: Mutex::new(PinState::default()),
        }
    }

    pub fn id(&self) -> PageId {
        self.latch.id()
    }

    pub fn pin(&self) -> Option<PinGuard<'_, T>> {
        let mut s = self.state.lock().expect("frame poisoned");
        if s.evicted {
            return None;
        }
        s.pins += 1;
        Some(PinGuard { frame: self })
    }

    pub fn try_evict(&self) -> bool {
        let mut s = self.state.lock().expect("frame poisoned");
        if s.pins == 0 && !s.evicted {
            s.evicted = true;
            true
        } else {
            false
        }
    }

    pub fn is_evicted(&self) -> bool {
        self.state.lock().expect("frame poisoned").evicted
    }

    pub fn pin_count(&self) -> u32 {
        self.state.lock().expect("frame poisoned").pins
    }
}

impl<'a, T> PinGuard<'a, T> {
    pub fn latch(&self) -> &'a PageLatch<T> {
        &self.frame.latch
    }

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

pub struct Pinned<'a, T> {
    pool: &'a ClockPool<T>,
    idx: usize,
    pid: PageId,
}

impl<T> ClockPool<T> {
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
    pub fn pid(&self) -> PageId {
        self.pid
    }

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
