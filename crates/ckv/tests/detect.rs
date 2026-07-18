//! Positive corruption-detection test: the crate's headline guarantee is that
//! "corruption is never silent", and this asserts it *directly* rather than hoping
//! a fault-injected crash happens to produce a readable-but-torn page.
//!
//! An adversarial audit showed the crash campaign was structurally unfalsifiable
//! for this property — it passed unchanged with checksum verification removed,
//! because across 24 seeds the adversary mostly *dropped* pending writes (leaving
//! the intact checkpointed page) rather than mixing sectors into a page that
//! decodes as plausible data. Strengthening that test's assertions helped but did
//! not make it falsifiable. So corruption is injected deterministically here
//! instead: flip one byte in a bucket page's entry area on disk, and every read
//! path must report `Corrupt` rather than return the damaged bytes as data.

use keel_ckv::{KvError, PagedKv};
use keel_page::PAGE_SIZE;
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

const BUCKETS: u32 = 4;
const FRAMES: usize = 4;
const KEYS: u64 = 60;

fn bucket_of(k: u64, buckets: u32) -> u32 {
    (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32 % buckets
}

#[test]
fn a_flipped_byte_is_detected_on_every_read_path() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let kv = PagedKv::create(disk.clone(), BUCKETS, FRAMES).unwrap();
        for k in 0..KEYS {
            kv.put(k, 1_000 + k).unwrap();
        }
        kv.checkpoint().unwrap();
    }

    const VICTIM: u32 = 1;
    let mut page = vec![0u8; PAGE_SIZE];
    disk.read_at(&mut page, VICTIM as u64 * PAGE_SIZE as u64)
        .unwrap();
    page[64] ^= 0xFF;
    disk.write_at(&page, VICTIM as u64 * PAGE_SIZE as u64)
        .unwrap();

    let kv = PagedKv::open(disk, BUCKETS, FRAMES);

    assert!(
        matches!(kv.bucket_entries(VICTIM), Err(KvError::Corrupt(b)) if b == VICTIM),
        "bucket_entries returned damaged bytes as valid data"
    );
    assert!(
        !kv.bucket_intact(VICTIM).unwrap(),
        "bucket_intact missed it"
    );

    assert!(
        matches!(kv.total(), Err(KvError::Corrupt(b)) if b == VICTIM),
        "total() silently summed a corrupt bucket"
    );

    let victim_key = (0..KEYS)
        .find(|&k| bucket_of(k, BUCKETS) == VICTIM)
        .expect("some key must hash to the victim bucket");
    assert!(
        matches!(kv.get(victim_key), Err(KvError::Corrupt(b)) if b == VICTIM),
        "get() returned data from a corrupt page"
    );
    assert!(
        matches!(kv.put(victim_key, 7), Err(KvError::Corrupt(b)) if b == VICTIM),
        "put() overwrote a corrupt page and re-sealed it"
    );
    assert!(
        matches!(kv.update(victim_key, 0, |v| v + 1), Err(KvError::Corrupt(b)) if b == VICTIM),
        "update() read-modify-wrote a corrupt page"
    );

    let ok_key = (0..KEYS)
        .find(|&k| bucket_of(k, BUCKETS) != VICTIM)
        .expect("some key must hash elsewhere");
    assert_eq!(kv.get(ok_key).unwrap(), Some(1_000 + ok_key));
}
