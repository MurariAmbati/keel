//! The 8 KB slotted page — KEEL's unit of storage (D4).
//!
//! Every page carries a header `{checksum, pageLSN, flags, type, version,
//! slotCount, freeStart, freeEnd}` and a body organized as a slotted page: a
//! slot array growing forward from the header, tuple bytes packed from the tail,
//! a free gap between them. Two invariants are load-bearing everywhere above:
//!
//!   * **Slot indices are stable.** A slot's index never changes once assigned,
//!     even across compaction. A RID `(page, slot)` is a permanent address; the
//!     heap and every index depend on that (§2.2). Deletes *tombstone* a slot;
//!     they never renumber.
//!   * **Every page self-verifies.** A CRC32 over the page body catches torn
//!     writes and bit-rot on read, which is exactly what makes the crash
//!     campaign able to tell "recovered correctly" from "recovered garbage".
//!
//! This is the sole crate that may use `unsafe` (D11); the implementation here
//! is nonetheless entirely safe — bounds-checked byte accessors, no transmute.
//! The allowance only reserves the boundary for a future zero-copy optimization
//! that would be measured, not assumed.

use std::fmt;

/// Page size in bytes. 8 KB (D4).
pub const PAGE_SIZE: usize = 8192;

/// Fixed page-header size. Layout (little-endian):
/// ```text
///  0..4   checksum   u32   CRC32 of bytes [4, PAGE_SIZE)
///  4..12  page_lsn   u64   LSN of the last log record that touched this page
/// 12..14  flags      u16   FPW-logged bit, etc. (set by higher layers)
/// 14..15  page_type  u8    Meta / Heap / BTreeLeaf / BTreeInternal / FreeList / Overflow
/// 15..16  version    u8    page format version
/// 16..18  slot_count u16   number of slots (live + tombstoned)
/// 18..20  free_start u16   end of the slot array (next-slot offset)
/// 20..22  free_end   u16   start of the tuple heap (grows down from PAGE_SIZE)
/// 22..24  reserved   u16
/// 24..32  extra      u64   generic link (heap: 0; B-tree: sibling/next page)
/// ```
pub const HEADER_SIZE: usize = 32;

/// Size of one slot entry: `{offset: u16, len: u16}`.
pub const SLOT_SIZE: usize = 4;

/// Current page format version.
pub const PAGE_FORMAT_VERSION: u8 = 1;

/// The largest tuple that can fit in an otherwise-empty page.
pub const MAX_TUPLE_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

/// A slot index within a page. Combined with a page id it forms a RID.
pub type SlotId = u16;

const OFF_CHECKSUM: usize = 0;
const OFF_PAGE_LSN: usize = 4;
const OFF_FLAGS: usize = 12;
const OFF_TYPE: usize = 14;
const OFF_VERSION: usize = 15;
const OFF_SLOT_COUNT: usize = 16;
const OFF_FREE_START: usize = 18;
const OFF_FREE_END: usize = 20;
const OFF_EXTRA: usize = 24;

/// What a page holds. Stored in the header so `pageview`/`dbcheck` can interpret
/// a raw page without external context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Meta = 0,
    Heap = 1,
    BTreeLeaf = 2,
    BTreeInternal = 3,
    FreeList = 4,
    Overflow = 5,
}

impl PageType {
    pub fn from_u8(v: u8) -> Option<PageType> {
        Some(match v {
            0 => PageType::Meta,
            1 => PageType::Heap,
            2 => PageType::BTreeLeaf,
            3 => PageType::BTreeInternal,
            4 => PageType::FreeList,
            5 => PageType::Overflow,
            _ => return None,
        })
    }
}

/// Errors from page operations. All are recoverable by the caller (e.g. the
/// heap responds to `PageFull` by allocating another page).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageError {
    /// Not enough room, even after compaction.
    PageFull,
    /// The tuple is larger than any page could hold (needs overflow pages).
    TupleTooLarge,
    /// The slot index is out of range or refers to a tombstone.
    BadSlot,
    /// The header is structurally impossible (used by `verify`).
    Corrupt(&'static str),
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageError::PageFull => write!(f, "page full"),
            PageError::TupleTooLarge => write!(f, "tuple too large for a page"),
            PageError::BadSlot => write!(f, "bad or dead slot"),
            PageError::Corrupt(w) => write!(f, "corrupt page: {w}"),
        }
    }
}

impl std::error::Error for PageError {}

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

/// CRC32/IEEE of a byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

