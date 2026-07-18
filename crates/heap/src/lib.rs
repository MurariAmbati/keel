use std::cell::{Cell, RefCell};

use keel_buffer::{BufferError, BufferPool, PageId};
use keel_page::{PageError, PageType, SlotId, SlottedPage, MAX_TUPLE_SIZE};
use keel_pager::Pager;

const TAG_TUPLE: u8 = 0;
const TAG_FORWARD: u8 = 1;
const TAG_FORWARD_TARGET: u8 = 2;

const RID_BYTES: usize = 6;
const TAG_BYTES: usize = 1;

pub const MAX_RECORD: usize = MAX_TUPLE_SIZE - TAG_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Tuple,
    Forward(Rid),
    ForwardTarget,
}

pub fn classify_record(bytes: &[u8]) -> std::result::Result<RecordKind, u8> {
    match bytes.first() {
        Some(&TAG_TUPLE) => Ok(RecordKind::Tuple),
        Some(&TAG_FORWARD) if bytes.len() > RID_BYTES => {
            Ok(RecordKind::Forward(Rid::decode(&bytes[1..])))
        }
        Some(&TAG_FORWARD_TARGET) => Ok(RecordKind::ForwardTarget),
        Some(&other) => Err(other),
        None => Err(u8::MAX),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rid {
    pub page: PageId,
    pub slot: SlotId,
}

impl Rid {
    pub fn new(page: PageId, slot: SlotId) -> Self {
        Self { page, slot }
    }
    fn encode(self) -> [u8; RID_BYTES] {
        let mut b = [0u8; RID_BYTES];
        b[0..4].copy_from_slice(&self.page.to_le_bytes());
        b[4..6].copy_from_slice(&self.slot.to_le_bytes());
        b
    }
    fn decode(b: &[u8]) -> Rid {
        Rid {
            page: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            slot: u16::from_le_bytes([b[4], b[5]]),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    pub inserts: u64,
    pub gets: u64,
    pub updates: u64,
    pub deletes: u64,
    pub forward_hops: u64,
    pub forwards_created: u64,
    pub new_pages: u64,
}

#[derive(Debug)]
pub enum HeapError {
    Buffer(BufferError),
    RecordTooLarge,
    DanglingForward(Rid),
    BadTag(u8),
    ForwardWontFit(Rid),
}

impl std::fmt::Display for HeapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeapError::Buffer(e) => write!(f, "{e}"),
            HeapError::RecordTooLarge => write!(f, "record too large for a page"),
            HeapError::DanglingForward(r) => write!(f, "dangling forward to {r:?}"),
            HeapError::BadTag(t) => write!(f, "bad heap record tag {t}"),
            HeapError::ForwardWontFit(r) => write!(f, "forward stub won't fit at {r:?}"),
        }
    }
}
impl std::error::Error for HeapError {}
impl From<BufferError> for HeapError {
    fn from(e: BufferError) -> Self {
        HeapError::Buffer(e)
    }
}
impl From<keel_pager::PagerError> for HeapError {
    fn from(e: keel_pager::PagerError) -> Self {
        HeapError::Buffer(match e {
            keel_pager::PagerError::Io(e) => BufferError::Io(e),
            keel_pager::PagerError::Corrupt(p) => BufferError::Corrupt(p),
            keel_pager::PagerError::Exhausted => BufferError::Exhausted,
        })
    }
}

pub type Result<T> = std::result::Result<T, HeapError>;

pub struct HeapFile<'a, P: Pager = BufferPool> {
    bp: &'a P,
    fsm: RefCell<Vec<u16>>,
    cursor: Cell<usize>,
    stats: Cell<HeapStats>,
}

impl<'a, P: Pager> HeapFile<'a, P> {
    pub fn open(bp: &'a P) -> Result<Self> {
        let n = Pager::page_count(bp);
        let mut fsm = Vec::with_capacity(n as usize);
        for pid in 0..n {
            let free = bp.with_page(pid, |b| {
                let page = SlottedPage::from_bytes(b);
                if page.page_type() == Some(PageType::Heap) {
                    page.compactable_free() as u16
                } else {
                    0
                }
            })?;
            fsm.push(free);
        }
        Ok(Self {
            bp,
            fsm: RefCell::new(fsm),
            cursor: Cell::new(0),
            stats: Cell::new(HeapStats::default()),
        })
    }

