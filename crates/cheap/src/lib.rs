use std::sync::{Arc, Mutex};

use keel_cbuffer::{CacheError, PageCache, PageId};
use keel_page::{PageError, PageType, SlotId, SlottedPage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rid {
    pub page: PageId,
    pub slot: SlotId,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct HeapReport {
    pub pages: u32,
    pub bad_checksum: Vec<PageId>,
    pub foreign_pages: Vec<PageId>,
    pub live_records: u64,
}

impl HeapReport {
    pub fn is_clean(&self) -> bool {
        self.bad_checksum.is_empty() && self.foreign_pages.is_empty()
    }
}

#[derive(Debug)]
pub enum HeapError {
    Cache(CacheError),
    TooLarge,
}

impl From<CacheError> for HeapError {
    fn from(e: CacheError) -> Self {
        HeapError::Cache(e)
    }
}

pub struct Heap {
    cache: Arc<PageCache>,
    hint: Mutex<Option<PageId>>,
}

impl Heap {
    pub fn open(cache: Arc<PageCache>) -> Self {
        let n = cache.page_count();
        let hint = if n == 0 { None } else { Some(n - 1) };
        Heap {
            cache,
            hint: Mutex::new(hint),
        }
    }

    pub fn insert(&self, rec: &[u8]) -> Result<Rid, HeapError> {
        loop {
            let cur = *self.hint.lock().expect("heap hint poisoned");
            if let Some(pid) = cur {
                let p = self.cache.fetch(pid)?;
                {
                    let mut b = p.write();
                    let mut sp = SlottedPage::from_bytes(&mut b[..]);
                    let outcome = sp.insert(rec);
                    sp.recompute_checksum();
                    match outcome {
                        Ok(slot) => return Ok(Rid { page: pid, slot }),
                        Err(PageError::TupleTooLarge) => return Err(HeapError::TooLarge),
                        Err(_) => {}
                    }
                }
            }
            let mut h = self.hint.lock().expect("heap hint poisoned");
            if *h == cur {
                let np = self.cache.new_page()?;
                let npid = np.pid();
                {
                    let mut nb = np.write();
                    let mut sp = SlottedPage::init(&mut nb[..], PageType::Heap);
                    sp.recompute_checksum();
                }
                *h = Some(npid);
            }
        }
    }

    pub fn get(&self, rid: Rid) -> Result<Option<Vec<u8>>, HeapError> {
        let p = self.cache.fetch(rid.page)?;
        let b = p.read();
        let sp = SlottedPage::from_bytes(&b[..]);
        Ok(sp.get(rid.slot).map(|s| s.to_vec()))
    }

    pub fn delete(&self, rid: Rid) -> Result<bool, HeapError> {
        let p = self.cache.fetch(rid.page)?;
        let mut b = p.write();
        let mut sp = SlottedPage::from_bytes(&mut b[..]);
        let existed = sp.delete(rid.slot);
        if existed {
            sp.recompute_checksum();
        }
        Ok(existed)
    }

    pub fn scan(&self) -> Result<Vec<(Rid, Vec<u8>)>, HeapError> {
        let n = self.cache.page_count();
        let mut out = Vec::new();
        for page in 0..n {
            let p = self.cache.fetch(page)?;
            let b = p.read();
            let sp = SlottedPage::from_bytes(&b[..]);
            if sp.page_type() != Some(PageType::Heap) {
                continue;
            }
            for slot in 0..sp.slot_count() {
                if let Some(rec) = sp.get(slot) {
                    out.push((
                        Rid {
                            page,
                            slot: slot as SlotId,
                        },
                        rec.to_vec(),
                    ));
                }
            }
        }
        Ok(out)
    }

    pub fn verify(&self) -> Result<HeapReport, HeapError> {
        let mut rep = HeapReport::default();
        for page in 0..self.cache.page_count() {
            let p = self.cache.fetch(page)?;
            let b = p.read();
            let sp = SlottedPage::from_bytes(&b[..]);
            rep.pages += 1;
            if !sp.verify_checksum() {
                rep.bad_checksum.push(page);
                continue;
            }
            if sp.page_type() != Some(PageType::Heap) {
                rep.foreign_pages.push(page);
                continue;
            }
            for slot in 0..sp.slot_count() {
                if sp.get(slot).is_some() {
                    rep.live_records += 1;
                }
            }
        }
        Ok(rep)
    }

    pub fn checkpoint(&self) -> std::io::Result<()> {
        self.cache.checkpoint()
    }

    pub fn seal(&self) {
        *self.hint.lock().expect("heap hint poisoned") = None;
    }
}