#[inline]
fn rd_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
#[inline]
fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
#[inline]
fn rd_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}
#[inline]
fn wr_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn wr_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn wr_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A slotted page over some byte buffer `B`. `B = &mut [u8]` views a buffer-pool
/// frame in place; `B = PageBuf` owns one for tests and standalone use.
///
/// Read methods require `B: AsRef<[u8]>`; mutating methods additionally require
/// `B: AsMut<[u8]>`. The buffer must be exactly `PAGE_SIZE` bytes.
pub struct SlottedPage<B> {
    buf: B,
}

impl<B: AsRef<[u8]>> SlottedPage<B> {
    /// Wrap an already-initialized page buffer. Panics if not `PAGE_SIZE` long.
    pub fn from_bytes(buf: B) -> Self {
        assert_eq!(
            buf.as_ref().len(),
            PAGE_SIZE,
            "page buffer must be {PAGE_SIZE} bytes"
        );
        Self { buf }
    }

    fn b(&self) -> &[u8] {
        self.buf.as_ref()
    }

    pub fn page_lsn(&self) -> u64 {
        rd_u64(self.b(), OFF_PAGE_LSN)
    }
    pub fn flags(&self) -> u16 {
        rd_u16(self.b(), OFF_FLAGS)
    }
    pub fn page_type(&self) -> Option<PageType> {
        PageType::from_u8(self.b()[OFF_TYPE])
    }
    pub fn format_version(&self) -> u8 {
        self.b()[OFF_VERSION]
    }
    pub fn slot_count(&self) -> u16 {
        rd_u16(self.b(), OFF_SLOT_COUNT)
    }
    pub fn free_start(&self) -> u16 {
        rd_u16(self.b(), OFF_FREE_START)
    }
    pub fn free_end(&self) -> u16 {
        rd_u16(self.b(), OFF_FREE_END)
    }
    pub fn extra(&self) -> u64 {
        rd_u64(self.b(), OFF_EXTRA)
    }
    pub fn stored_checksum(&self) -> u32 {
        rd_u32(self.b(), OFF_CHECKSUM)
    }

    /// The checksum this page's bytes currently imply (body = [4, PAGE_SIZE)).
    pub fn computed_checksum(&self) -> u32 {
        crc32(&self.b()[OFF_PAGE_LSN..])
    }

    /// Does the stored checksum match the body? A `false` here is how a torn
    /// write is detected on read.
    pub fn verify_checksum(&self) -> bool {
        self.stored_checksum() == self.computed_checksum()
    }

    fn read_slot(&self, slot: SlotId) -> (u16, u16) {
        let base = HEADER_SIZE + slot as usize * SLOT_SIZE;
        (rd_u16(self.b(), base), rd_u16(self.b(), base + 2))
    }

    /// Bytes of a live tuple, or `None` if the slot is out of range or dead.
    pub fn get(&self, slot: SlotId) -> Option<&[u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (off, len) = self.read_slot(slot);
        if off == 0 {
            return None;
        }
        Some(&self.b()[off as usize..off as usize + len as usize])
    }

    /// Is this slot a live tuple?
    pub fn is_live(&self, slot: SlotId) -> bool {
        slot < self.slot_count() && self.read_slot(slot).0 != 0
    }

    /// Count of live (non-tombstone) tuples.
    pub fn live_count(&self) -> u16 {
        (0..self.slot_count())
            .filter(|&s| self.read_slot(s).0 != 0)
            .count() as u16
    }

    /// Contiguous free bytes between the slot array and the tuple heap.
    pub fn free_space(&self) -> usize {
        self.free_end() as usize - self.free_start() as usize
    }

    /// Contiguous free bytes that *would* be available after a compaction —
    /// i.e. current free plus space held by tombstones. The heap's free-space
    /// map tracks this so deleted space becomes reusable without eagerly
    /// compacting on every delete.
    pub fn compactable_free(&self) -> usize {
        let mut sum_live = 0usize;
        for s in 0..self.slot_count() {
            let (off, len) = self.read_slot(s);
            if off != 0 {
                sum_live += len as usize;
            }
        }
        PAGE_SIZE - HEADER_SIZE - self.slot_count() as usize * SLOT_SIZE - sum_live
    }

