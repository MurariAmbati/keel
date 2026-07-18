use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use keel_page::PAGE_SIZE;
use keel_vfs::BlockFile;

pub type PageId = u32;

#[inline]
fn page_offset(pid: PageId) -> u64 {
    pid as u64 * PAGE_SIZE as u64
}

pub trait WalSync: Send + Sync {
    fn flushed_lsn(&self) -> u64;
    fn flush_until(&self, lsn: u64) -> io::Result<()>;
}

pub struct NoWal;
impl WalSync for NoWal {
    fn flushed_lsn(&self) -> u64 {
        u64::MAX
    }
    fn flush_until(&self, _lsn: u64) -> io::Result<()> {
        Ok(())
    }
}

pub type LsnOf = fn(&[u8]) -> u64;

pub type StampOf = fn(&mut [u8]);

pub type VerifyOf = fn(&[u8]) -> bool;

fn no_stamp(_: &mut [u8]) {}
fn always_valid(_: &[u8]) -> bool {
    true
}

#[derive(Clone, Copy)]
pub struct PageFormat {
    pub lsn_of: LsnOf,
    pub stamp: StampOf,
    pub verify: VerifyOf,
}

impl PageFormat {
    pub fn opaque() -> Self {
        Self {
            lsn_of: no_lsn,
            stamp: no_stamp,
            verify: always_valid,
        }
    }

    pub fn keel_page() -> Self {
        Self {
            lsn_of: keel_page::raw::page_lsn,
            stamp: keel_page::raw::stamp_checksum,
            verify: keel_page::raw::verify_checksum,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fetch {
    Normal,
    Redo,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Abort {
    KeepOldDirty,
    Discard,
}

fn no_lsn(_: &[u8]) -> u64 {
    0
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    Exhausted,
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

struct Frame {
    pid: Option<PageId>,
    pins: u32,
    ref_bit: bool,
    dirty: bool,
    busy: bool,
    buf: Arc<RwLock<Vec<u8>>>,
}

struct Dir {
    cap: usize,
    map: HashMap<PageId, usize>,
    frames: Vec<Frame>,
    hand: usize,
    next_page: PageId,
    steal: bool,
}

impl Dir {
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

pub struct PageCache {
    file: Arc<dyn BlockFile>,
    wal: Arc<dyn WalSync>,
    fmt: PageFormat,
    dir: Mutex<Dir>,
    dpt: Mutex<HashMap<PageId, u64>>,
    ready: Condvar,
    hits: AtomicU64,
    loads: AtomicU64,
    evictions: AtomicU64,
    flushes: AtomicU64,
    allocations: AtomicU64,
}

pub struct PageRef<'a> {
    cache: &'a PageCache,
    idx: usize,
    pid: PageId,
    buf: Arc<RwLock<Vec<u8>>>,
}

impl PageCache {
    pub fn open(file: Arc<dyn BlockFile>, cap: usize) -> Self {
        Self::open_formatted(file, cap, Arc::new(NoWal), PageFormat::opaque())
    }

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

    pub fn fetch(&self, pid: PageId) -> Result<PageRef<'_>, CacheError> {
        self.fetch_mode(pid, Fetch::Normal)
    }

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

    fn write_stamped(&self, bytes: &[u8], pid: PageId) -> io::Result<()> {
        let mut out = bytes.to_vec();
        (self.fmt.stamp)(&mut out);
        self.file.write_at(&out, page_offset(pid))
    }

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
    pub fn set_no_steal(&self) {
        self.dir.lock().expect("cache poisoned").steal = false;
    }

    pub fn note_dirty(&self, pid: PageId, reclsn: u64) {
        self.dpt
            .lock()
            .expect("cache poisoned")
            .entry(pid)
            .or_insert(reclsn);
    }

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

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }

    pub fn page_count(&self) -> PageId {
        self.dir.lock().expect("cache poisoned").next_page
    }

    pub fn live_pins(&self) -> u32 {
        self.dir
            .lock()
            .expect("cache poisoned")
            .frames
            .iter()
            .map(|f| f.pins)
            .sum()
    }

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

    pub fn checkpoint(&self) -> io::Result<()> {
        self.flush_all()?;
        self.file.sync()
    }
}

impl PageRef<'_> {
    pub fn pid(&self) -> PageId {
        self.pid
    }

    pub fn read(&self) -> RwLockReadGuard<'_, Vec<u8>> {
        self.buf.read().expect("page buffer poisoned")
    }

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
