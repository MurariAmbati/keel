//! Pager differential: the same generic workload driven through the
//! single-threaded `BufferPool` and the concurrent `PageCache` must produce
//! byte-identical results.
//!
//! This is what de-risks the rest of the engine swap. Rather than converting
//! `heap`/`btree` and hoping the two pools agree, the seam itself is held to the
//! established oracle discipline: one workload, two implementations, compared. Any
//! behavioural divergence (page numbering, allocation, eviction, checksum handling,
//! what a reopened pool sees) surfaces here — in a 60-line test — instead of inside
//! a B-tree split at slice 4.
//!
//! Note the workload never stamps a checksum: both pools guarantee it centrally, so
//! agreeing here also confirms `cbuffer`'s new `PageFormat` matches `keel-buffer`'s
//! long-standing behaviour.

use keel_buffer::BufferPool;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_page::{raw, PageType, SlottedPage};
use keel_pager::{Pager, PagerError};
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

const FRAMES: usize = 3;
const PAGES: u32 = 6;
const PER_PAGE: u32 = 4;

/// A summary of everything observable through the seam.
type Summary = Vec<(u32, Option<u8>, Vec<Vec<u8>>)>;

/// The slotted header a *freshly allocated* page must present, captured before any
/// insert touches it. This matters: `SlottedPage::insert` calls `compact()` when
/// space looks short, and compacting a page with no live tuples *repairs* a
/// zero-initialised header into a valid empty one. So a pool that mis-initialises a
/// page is invisible once records are written — the divergence self-heals. Sampling
/// at allocation is what makes this differential able to see it.
type FreshHeaders = Vec<(u16, u16, u16)>;

fn exercise<P: Pager>(p: &P) -> Result<(Summary, FreshHeaders), PagerError> {
    let mut fresh: FreshHeaders = Vec::new();
    for i in 0..PAGES {
        let pid = p.alloc_slotted(PageType::Heap)?;
        let hdr = p.with_page(pid, |b| {
            let sp = SlottedPage::from_bytes(b);
            (sp.slot_count(), sp.free_start(), sp.free_end())
        })?;
        fresh.push(hdr);
        p.with_page_mut(pid, |b| {
            let mut sp = SlottedPage::from_bytes(&mut b[..]);
            for j in 0..PER_PAGE {
                sp.insert(format!("page{i}-rec{j}").as_bytes()).unwrap();
            }
        })?;
    }
    let rawpid = p.alloc_raw(PageType::BTreeLeaf)?;
    p.with_page_mut(rawpid, |b| {
        raw::set_page_lsn(b, 0xBEEF);
    })?;

    p.checkpoint()?;

    let mut out: Summary = Vec::new();
    for pid in 0..p.page_count() {
        let entry = p.with_page(pid, |b| {
            let sp = SlottedPage::from_bytes(b);
            let ty = sp.page_type().map(|t| t as u8);
            let recs: Vec<Vec<u8>> = (0..sp.slot_count())
                .filter_map(|s| sp.get(s).map(|r| r.to_vec()))
                .collect();
            (ty, recs)
        })?;
        out.push((pid, entry.0, entry.1));
    }
    Ok((out, fresh))
}

#[test]
fn both_pools_agree_through_the_pager_seam() {
    let disk_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let pool = BufferPool::open_default(disk_a, FRAMES).unwrap();
    let from_buffer = exercise(&pool).expect("BufferPool workload");

    let disk_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk_b, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    let from_cache = exercise(&cache).expect("PageCache workload");

    assert_eq!(
        from_buffer.0, from_cache.0,
        "the two pools disagree on page contents through the pager seam"
    );
    assert_eq!(
        from_buffer.1, from_cache.1,
        "the two pools hand out differently-initialised fresh pages"
    );
    assert_eq!(from_buffer.0.len() as u32, PAGES + 1);
    assert!(from_buffer
        .0
        .iter()
        .any(|(_, _, recs)| recs.len() == PER_PAGE as usize));
}

#[test]
fn both_pools_survive_reopen_identically() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let pool = BufferPool::open_default(disk.clone(), FRAMES).unwrap();
        exercise(&pool).expect("write via BufferPool");
    }
    let cache = PageCache::open_formatted(
        disk.clone(),
        FRAMES,
        Arc::new(NoWal),
        PageFormat::keel_page(),
    );
    let seen_by_cache: Summary = (0..Pager::page_count(&cache))
        .map(|pid| {
            cache
                .with_page(pid, |b| {
                    let sp = SlottedPage::from_bytes(b);
                    (
                        pid,
                        sp.page_type().map(|t| t as u8),
                        (0..sp.slot_count())
                            .filter_map(|s| sp.get(s).map(|r| r.to_vec()))
                            .collect::<Vec<_>>(),
                    )
                })
                .expect("PageCache must read pages BufferPool wrote")
        })
        .collect();

    let disk2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let pool2 = BufferPool::open_default(disk2, FRAMES).unwrap();
    let reference = exercise(&pool2).expect("reference").0;
    assert_eq!(
        seen_by_cache, reference,
        "PageCache read BufferPool's pages differently than BufferPool wrote them"
    );
}
