//! Can a descent hold a parent latch while acquiring a child's?
//!
//! Every latch-coupling protocol depends on this and nothing else. If `PageCache` cannot hand
//! out two page guards at once — or deadlocks when the second fetch triggers an eviction that
//! wants a page the caller is already holding — then crabbing is not implementable here at all,
//! whatever a design says on paper.

use std::sync::Arc;

use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_vfs::{BlockFile, MemDisk};

fn cache(frames: usize) -> PageCache {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    PageCache::open_formatted(disk, frames, Arc::new(NoWal), PageFormat::keel_page())
}

#[test]
fn two_read_guards_can_be_held_at_once() {
    let c = cache(16);
    let a = c.new_page().expect("alloc a");
    let b = c.new_page().expect("alloc b");
    let (pa, pb) = (a.pid(), b.pid());
    drop(a);
    drop(b);

    let ra = c.fetch(pa).expect("fetch a");
    let ga = ra.read();
    let rb = c.fetch(pb).expect("fetch b");
    let gb = rb.read();
    assert_eq!(
        ga.len(),
        gb.len(),
        "both guards must be live simultaneously"
    );
}

#[test]
fn a_write_guard_on_the_child_while_reading_the_parent() {
    let c = cache(16);
    let parent = c.new_page().expect("alloc parent");
    let child = c.new_page().expect("alloc child");
    let (pp, pc) = (parent.pid(), child.pid());
    drop(parent);
    drop(child);

    let rp = c.fetch(pp).expect("fetch parent");
    let gp = rp.read();
    let rc = c.fetch(pc).expect("fetch child");
    let mut gc = rc.write();
    gc[64] = 0xAB;
    assert_eq!(
        gp.len(),
        gc.len(),
        "coupled parent-read + child-write must coexist"
    );
}

/// The dangerous one: acquire the second page when the pool has no free frame, so the fetch
/// must evict. If eviction can pick the frame the caller is still holding, a descent deadlocks
/// against itself under memory pressure — which would not show up in any small test.
#[test]
fn fetching_under_pressure_while_holding_a_guard_does_not_wedge() {
    let c = cache(4);
    let mut pids = Vec::new();
    for _ in 0..16 {
        let p = c.new_page().expect("alloc");
        pids.push(p.pid());
    }

    let held = c.fetch(pids[0]).expect("fetch held");
    let _g = held.read();

    for &pid in &pids[1..] {
        let r = c
            .fetch(pid)
            .expect("fetch under pressure must not fail while a guard is held");
        let _gg = r.read();
    }
}