    pub fn stats(&self) -> HeapStats {
        self.stats.get()
    }

    fn bump<F: FnOnce(&mut HeapStats)>(&self, f: F) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }

    fn set_fsm(&self, pid: PageId, free: usize) {
        let mut fsm = self.fsm.borrow_mut();
        while fsm.len() <= pid as usize {
            fsm.push(0);
        }
        fsm[pid as usize] = free.min(u16::MAX as usize) as u16;
    }

    fn pick_page(&self, need: usize) -> Option<PageId> {
        let want = need + keel_page::SLOT_SIZE;
        let fsm = self.fsm.borrow();
        let n = fsm.len();
        if n == 0 {
            return None;
        }
        let start = self.cursor.get() % n;
        for i in 0..n {
            let idx = (start + i) % n;
            if fsm[idx] as usize >= want {
                self.cursor.set((idx + 1) % n);
                return Some(idx as PageId);
            }
        }
        None
    }

    fn read_slot(&self, rid: Rid) -> Result<Option<(u8, Vec<u8>)>> {
        if rid.page >= Pager::page_count(self.bp) {
            return Ok(None);
        }
        Ok(self.bp.with_page(rid.page, |b| {
            match SlottedPage::from_bytes(b).get(rid.slot) {
                None | Some([]) => None,
                Some(bytes) => Some((bytes[0], bytes[1..].to_vec())),
            }
        })?)
    }

    fn insert_tagged(&self, tag: u8, record: &[u8]) -> Result<Rid> {
        let need = TAG_BYTES + record.len();
        if need > MAX_TUPLE_SIZE {
            return Err(HeapError::RecordTooLarge);
        }
        let mut buf = Vec::with_capacity(need);
        buf.push(tag);
        buf.extend_from_slice(record);

        if let Some(pid) = self.pick_page(need) {
            let (res, free) = self.bp.with_page_mut(pid, |b| {
                let mut page = SlottedPage::from_bytes(b);
                let res = page.insert(&buf);
                let free = page.compactable_free();
                (res, free)
            })?;
            match res {
                Ok(slot) => {
                    self.set_fsm(pid, free);
                    return Ok(Rid::new(pid, slot));
                }
                Err(PageError::PageFull) => {
                    self.set_fsm(pid, free);
                }
                Err(PageError::TupleTooLarge) => return Err(HeapError::RecordTooLarge),
                Err(e) => panic!("unexpected page error on insert: {e:?}"),
            }
        }

        let pid = self.bp.alloc_slotted(PageType::Heap)?;
        let (slot, free) = self.bp.with_page_mut(pid, |b| {
            let mut page = SlottedPage::from_bytes(b);
            let slot = page
                .insert(&buf)
                .expect("a fresh page must hold a record that is <= MAX_TUPLE_SIZE");
            (slot, page.compactable_free())
        })?;
        self.set_fsm(pid, free);
        self.bump(|s| s.new_pages += 1);
        Ok(Rid::new(pid, slot))
    }

    fn delete_slot(&self, rid: Rid) -> Result<bool> {
        let (existed, free) = self.bp.with_page_mut(rid.page, |b| {
            let mut page = SlottedPage::from_bytes(b);
            let existed = page.delete(rid.slot);
            (existed, page.compactable_free())
        })?;
        self.set_fsm(rid.page, free);
        Ok(existed)
    }

    fn set_slot(&self, rid: Rid, tag: u8, record: &[u8]) -> Result<bool> {
        let mut buf = Vec::with_capacity(TAG_BYTES + record.len());
        buf.push(tag);
        buf.extend_from_slice(record);
        let (res, free) = self.bp.with_page_mut(rid.page, |b| {
            let mut page = SlottedPage::from_bytes(b);
            let res = page.set(rid.slot, &buf);
            let free = page.compactable_free();
            (res, free)
        })?;
        match res {
            Ok(()) => {
                self.set_fsm(rid.page, free);
                Ok(true)
            }
            Err(PageError::PageFull) => Ok(false),
            Err(PageError::TupleTooLarge) => Err(HeapError::RecordTooLarge),
            Err(e) => panic!("unexpected page error on set at {rid:?} tag={tag}: {e:?}"),
        }
    }

    fn write_stub(&self, at: Rid, target: Rid) -> Result<bool> {
        let mut buf = Vec::with_capacity(TAG_BYTES + RID_BYTES);
        buf.push(TAG_FORWARD);
        buf.extend_from_slice(&target.encode());
        let (res, free) = self.bp.with_page_mut(at.page, |b| {
            let mut page = SlottedPage::from_bytes(b);
            let res = page.set(at.slot, &buf);
            let free = page.compactable_free();
            (res, free)
        })?;
        match res {
            Ok(()) => {
                self.set_fsm(at.page, free);
                Ok(true)
            }
            Err(PageError::PageFull) => Ok(false),
            Err(e) => panic!("unexpected page error writing stub: {e:?}"),
        }
    }

    pub fn insert(&self, record: &[u8]) -> Result<Rid> {
        if record.len() > MAX_RECORD {
            return Err(HeapError::RecordTooLarge);
        }
        self.bump(|s| s.inserts += 1);
        self.insert_tagged(TAG_TUPLE, record)
    }

    pub fn get(&self, rid: Rid) -> Result<Option<Vec<u8>>> {
        self.bump(|s| s.gets += 1);
        let (tag, payload) = match self.read_slot(rid)? {
            Some(x) => x,
            None => return Ok(None),
        };
        match tag {
            TAG_TUPLE => Ok(Some(payload)),
            TAG_FORWARD => {
                self.bump(|s| s.forward_hops += 1);
                let target = Rid::decode(&payload);
                match self.read_slot(target)? {
                    Some((TAG_FORWARD_TARGET, p)) | Some((TAG_TUPLE, p)) => Ok(Some(p)),
                    _ => Err(HeapError::DanglingForward(target)),
                }
            }
            TAG_FORWARD_TARGET => Ok(None),
            other => Err(HeapError::BadTag(other)),
        }
    }

    pub fn delete(&self, rid: Rid) -> Result<bool> {
        self.bump(|s| s.deletes += 1);
        let (tag, payload) = match self.read_slot(rid)? {
            Some(x) => x,
            None => return Ok(false),
        };
        match tag {
            TAG_TUPLE => {
                self.delete_slot(rid)?;
                Ok(true)
            }
            TAG_FORWARD => {
                let target = Rid::decode(&payload);
                self.delete_slot(target)?;
                self.delete_slot(rid)?;
                Ok(true)
            }
            TAG_FORWARD_TARGET => Ok(false),
            other => Err(HeapError::BadTag(other)),
        }
    }

    pub fn update(&self, rid: Rid, record: &[u8]) -> Result<bool> {
        if record.len() > MAX_RECORD {
            return Err(HeapError::RecordTooLarge);
        }
        self.bump(|s| s.updates += 1);
        let (tag, payload) = match self.read_slot(rid)? {
            Some(x) => x,
            None => return Ok(false),
        };
        match tag {
            TAG_TUPLE => {
                if self.set_slot(rid, TAG_TUPLE, record)? {
                    return Ok(true);
                }
                let target = self.insert_tagged(TAG_FORWARD_TARGET, record)?;
                if self.write_stub(rid, target)? {
                    self.bump(|s| s.forwards_created += 1);
                    Ok(true)
                } else {
                    self.delete_slot(target)?;
                    Err(HeapError::ForwardWontFit(rid))
                }
            }
            TAG_FORWARD => {
                let target = Rid::decode(&payload);
                if self.set_slot(target, TAG_FORWARD_TARGET, record)? {
                    return Ok(true);
                }
                let new_target = self.insert_tagged(TAG_FORWARD_TARGET, record)?;
                self.delete_slot(target)?;
                let ok = self.write_stub(rid, new_target)?;
                debug_assert!(ok, "repointing a stub is a same-size write and must fit");
                Ok(true)
            }
            TAG_FORWARD_TARGET => Ok(false),
            other => Err(HeapError::BadTag(other)),
        }
    }

    pub fn scan(&self) -> Result<Vec<(Rid, Vec<u8>)>> {
        let mut out = Vec::new();
        for pid in 0..Pager::page_count(self.bp) {
            let slots: Option<Vec<(SlotId, u8, Vec<u8>)>> = self.bp.with_page(pid, |b| {
                let p = SlottedPage::from_bytes(b);
                if p.page_type() != Some(PageType::Heap) {
                    return None;
                }
                Some(
                    p.iter()
                        .filter(|(_, b)| !b.is_empty())
                        .map(|(s, b)| (s, b[0], b[1..].to_vec()))
                        .collect(),
                )
            })?;
            let Some(slots) = slots else { continue };
            for (slot, tag, payload) in slots {
                let rid = Rid::new(pid, slot);
                match tag {
                    TAG_TUPLE => out.push((rid, payload)),
                    TAG_FORWARD => {
                        self.bump(|s| s.forward_hops += 1);
                        let target = Rid::decode(&payload);
                        match self.read_slot(target)? {
                            Some((TAG_FORWARD_TARGET, p)) | Some((TAG_TUPLE, p)) => {
                                out.push((rid, p))
                            }
                            _ => return Err(HeapError::DanglingForward(target)),
                        }
                    }
                    TAG_FORWARD_TARGET => {}
                    other => return Err(HeapError::BadTag(other)),
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_buffer::BufferPool;
    use keel_rng::Rng;
    use keel_vfs::{BlockFile, MemDisk};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn fresh_pool(frames: usize) -> BufferPool {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        BufferPool::open_default(disk, frames).unwrap()
    }

    #[test]
    fn insert_get_delete() {
        let bp = fresh_pool(16);
        let h = HeapFile::open(&bp).unwrap();
        let r1 = h.insert(b"hello").unwrap();
        let r2 = h.insert(b"world").unwrap();
        assert_eq!(h.get(r1).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(h.get(r2).unwrap().as_deref(), Some(&b"world"[..]));
        assert!(h.delete(r1).unwrap());
        assert_eq!(h.get(r1).unwrap(), None);
        assert!(!h.delete(r1).unwrap());
    }

    #[test]
    fn update_in_place_and_grow() {
        let bp = fresh_pool(16);
        let h = HeapFile::open(&bp).unwrap();
        let r = h.insert(b"aaaaaaaa").unwrap();
        assert!(h.update(r, b"bb").unwrap());
        assert_eq!(h.get(r).unwrap().as_deref(), Some(&b"bb"[..]));
        assert!(h.update(r, b"cccccccccccccccccccc").unwrap());
        assert_eq!(
            h.get(r).unwrap().as_deref(),
            Some(&b"cccccccccccccccccccc"[..])
        );
    }

    #[test]
    fn update_forces_forward_and_rid_stays_valid() {
        let bp = fresh_pool(16);
        let h = HeapFile::open(&bp).unwrap();
        let mut rids = Vec::new();
        for i in 0..200 {
            rids.push(
                h.insert(format!("row-{i:04}-xxxxxxxxxxxxxxxxxxxx").as_bytes())
                    .unwrap(),
            );
        }
        let victim = rids[0];
        let big = vec![b'Z'; 4000];
        assert!(h.update(victim, &big).unwrap());
        assert_eq!(h.get(victim).unwrap().as_deref(), Some(big.as_slice()));
        assert!(
            h.stats().forwards_created >= 1,
            "the update should have forwarded"
        );
        assert!(h.stats().forward_hops >= 1);
    }

    #[test]
    fn forwarded_tuple_updated_again_keeps_chain_length_one() {
        let bp = fresh_pool(16);
        let h = HeapFile::open(&bp).unwrap();
        let mut rids = Vec::new();
        for i in 0..200 {
            rids.push(
                h.insert(format!("row-{i:04}-yyyyyyyyyyyyyyyyyyyy").as_bytes())
                    .unwrap(),
            );
        }
        let victim = rids[0];
        h.update(victim, &vec![b'A'; 4000]).unwrap();
        let hops_after_first = h.stats().forward_hops;
        h.update(victim, &vec![b'B'; 5000]).unwrap();
        assert_eq!(h.get(victim).unwrap().unwrap().len(), 5000);
        let g = h.get(victim).unwrap();
        assert_eq!(g.unwrap()[0], b'B');
        let _ = hops_after_first;
    }

    #[test]
    fn scan_yields_each_logical_tuple_once() {
        let bp = fresh_pool(16);
        let h = HeapFile::open(&bp).unwrap();
        let mut model: HashMap<Rid, Vec<u8>> = HashMap::new();
        for i in 0..300 {
            let rec = format!("scan-{i:04}").into_bytes();
            let rid = h.insert(&rec).unwrap();
            model.insert(rid, rec);
        }
        let keys: Vec<Rid> = model.keys().copied().collect();
        for (k, rid) in keys.iter().enumerate() {
            if k % 7 == 0 {
                let big = vec![b'X'; 3000];
                h.update(*rid, &big).unwrap();
                model.insert(*rid, big);
            }
        }
        let scanned = h.scan().unwrap();
        assert_eq!(scanned.len(), model.len(), "scan count must match model");
        let scanned_map: HashMap<Rid, Vec<u8>> = scanned.into_iter().collect();
        assert_eq!(scanned_map, model);
    }

    #[test]
    fn stale_rid_recycled_as_forward_target_is_inert() {
        let bp = fresh_pool(16);
        let h = HeapFile::open(&bp).unwrap();
        let mut filler = Vec::new();
        for i in 0..300 {
            filler.push(
                h.insert(format!("fill-{i:04}-aaaaaaaaaaaaaaaaaaaa").as_bytes())
                    .unwrap(),
            );
        }
        let stale = h.insert(b"soon-to-be-deleted-and-recycled").unwrap();
        assert!(h.delete(stale).unwrap());
        let owner = filler[0];
        h.update(owner, &vec![b'Z'; 4000]).unwrap();
        assert!(h.stats().forwards_created >= 1);
        assert_eq!(h.get(stale).unwrap(), None);
        assert!(!h.delete(stale).unwrap());
        assert!(!h.update(stale, b"nope").unwrap());
        assert_eq!(h.get(owner).unwrap().unwrap().len(), 4000);
    }

    #[test]
    fn fuzz_vs_model() {
        for seed in 0..24 {
            let bp = fresh_pool(6);
            let h = HeapFile::open(&bp).unwrap();
            let mut rng = Rng::seed(seed);
            let mut model: HashMap<Rid, Vec<u8>> = HashMap::new();

            for _ in 0..4000 {
                let live: Vec<Rid> = model.keys().copied().collect();
                match rng.below(4) {
                    0 => {
                        let len = rng.range(8, 300) as usize;
                        let mut rec = vec![0u8; len];
                        rng.fill_bytes(&mut rec);
                        let rid = h.insert(&rec).unwrap();
                        assert!(!model.contains_key(&rid), "insert returned a live RID");
                        model.insert(rid, rec);
                    }
                    1 => {
                        if let Some(&rid) = pick(&mut rng, &live) {
                            assert!(h.delete(rid).unwrap());
                            model.remove(&rid);
                        }
                    }
                    2 => {
                        if let Some(&rid) = pick(&mut rng, &live) {
                            let len = rng.range(8, 4500) as usize;
                            let mut rec = vec![0u8; len];
                            rng.fill_bytes(&mut rec);
                            match h.update(rid, &rec) {
                                Ok(true) => {
                                    model.insert(rid, rec);
                                }
                                Ok(false) => unreachable!(),
                                Err(HeapError::ForwardWontFit(_)) => {}
                                Err(e) => panic!("seed {seed}: {e}"),
                            }
                        }
                    }
                    _ => {
                        if let Some(&rid) = pick(&mut rng, &live) {
                            assert_eq!(
                                h.get(rid).unwrap().as_deref(),
                                Some(model[&rid].as_slice())
                            );
                        }
                    }
                }
            }

            for (rid, rec) in &model {
                assert_eq!(
                    h.get(*rid).unwrap().as_deref(),
                    Some(rec.as_slice()),
                    "seed {seed}"
                );
            }
            let scanned: HashMap<Rid, Vec<u8>> = h.scan().unwrap().into_iter().collect();
            assert_eq!(scanned, model, "seed {seed}: scan diverged from model");
        }
    }

    fn pick<'r>(rng: &mut Rng, xs: &'r [Rid]) -> Option<&'r Rid> {
        rng.choose_index(xs.len()).map(|i| &xs[i])
    }
}
