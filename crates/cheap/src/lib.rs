//! `cheap` — a concurrent, durable **record heap** over `cbuffer`, using the real
//! `keel_page::SlottedPage` on-disk format and stable `(page, slot)` RIDs.
//!
//! This is the write-path integration rung above `ckv`. Where `ckv` was a
//! hash-bucket KV, `cheap` is the *heap itself* — the unordered record store the
//! SQL engine's `SeqScan` / `INSERT` sit on — but running over the **concurrent**
//! buffer: it grows via [`PageCache::new_page`], every page is a checksummed
//! slotted page (so `dbcheck` and the crash campaign apply unchanged), and inserts
//! from many threads never collide, lose a record, or tear a page.
//!
//! ## Concurrency protocol
//! A single `Mutex<Option<PageId>>` holds the *current insert page* hint. An insert
//! reads the hint, fetches that page, takes its **write latch** (from `cbuffer`),
//! and tries `SlottedPage::insert`. On success it stamps the checksum and returns
//! the RID. On `PageFull` it drops the page latch, takes the hint mutex, and — only
//! if no other thread has already advanced the hint past the page it saw full —
//! allocates a fresh page and publishes it as the new hint. A page latch is never
//! held while acquiring the hint mutex, so the two locks have a fixed order and
//! cannot deadlock.
//!
//! ## Durability contract
//! `cheap` has no WAL of its own; its barrier is [`Heap::checkpoint`], inherited
//! from `cbuffer`: **after `checkpoint()` returns, every inserted record survives a
//! power loss byte-for-byte.** Records inserted after the last checkpoint are
//! best-effort — they may or may not survive a crash — but a surviving page is
//! never *silently* corrupt: its `keel_page` checksum catches any torn write.

use std::sync::{Arc, Mutex};

use keel_cbuffer::{CacheError, PageCache, PageId};
use keel_page::{PageError, PageType, SlotId, SlottedPage};

/// A record identifier: the page holding the tuple and its stable slot within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rid {
    pub page: PageId,
    pub slot: SlotId,
}

/// The result of [`Heap::verify`] — this heap's `dbcheck` rule (D12).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HeapReport {
    /// Pages walked.
    pub pages: u32,
    /// Pages whose stored checksum does not match their bytes. On a non-tearing
    /// disk this must always be empty: a page that is merely *stale-checksummed*
    /// still reads back correct data, so no data-equality oracle can see it —
    /// which is exactly how KEEL-0004 stayed hidden behind three green tests.
    pub bad_checksum: Vec<PageId>,
    /// Pages that are resident but not `PageType::Heap`.
    pub foreign_pages: Vec<PageId>,
    /// Total live (non-tombstoned) records seen.
    pub live_records: u64,
}

impl HeapReport {
    pub fn is_clean(&self) -> bool {
        self.bad_checksum.is_empty() && self.foreign_pages.is_empty()
    }
}

/// Failures a heap operation can surface.
#[derive(Debug)]
pub enum HeapError {
    /// The buffer cache could not serve or allocate a page.
    Cache(CacheError),
    /// The record is larger than any page can ever hold.
    TooLarge,
}

impl From<CacheError> for HeapError {
    fn from(e: CacheError) -> Self {
        HeapError::Cache(e)
    }
}

/// A concurrent, durable record heap over a shared [`PageCache`].
pub struct Heap {
    cache: Arc<PageCache>,
    /// The current insert page hint, guarded so fresh-page allocation is
    /// serialized and never races two threads into allocating twice.
    hint: Mutex<Option<PageId>>,
}

impl Heap {
    /// Open a heap over a cache. On reopen the last page becomes the insert
    /// cursor — if it is full the first insert simply allocates a fresh one.
    pub fn open(cache: Arc<PageCache>) -> Self {
        let n = cache.page_count();
        let hint = if n == 0 { None } else { Some(n - 1) };
        Heap {
            cache,
            hint: Mutex::new(hint),
        }
    }

    /// Insert a record, returning its stable RID. Concurrency-safe: many threads
    /// may insert at once; each record lands exactly once and no page tears.
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

    /// Read a record by RID. `None` if the slot is empty (never used or deleted).
    pub fn get(&self, rid: Rid) -> Result<Option<Vec<u8>>, HeapError> {
        let p = self.cache.fetch(rid.page)?;
        let b = p.read();
        let sp = SlottedPage::from_bytes(&b[..]);
        Ok(sp.get(rid.slot).map(|s| s.to_vec()))
    }

    /// Delete a record by RID. Returns whether a live record was removed.
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

    /// Full sequential scan: every live `(RID, record)` across every heap page.
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

    /// Walk every page and check this heap's structural invariants — the `dbcheck`
    /// rule for `cheap` (D12). Every page must be a `Heap` page whose stored
    /// checksum matches its bytes.
    ///
    /// This exists because a *stale checksum* is invisible to any data-equality
    /// oracle: `scan`/`get` return correct records from a page whose checksum no
    /// longer describes it, so the differential, the race test and the crash
    /// campaign all stayed green while KEEL-0004 was live. Call it after a
    /// checkpoint on a non-tearing disk and it must report clean.
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

    /// Force every dirty page to disk — the durability barrier. After this
    /// returns, all inserted records survive a power loss byte-for-byte.
    pub fn checkpoint(&self) -> std::io::Result<()> {
        self.cache.checkpoint()
    }

    /// Seal the insertion frontier: the next insert starts on a *fresh* page, so
    /// every page written so far becomes immutable. Paired with `checkpoint`, this
    /// freezes the checkpointed pages — no later insert can re-dirty them by reusing
    /// their free space — so their records survive any crash byte-for-byte. (It
    /// trades the tail page's remaining free space for that guarantee.)
    pub fn seal(&self) {
        *self.hint.lock().expect("heap hint poisoned") = None;
    }
}
