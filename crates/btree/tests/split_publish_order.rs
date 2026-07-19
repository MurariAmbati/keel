//! Reachable-but-unwritten is invalid; written-but-unreachable is fine. That asymmetry is why a
//! split must fill its new sibling before publishing a pointer to it.
//!
//! `insert_rec`'s leaf-split arm allocates `right_pid`, sets `leaf.next = right_pid`, then writes
//! **`pid` first and `right_pid` second**. Between those two writes the sibling is reachable from
//! the tree but has never been written. `alloc` zeroes the whole page (`raw::init_header`), and a
//! zeroed leaf decodes `extra == 0` as `prev = 0, next = 0` - page 0, the **Meta page**, not `NIL`
//! (`u32::MAX`). So the window exposes a tree whose leaf chain runs into the Meta page.
//!
//! The two tests establish the asymmetry directly:
//!
//! * `a_reachable_but_unwritten_page_is_never_accepted` - zero any page of a real tree and
//!   `check()` rejects it, for every page. That is the state publish-then-fill exposes.
//! * `an_unreferenced_written_page_keeps_the_tree_valid` - a fully written page nothing points at
//!   leaves the tree valid. That is the state fill-then-publish exposes instead.
//!
//! So reordering converts an observable-invalid window into an observable-valid one, for free.
//!
//! WHAT THIS DOES NOT DO, stated because it is easy to assume otherwise. `store_leaf` writes via
//! `Pager::with_page_mut`, which mutates the **cached** page and marks it dirty; the disk write
//! happens later, at eviction or checkpoint, in whatever order the cache chooses. So the store
//! order governs what a concurrent reader sees through the cache and does **not** make the split
//! crash-atomic - a crash can still land `pid`'s flush without `right_pid`'s whichever order the
//! stores happened in. Crash atomicity for a split needs the WAL (these tests run `NoWal`), which
//! is separate work that this reordering is not a substitute for.

use std::sync::Arc;

use keel_btree::BTree;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_heap::Rid;
use keel_page::PAGE_SIZE;
use keel_vfs::{BlockFile, MemDisk};

const N: usize = 4000;

fn key(n: usize) -> [u8; 8] {
    (n as u64).to_be_bytes()
}

fn open(disk: &Arc<dyn BlockFile>) -> PageCache {
    PageCache::open_formatted(
        Arc::clone(disk),
        256,
        Arc::new(NoWal),
        PageFormat::keel_page(),
    )
}

/// A tree deep enough to have split many times, flushed to the image.
fn build(disk: &Arc<dyn BlockFile>) -> (u32, u32) {
    let cache = open(disk);
    let tree = BTree::create(&cache).expect("create");
    for n in 0..N {
        tree.insert(
            &key(n),
            Rid {
                page: n as u32,
                slot: 0,
            },
        )
        .expect("insert");
    }
    assert!(
        tree.check().expect("check").ok(),
        "the tree must be valid before we damage it"
    );
    let root = tree.root();
    let pages = cache.page_count();
    cache.checkpoint().expect("checkpoint");
    (root, pages)
}

fn valid_after(disk: &Arc<dyn BlockFile>, root: u32) -> bool {
    let cache = open(disk);
    let tree = BTree::open_rooted(&cache, root);
    tree.check().map(|r| r.ok()).unwrap_or(false)
}

#[test]
fn a_reachable_but_unwritten_page_is_never_accepted() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let (root, pages) = build(&disk);

    let mut accepted_a_lost_write = Vec::new();
    for pid in 1..pages {
        let mut save = vec![0u8; PAGE_SIZE];
        let off = pid as u64 * PAGE_SIZE as u64;
        disk.read_at(&mut save, off).expect("read");

        disk.write_at(&vec![0u8; PAGE_SIZE], off).expect("zero");
        if valid_after(&disk, root) {
            accepted_a_lost_write.push(pid);
        }
        disk.write_at(&save, off).expect("restore");
    }

    assert!(
        accepted_a_lost_write.is_empty(),
        "check() called the tree valid after page(s) {accepted_a_lost_write:?} lost their write. \
         This is exactly the state publish-then-fill exposes to a concurrent reader \
         between its two writes, so it must never be accepted"
    );
}

#[test]
fn an_unreferenced_written_page_keeps_the_tree_valid() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let (root, _) = build(&disk);

    let cache = open(&disk);
    let orphan = cache.new_page().expect("alloc the would-be sibling");
    let orphan_pid = orphan.pid();
    {
        let mut b = orphan.write();
        keel_page::raw::init_header(&mut b, keel_page::PageType::BTreeLeaf);
    }
    drop(orphan);
    cache.checkpoint().expect("checkpoint");
    drop(cache);

    assert!(
        valid_after(&disk, root),
        "under fill-then-publish the window exposes the pre-split tree, plus an unreferenced page's, leaving the \
         {orphan_pid}. That must still be a valid tree — \
         the split is not yet visible, nothing is corrupt"
    );
}
