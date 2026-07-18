//! A durable, concurrent hash-bucket key→value store over
//! [`cbuffer::PageCache`] — the reference for how a real heap or B-tree will use
//! the concurrent buffer.
//!
//! Each bucket is one page; a `put`/`get`/`update` on a bucket runs under that
//! page's reader/writer latch (via `cbuffer`'s `PageRef`), so the whole store
//! inherits the buffer's per-page concurrency, its `checkpoint` durability, and
//! its crash-campaign guarantees for free. That is the point: the concurrency and
//! durability were proven once, in `cbuffer`, and a data structure layered on top
//! composes them rather than re-deriving them. `update` holds the bucket's write
//! latch across the read-modify-write, so concurrent updates to the same key can
//! never lose an increment.
//!
//! Every bucket page carries a CRC over its payload, so a torn page (from a crash
//! mid-write) is *detected*, never silently read as valid data. Keys and values
//! are `u64` and a bucket holds a flat, unsorted array — this demonstrates the
//! *integration pattern*, not the engine's real record layout (that is the heap's
//! job). Overflow is an honest error, never a silent drop.

use std::sync::Arc;

use keel_cbuffer::{CacheError, PageCache};
use keel_page::{crc32, PAGE_SIZE};
use keel_vfs::BlockFile;

pub type Key = u64;
pub type Val = u64;

const HEADER: usize = 4;
const SLOT: usize = 16;
const CRC_AT: usize = PAGE_SIZE - 4;
/// Entries per bucket page.
pub const BUCKET_CAP: usize = (CRC_AT - HEADER) / SLOT;

#[derive(Debug)]
pub enum KvError {
    Cache(CacheError),
    /// A bucket page is full — the store's honest capacity signal.
    BucketFull(u32),
    /// A bucket page failed its checksum — a torn page from a crash.
    Corrupt(u32),
}
impl From<CacheError> for KvError {
    fn from(e: CacheError) -> Self {
        KvError::Cache(e)
    }
}
impl std::fmt::Display for KvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvError::Cache(e) => write!(f, "cache: {e}"),
            KvError::BucketFull(b) => write!(f, "bucket {b} full"),
            KvError::Corrupt(b) => write!(f, "bucket {b} failed checksum"),
        }
    }
}
impl std::error::Error for KvError {}

pub type Result<T> = std::result::Result<T, KvError>;

fn read_count(buf: &[u8]) -> usize {
    u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize
}
fn write_count(buf: &mut [u8], n: usize) {
    buf[0..4].copy_from_slice(&(n as u32).to_le_bytes());
}
fn entry(buf: &[u8], i: usize) -> (Key, Val) {
    let o = HEADER + i * SLOT;
    let k = u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    let v = u64::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap());
    (k, v)
}
fn set_entry(buf: &mut [u8], i: usize, k: Key, v: Val) {
    let o = HEADER + i * SLOT;
    buf[o..o + 8].copy_from_slice(&k.to_le_bytes());
    buf[o + 8..o + 16].copy_from_slice(&v.to_le_bytes());
}
fn find(buf: &[u8], k: Key) -> Option<usize> {
    (0..read_count(buf)).find(|&i| entry(buf, i).0 == k)
}
/// Stamp the trailing CRC over the payload. Called before releasing any write.
fn seal(buf: &mut [u8]) {
    let c = crc32(&buf[0..CRC_AT]);
    buf[CRC_AT..].copy_from_slice(&c.to_le_bytes());
}
/// Whether a bucket page's CRC matches its payload.
fn intact(buf: &[u8]) -> bool {
    crc32(&buf[0..CRC_AT]) == u32::from_le_bytes(buf[CRC_AT..].try_into().unwrap())
}

/// A hash-bucket KV over a fixed number of bucket pages.
pub struct PagedKv {
    cache: PageCache,
    buckets: u32,
}

impl PagedKv {
    /// Create a store of `buckets` empty buckets on `file` (initialized with that
    /// many sealed, empty pages and synced), cached in `frames` frames.
    pub fn create(file: Arc<dyn BlockFile>, buckets: u32, frames: usize) -> Result<Self> {
        assert!(buckets > 0 && frames > 0);
        let mut page = vec![0u8; PAGE_SIZE];
        write_count(&mut page, 0);
        seal(&mut page);
        for b in 0..buckets {
            file.write_at(&page, b as u64 * PAGE_SIZE as u64)
                .map_err(|e| KvError::Cache(CacheError::Io(e)))?;
        }
        file.sync().map_err(|e| KvError::Cache(CacheError::Io(e)))?;
        Ok(Self {
            cache: PageCache::open(file, frames),
            buckets,
        })
    }

    /// Re-open an existing store whose file already holds `buckets` bucket pages.
    pub fn open(file: Arc<dyn BlockFile>, buckets: u32, frames: usize) -> Self {
        Self {
            cache: PageCache::open(file, frames),
            buckets,
        }
    }

