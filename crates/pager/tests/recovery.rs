//! Differential over the `RecoveryPager` surface — the primitives `wal::TxnStore`
//! depends on, compared across both pools before `wal` is migrated onto them.
//!
//! Same discipline as `differential.rs` one layer down: rather than flipping `wal`
//! and hoping the recovery semantics match, the semantics are compared directly.
//! Steal policy, DPT bookkeeping, abort-time invalidation, and the fault-tolerant
//! redo fetch are exactly the places where a subtle difference between the two pools
//! would show up as a *recovery* bug — the worst kind to debug, because it only
//! appears after a crash.

use keel_buffer::BufferPool;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_page::{PageType, SlottedPage};
use keel_pager::{Pager, PagerError, RecoveryPager};
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

/// Observable recovery behaviour, in the order `wal` would exercise it.
type Observed = (Vec<(u32, u64)>, Vec<(u32, u64)>, bool, Vec<u8>, u32);

fn exercise_recovery<P: RecoveryPager>(bp: &P) -> Result<Observed, PagerError> {
    let a = bp.alloc_slotted(PageType::Heap)?;
    let b = bp.alloc_slotted(PageType::Heap)?;
    bp.with_page_mut(a, |buf| {
        SlottedPage::from_bytes(buf).insert(b"committed").unwrap();
    })?;
    bp.checkpoint()?;

    bp.note_dirty(a, 500);
    bp.note_dirty(a, 900);
    bp.note_dirty(b, 700);
    let dpt_after_notes = bp.dpt_snapshot();

    bp.invalidate(a);
    let dpt_after_invalidate = bp.dpt_snapshot();

    let far = 40u32;
    let head = bp.with_page_for_redo(far, |buf| buf[..16].to_vec())?;
    let count_after_redo = Pager::page_count(bp);

    bp.set_no_steal();
    let mut refused = false;
    for _ in 0..64 {
        match bp.alloc_slotted(PageType::Heap) {
            Ok(pid) => {
                bp.with_page_mut(pid, |buf| {
                    SlottedPage::from_bytes(buf).insert(b"dirty").unwrap();
                })?;
            }
            Err(PagerError::Exhausted) => {
                refused = true;
                break;
            }
            Err(e) => return Err(e),
        }
    }

    Ok((
        dpt_after_notes,
        dpt_after_invalidate,
        refused,
        head,
        count_after_redo,
    ))
}

#[test]
fn both_pools_agree_on_the_recovery_surface() {
    let disk_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let pool = BufferPool::open_default(disk_a, 4).unwrap();
    let via_buffer = exercise_recovery(&pool).expect("BufferPool recovery workload");

    let disk_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk_b, 4, Arc::new(NoWal), PageFormat::keel_page());
    let via_cache = exercise_recovery(&cache).expect("PageCache recovery workload");

    assert_eq!(via_buffer.0, via_cache.0, "DPT snapshots differ");
    assert_eq!(via_buffer.1, via_cache.1, "DPT after invalidate differs");
    assert_eq!(
        via_buffer.2, via_cache.2,
        "no-steal refusal behaviour differs"
    );
    assert_eq!(
        via_buffer.3, via_cache.3,
        "redo-rebuilt page contents differ"
    );
    assert_eq!(via_buffer.4, via_cache.4, "page_count after redo differs");

    assert_eq!(via_buffer.0.len(), 2, "expected two pages in the DPT");
    assert_eq!(via_buffer.0[0].1, 500, "DPT must keep the OLDEST recLSN");
    assert_eq!(via_buffer.1.len(), 1, "invalidate should have removed one");
    assert!(
        via_buffer.2,
        "no-steal never refused — policy not exercised"
    );
    assert!(
        via_buffer.3.iter().all(|&x| x == 0),
        "a redo-rebuilt missing page must start blank"
    );
    assert!(
        via_buffer.4 > 40,
        "redo must extend the watermark past the page"
    );
}