    /// Iterate `(slot, bytes)` over live tuples in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (SlotId, &[u8])> {
        (0..self.slot_count()).filter_map(move |s| self.get(s).map(|d| (s, d)))
    }

    /// Structural self-check for `dbcheck`: header fields are consistent and no
    /// slot points outside the tuple heap or overlaps the slot array.
    pub fn validate_structure(&self) -> Result<(), PageError> {
        let n = self.slot_count() as usize;
        let fs = self.free_start() as usize;
        let fe = self.free_end() as usize;
        if fs != HEADER_SIZE + n * SLOT_SIZE {
            return Err(PageError::Corrupt("free_start != end of slot array"));
        }
        if fe > PAGE_SIZE || fs > fe {
            return Err(PageError::Corrupt("free window out of range"));
        }
        if self.format_version() != PAGE_FORMAT_VERSION {
            return Err(PageError::Corrupt("unknown page format version"));
        }
        if self.page_type().is_none() {
            return Err(PageError::Corrupt("unknown page type"));
        }
        for s in 0..n as SlotId {
            let (off, len) = self.read_slot(s);
            if off == 0 {
                continue;
            }
            let o = off as usize;
            let l = len as usize;
            if o < fe || o + l > PAGE_SIZE {
                return Err(PageError::Corrupt("tuple outside heap region"));
            }
        }
        Ok(())
    }

    /// Borrow the raw bytes (for the pager to write to disk).
    pub fn as_bytes(&self) -> &[u8] {
        self.b()
    }
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> SlottedPage<B> {
    /// Format a fresh, empty page of the given type. Zeros everything, sets the
    /// header, and stamps a valid checksum.
    pub fn init(mut buf: B, page_type: PageType) -> Self {
        {
            let b = buf.as_mut();
            assert_eq!(b.len(), PAGE_SIZE, "page buffer must be {PAGE_SIZE} bytes");
            b.iter_mut().for_each(|x| *x = 0);
            b[OFF_TYPE] = page_type as u8;
            b[OFF_VERSION] = PAGE_FORMAT_VERSION;
            wr_u16(b, OFF_SLOT_COUNT, 0);
            wr_u16(b, OFF_FREE_START, HEADER_SIZE as u16);
            wr_u16(b, OFF_FREE_END, PAGE_SIZE as u16);
        }
        let mut p = Self { buf };
        p.recompute_checksum();
        p
    }

    fn m(&mut self) -> &mut [u8] {
        self.buf.as_mut()
    }

    pub fn set_page_lsn(&mut self, lsn: u64) {
        wr_u64(self.m(), OFF_PAGE_LSN, lsn);
    }
    pub fn set_flags(&mut self, flags: u16) {
        wr_u16(self.m(), OFF_FLAGS, flags);
    }
    pub fn set_extra(&mut self, v: u64) {
        wr_u64(self.m(), OFF_EXTRA, v);
    }

    fn set_slot_count(&mut self, n: u16) {
        wr_u16(self.m(), OFF_SLOT_COUNT, n);
    }
    fn set_free_start(&mut self, v: u16) {
        wr_u16(self.m(), OFF_FREE_START, v);
    }
    fn set_free_end(&mut self, v: u16) {
        wr_u16(self.m(), OFF_FREE_END, v);
    }
    fn write_slot(&mut self, slot: SlotId, off: u16, len: u16) {
        let base = HEADER_SIZE + slot as usize * SLOT_SIZE;
        wr_u16(self.m(), base, off);
        wr_u16(self.m(), base + 2, len);
    }

    /// Recompute and store the page checksum. The pager calls this immediately
    /// before writing a page to disk.
    pub fn recompute_checksum(&mut self) {
        let ck = crc32(&self.b()[OFF_PAGE_LSN..]);
        wr_u32(self.m(), OFF_CHECKSUM, ck);
    }

    fn first_dead_slot(&self) -> Option<SlotId> {
        (0..self.slot_count()).find(|&s| self.read_slot(s).0 == 0)
    }

    /// Insert a tuple, returning its (stable) slot id. Reuses a tombstoned slot
    /// when one exists, so RIDs are recycled without growing the slot array.
    pub fn insert(&mut self, data: &[u8]) -> Result<SlotId, PageError> {
        if data.len() > MAX_TUPLE_SIZE {
            return Err(PageError::TupleTooLarge);
        }
        let reuse = self.first_dead_slot();
        let overhead = if reuse.is_some() { 0 } else { SLOT_SIZE };
        if self.free_space() < data.len() + overhead {
            self.compact();
            if self.free_space() < data.len() + overhead {
                return Err(PageError::PageFull);
            }
        }
        let off = self.free_end() as usize - data.len();
        self.m()[off..off + data.len()].copy_from_slice(data);
        self.set_free_end(off as u16);
        let slot = match reuse {
            Some(s) => s,
            None => {
                let s = self.slot_count();
                self.set_slot_count(s + 1);
                let fs = self.free_start() + SLOT_SIZE as u16;
                self.set_free_start(fs);
                s
            }
        };
        self.write_slot(slot, off as u16, data.len() as u16);
        Ok(slot)
    }

    /// Replace the contents of a live slot. Shrinks in place; grows by
    /// relocation, and is transactional on failure — if the new value doesn't
    /// fit even after compaction, the old value is left intact and `PageFull`
    /// is returned so the caller can forward the tuple to another page (§2.2).
    pub fn set(&mut self, slot: SlotId, data: &[u8]) -> Result<(), PageError> {
        if data.len() > MAX_TUPLE_SIZE {
            return Err(PageError::TupleTooLarge);
        }
        if slot >= self.slot_count() {
            return Err(PageError::BadSlot);
        }
        let (off, len) = self.read_slot(slot);
        if off == 0 {
            return Err(PageError::BadSlot);
        }
        if data.len() <= len as usize {
            let o = off as usize;
            self.m()[o..o + data.len()].copy_from_slice(data);
            self.write_slot(slot, off, data.len() as u16);
            return Ok(());
        }
        let old = self.get(slot).unwrap().to_vec();
        self.write_slot(slot, 0, 0);
        self.compact();
        if self.free_space() >= data.len() {
            let o = self.free_end() as usize - data.len();
            self.m()[o..o + data.len()].copy_from_slice(data);
            self.set_free_end(o as u16);
            self.write_slot(slot, o as u16, data.len() as u16);
            Ok(())
        } else {
            let o = self.free_end() as usize - old.len();
            self.m()[o..o + old.len()].copy_from_slice(&old);
            self.set_free_end(o as u16);
            self.write_slot(slot, o as u16, old.len() as u16);
            Err(PageError::PageFull)
        }
    }

    /// Tombstone a slot. The tuple bytes are reclaimed on the next compaction.
    /// Returns whether a live tuple was there.
    pub fn delete(&mut self, slot: SlotId) -> bool {
        if slot >= self.slot_count() {
            return false;
        }
        if self.read_slot(slot).0 == 0 {
            return false;
        }
        self.write_slot(slot, 0, 0);
        true
    }

    /// Reclaim dead space by repacking live tuples against the page tail. Slot
    /// indices are preserved (RIDs survive); tombstones remain tombstones.
    pub fn compact(&mut self) {
        let n = self.slot_count();
        let mut scratch = [0u8; PAGE_SIZE];
        let mut moves: Vec<(SlotId, u16, u16)> = Vec::new();
        let mut write_end = PAGE_SIZE;
        for s in 0..n {
            let (off, len) = self.read_slot(s);
            if off == 0 {
                continue;
            }
            let l = len as usize;
            let new_off = write_end - l;
            scratch[new_off..write_end].copy_from_slice(&self.b()[off as usize..off as usize + l]);
            moves.push((s, new_off as u16, len));
            write_end = new_off;
        }
        self.m()[write_end..PAGE_SIZE].copy_from_slice(&scratch[write_end..PAGE_SIZE]);
        for (s, new_off, len) in moves {
            self.write_slot(s, new_off, len);
        }
        self.set_free_end(write_end as u16);
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.m()
    }
}