    fn bucket_of(&self, k: Key) -> u32 {
        (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32 % self.buckets
    }

    /// Look up `k`.
    pub fn get(&self, k: Key) -> Result<Option<Val>> {
        let bucket = self.bucket_of(k);
        let p = self.cache.fetch(bucket)?;
        let b = p.read();
        if !intact(&b) {
            return Err(KvError::Corrupt(bucket));
        }
        Ok(find(&b, k).map(|i| entry(&b, i).1))
    }

    /// Insert or overwrite `k = v`.
    pub fn put(&self, k: Key, v: Val) -> Result<()> {
        let bucket = self.bucket_of(k);
        let p = self.cache.fetch(bucket)?;
        let mut b = p.write();
        if !intact(&b) {
            return Err(KvError::Corrupt(bucket));
        }
        if let Some(i) = find(&b, k) {
            set_entry(&mut b, i, k, v);
            seal(&mut b);
            return Ok(());
        }
        let n = read_count(&b);
        if n >= BUCKET_CAP {
            return Err(KvError::BucketFull(bucket));
        }
        set_entry(&mut b, n, k, v);
        write_count(&mut b, n + 1);
        seal(&mut b);
        Ok(())
    }

    /// Atomic read-modify-write under the bucket's write latch, so concurrent
    /// updates to `k` never lose one.
    pub fn update(&self, k: Key, default: Val, f: impl FnOnce(Val) -> Val) -> Result<()> {
        let bucket = self.bucket_of(k);
        let p = self.cache.fetch(bucket)?;
        let mut b = p.write();
        if !intact(&b) {
            return Err(KvError::Corrupt(bucket));
        }
        if let Some(i) = find(&b, k) {
            let (_, v) = entry(&b, i);
            set_entry(&mut b, i, k, f(v));
            seal(&mut b);
            return Ok(());
        }
        let n = read_count(&b);
        if n >= BUCKET_CAP {
            return Err(KvError::BucketFull(bucket));
        }
        set_entry(&mut b, n, k, f(default));
        write_count(&mut b, n + 1);
        seal(&mut b);
        Ok(())
    }

    /// Flush every dirty bucket and `sync` — the durability barrier.
    pub fn checkpoint(&self) -> Result<()> {
        self.cache
            .checkpoint()
            .map_err(|e| KvError::Cache(CacheError::Io(e)))
    }

    pub fn buckets(&self) -> u32 {
        self.buckets
    }

    /// Sum of every value across every bucket — for conservation assertions.
    pub fn total(&self) -> Result<u128> {
        let mut sum = 0u128;
        for bkt in 0..self.buckets {
            let p = self.cache.fetch(bkt)?;
            let b = p.read();
            if !intact(&b) {
                return Err(KvError::Corrupt(bkt));
            }
            for i in 0..read_count(&b) {
                sum += entry(&b, i).1 as u128;
            }
        }
        Ok(sum)
    }

    /// Whether bucket `bkt`'s page passes its checksum (for crash inspection).
    pub fn bucket_intact(&self, bkt: u32) -> Result<bool> {
        let p = self.cache.fetch(bkt)?;
        let b = p.read();
        Ok(intact(&b))
    }

    /// The `(key, value)` entries of bucket `bkt`, or `Corrupt` if it is torn.
    pub fn bucket_entries(&self, bkt: u32) -> Result<Vec<(Key, Val)>> {
        let p = self.cache.fetch(bkt)?;
        let b = p.read();
        if !intact(&b) {
            return Err(KvError::Corrupt(bkt));
        }
        Ok((0..read_count(&b)).map(|i| entry(&b, i)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::MemDisk;

    fn fresh(buckets: u32, frames: usize) -> PagedKv {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        PagedKv::create(disk, buckets, frames).unwrap()
    }

    #[test]
    fn put_get_update_roundtrip() {
        let kv = fresh(8, 4);
        assert_eq!(kv.get(42).unwrap(), None);
        kv.put(42, 100).unwrap();
        assert_eq!(kv.get(42).unwrap(), Some(100));
        kv.put(42, 200).unwrap();
        assert_eq!(kv.get(42).unwrap(), Some(200));
        kv.update(42, 0, |v| v + 5).unwrap();
        assert_eq!(kv.get(42).unwrap(), Some(205));
        kv.update(99, 7, |v| v + 1).unwrap();
        assert_eq!(kv.get(99).unwrap(), Some(8));
    }

    #[test]
    fn survives_reopen_after_checkpoint() {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        {
            let kv = PagedKv::create(disk.clone(), 8, 4).unwrap();
            for k in 0..50u64 {
                kv.put(k, k * 10).unwrap();
            }
            kv.checkpoint().unwrap();
        }
        let kv = PagedKv::open(disk, 8, 4);
        for k in 0..50u64 {
            assert_eq!(kv.get(k).unwrap(), Some(k * 10));
        }
    }

    #[test]
    fn every_bucket_is_intact_after_writes() {
        let kv = fresh(8, 3);
        for k in 0..300u64 {
            kv.put(k, k).unwrap();
        }
        for bkt in 0..kv.buckets() {
            assert!(kv.bucket_intact(bkt).unwrap(), "bucket {bkt} checksum");
        }
    }

    #[test]
    fn differential_vs_hashmap() {
        use std::collections::HashMap;
        let kv = fresh(16, 6);
        let mut model: HashMap<Key, Val> = HashMap::new();
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..5000 {
            let k = next() % 200;
            match next() % 3 {
                0 => {
                    let v = next();
                    kv.put(k, v).unwrap();
                    model.insert(k, v);
                }
                1 => {
                    kv.update(k, 0, |v| v.wrapping_add(1)).unwrap();
                    let e = model.entry(k).or_insert(0);
                    *e = e.wrapping_add(1);
                }
                _ => {
                    assert_eq!(kv.get(k).unwrap(), model.get(&k).copied());
                }
            }
        }
        for k in 0..200u64 {
            assert_eq!(kv.get(k).unwrap(), model.get(&k).copied(), "key {k}");
        }
    }
}