/// An owned, heap-allocated page buffer (`Box<[u8; PAGE_SIZE]>` under the hood).
#[derive(Clone)]
pub struct PageBuf(Box<[u8]>);

impl PageBuf {
    /// A zeroed buffer (not a valid page until `init`/loaded).
    pub fn zeroed() -> Self {
        PageBuf(vec![0u8; PAGE_SIZE].into_boxed_slice())
    }

    /// A freshly formatted page of the given type.
    pub fn new(page_type: PageType) -> SlottedPage<PageBuf> {
        SlottedPage::init(PageBuf::zeroed(), page_type)
    }
}

impl AsRef<[u8]> for PageBuf {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
impl AsMut<[u8]> for PageBuf {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

/// Raw header access for page bodies that are *not* slotted (e.g. B-tree nodes),
/// which need the shared header (checksum, LSN, type, sibling link) but define
/// their own body. The checksum convention is identical — CRC32 over
/// `[4, PAGE_SIZE)` — so the buffer pool, `dbcheck`, and the WAL treat every page
/// uniformly regardless of body layout.
pub mod raw {
    use super::*;

    /// Bytes available for a non-slotted body, after the shared header.
    pub const BODY_CAPACITY: usize = PAGE_SIZE - HEADER_SIZE;

    /// Zero a buffer and stamp a minimal header (type + version). The caller
    /// fills the body and lets the pager checksum it on write.
    pub fn init_header(buf: &mut [u8], page_type: PageType) {
        assert_eq!(
            buf.len(),
            PAGE_SIZE,
            "page buffer must be {PAGE_SIZE} bytes"
        );
        buf.iter_mut().for_each(|x| *x = 0);
        buf[OFF_TYPE] = page_type as u8;
        buf[OFF_VERSION] = PAGE_FORMAT_VERSION;
    }

    pub fn page_type(buf: &[u8]) -> Option<PageType> {
        PageType::from_u8(buf[OFF_TYPE])
    }
    pub fn set_page_type(buf: &mut [u8], t: PageType) {
        buf[OFF_TYPE] = t as u8;
    }
    pub fn page_lsn(buf: &[u8]) -> u64 {
        rd_u64(buf, OFF_PAGE_LSN)
    }
    pub fn set_page_lsn(buf: &mut [u8], v: u64) {
        wr_u64(buf, OFF_PAGE_LSN, v);
    }
    /// The generic header link word — B-tree nodes pack sibling pointers here.
    pub fn extra(buf: &[u8]) -> u64 {
        rd_u64(buf, OFF_EXTRA)
    }
    pub fn set_extra(buf: &mut [u8], v: u64) {
        wr_u64(buf, OFF_EXTRA, v);
    }

    /// The body region `[HEADER_SIZE, PAGE_SIZE)`.
    pub fn body(buf: &[u8]) -> &[u8] {
        &buf[HEADER_SIZE..]
    }
    pub fn body_mut(buf: &mut [u8]) -> &mut [u8] {
        &mut buf[HEADER_SIZE..]
    }

    /// Compute and store the checksum (same convention as slotted pages).
    pub fn stamp_checksum(buf: &mut [u8]) {
        let ck = crc32(&buf[OFF_PAGE_LSN..]);
        wr_u32(buf, OFF_CHECKSUM, ck);
    }
    pub fn verify_checksum(buf: &[u8]) -> bool {
        rd_u32(buf, OFF_CHECKSUM) == crc32(&buf[OFF_PAGE_LSN..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_rng::Rng;
    use std::collections::HashMap;

    #[test]
    fn fresh_page_is_valid_and_empty() {
        let p = PageBuf::new(PageType::Heap);
        assert_eq!(p.page_type(), Some(PageType::Heap));
        assert_eq!(p.slot_count(), 0);
        assert_eq!(p.live_count(), 0);
        assert!(p.verify_checksum());
        assert!(p.validate_structure().is_ok());
        assert_eq!(p.free_space(), PAGE_SIZE - HEADER_SIZE);
    }

    #[test]
    fn insert_get_delete() {
        let mut p = PageBuf::new(PageType::Heap);
        let a = p.insert(b"alpha").unwrap();
        let b = p.insert(b"beta").unwrap();
        assert_eq!(p.get(a), Some(&b"alpha"[..]));
        assert_eq!(p.get(b), Some(&b"beta"[..]));
        assert_eq!(p.live_count(), 2);
        assert!(p.delete(a));
        assert_eq!(p.get(a), None);
        assert!(!p.delete(a));
        assert_eq!(p.live_count(), 1);
        assert_eq!(p.get(b), Some(&b"beta"[..]));
    }

    #[test]
    fn deleted_slot_is_reused() {
        let mut p = PageBuf::new(PageType::Heap);
        let a = p.insert(b"aaa").unwrap();
        let _b = p.insert(b"bbb").unwrap();
        p.delete(a);
        let c = p.insert(b"ccc").unwrap();
        assert_eq!(c, a, "insert should recycle the tombstoned slot id");
        assert_eq!(p.slot_count(), 2);
    }

    #[test]
    fn checksum_catches_corruption() {
        let mut p = PageBuf::new(PageType::Heap);
        p.insert(b"payload").unwrap();
        p.recompute_checksum();
        assert!(p.verify_checksum());
        let last = PAGE_SIZE - 1;
        p.as_bytes_mut()[last] ^= 0xFF;
        assert!(
            !p.verify_checksum(),
            "checksum must detect the flipped byte"
        );
    }

    #[test]
    fn set_shrink_in_place_and_grow_relocates() {
        let mut p = PageBuf::new(PageType::Heap);
        let s = p.insert(b"aaaaaaaaaa").unwrap();
        let end_after_insert = p.free_end();
        p.set(s, b"bb").unwrap();
        assert_eq!(p.get(s), Some(&b"bb"[..]));
        assert_eq!(
            p.free_end(),
            end_after_insert,
            "shrink must not move free_end"
        );
        p.set(s, b"cccccccccccccccc").unwrap();
        assert_eq!(p.get(s), Some(&b"cccccccccccccccc"[..]));
    }

    #[test]
    fn set_grow_on_full_page_preserves_old_value() {
        let mut p = PageBuf::new(PageType::Heap);
        let a = p.insert(&vec![1u8; 4000]).unwrap();
        let b = p.insert(&vec![2u8; 4000]).unwrap();
        assert_eq!(p.set(a, &vec![9u8; 5000]), Err(PageError::PageFull));
        assert_eq!(
            p.get(a),
            Some(&vec![1u8; 4000][..]),
            "old value must be intact after failed grow"
        );
        assert_eq!(
            p.get(b),
            Some(&vec![2u8; 4000][..]),
            "the other tuple must be untouched"
        );
    }

    #[test]
    fn compaction_reclaims_and_keeps_slot_ids() {
        let mut p = PageBuf::new(PageType::Heap);
        let mut ids = Vec::new();
        for i in 0..50 {
            ids.push(p.insert(format!("tuple-{i:04}").as_bytes()).unwrap());
        }
        for i in (0..50).step_by(2) {
            p.delete(ids[i]);
        }
        let free_before = p.free_space();
        p.compact();
        assert!(
            p.free_space() > free_before,
            "compaction should recover dead space"
        );
        for i in (1..50).step_by(2) {
            assert_eq!(p.get(ids[i]), Some(format!("tuple-{i:04}").as_bytes()));
        }
        assert!(p.validate_structure().is_ok());
    }

    #[test]
    fn page_fills_and_reports_full() {
        let mut p = PageBuf::new(PageType::Heap);
        let mut n = 0;
        loop {
            match p.insert(&[0xAB; 64]) {
                Ok(_) => n += 1,
                Err(PageError::PageFull) => break,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(n > 0);
        assert!(p.validate_structure().is_ok());
        assert!(
            p.verify_checksum() || {
                p.recompute_checksum();
                p.verify_checksum()
            }
        );
    }

    #[test]
    fn fuzz_vs_model() {
        for seed in 0..40 {
            let mut rng = Rng::seed(seed);
            let mut p = PageBuf::new(PageType::Heap);
            let mut model: HashMap<SlotId, Vec<u8>> = HashMap::new();
            for _ in 0..2000 {
                match rng.below(4) {
                    0 => {
                        let len = rng.range(1, 200) as usize;
                        let mut data = vec![0u8; len];
                        rng.fill_bytes(&mut data);
                        if let Ok(s) = p.insert(&data) {
                            assert!(!model.contains_key(&s) || model[&s].is_empty());
                            model.insert(s, data);
                        }
                    }
                    1 => {
                        if let Some(&s) = live_keys(&model, &p).first() {
                            assert!(p.delete(s));
                            model.remove(&s);
                        }
                    }
                    2 => {
                        let live = live_keys(&model, &p);
                        if let Some(&s) = pick(&mut rng, &live) {
                            let len = rng.range(1, 300) as usize;
                            let mut data = vec![0u8; len];
                            rng.fill_bytes(&mut data);
                            match p.set(s, &data) {
                                Ok(()) => {
                                    model.insert(s, data);
                                }
                                Err(PageError::PageFull) => {}
                                Err(e) => panic!("unexpected {e:?}"),
                            }
                        }
                    }
                    _ => {
                        p.compact();
                    }
                }
                for (&s, v) in &model {
                    assert_eq!(p.get(s), Some(v.as_slice()), "seed {seed} slot {s}");
                }
                assert_eq!(p.live_count() as usize, model.len());
                p.validate_structure().unwrap();
            }
        }
    }

    fn live_keys(model: &HashMap<SlotId, Vec<u8>>, _p: &SlottedPage<PageBuf>) -> Vec<SlotId> {
        let mut v: Vec<SlotId> = model.keys().copied().collect();
        v.sort_unstable();
        v
    }

    fn pick<'a>(rng: &mut Rng, xs: &'a [SlotId]) -> Option<&'a SlotId> {
        rng.choose_index(xs.len()).map(|i| &xs[i])
    }
}
