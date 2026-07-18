//! Does the D-PAGER-9 root-split race actually lose rows, or is it only an argument?
//!
//! D-PAGER-9 claims that converting `BTree::root` from `Cell<PageId>` to an atomic
//! satisfies `Sync` while leaving a lost-update window, because `insert` reads `root`,
//! performs a whole recursive descent plus a page allocation and an internal-node
//! write, and only then writes `root` back. That claim was made by reading the code.
//! This file is the experiment that decides it.
//!
//! The conversion is performed deliberately on this branch so the scenario is even
//! constructible — with `Cell` the tree is `!Sync` and the compiler refuses to share it.
//! That is the point: the type system currently prevents the bug, and the "obvious"
//! fix for `Sync` removes that protection without adding anything in its place.
//!
//! A pass here would mean D-PAGER-9 overstates the hazard and should be downgraded to
//! reasoning. A failure means the claim is real and reproducible.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use keel_btree::BTree;
use keel_heap::Rid;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_vfs::{BlockFile, MemDisk};

const THREADS: usize = 4;
const PER_THREAD: usize = 1500;

fn key(n: usize) -> [u8; 8] {
    (n as u64).to_be_bytes()
}

#[test]
fn concurrent_inserts_keep_every_row_reachable() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk, 256, Arc::new(NoWal), PageFormat::keel_page());
    let tree = BTree::create(&cache).expect("create tree");

    let barrier = Arc::new(Barrier::new(THREADS));
    let inserted = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let tree = &tree;
            let barrier = Arc::clone(&barrier);
            let inserted = &inserted;
            s.spawn(move || {
                barrier.wait();
                for i in 0..PER_THREAD {
                    let n = t * PER_THREAD + i;
                    let rid = Rid {
                        page: n as u32,
                        slot: 0,
                    };
                    if tree.insert(&key(n), rid).is_ok() {
                        inserted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    let claimed = inserted.load(Ordering::Relaxed);
    let mut missing = Vec::new();
    for n in 0..THREADS * PER_THREAD {
        if tree.get(&key(n)).expect("get must not error").is_none() {
            missing.push(n);
        }
    }

    assert!(
        missing.is_empty(),
        "{} of {claimed} accepted inserts are unreachable from the root \
         (first few: {:?}). Every insert returned Ok, so this is silent loss, not \
         reported failure. Note what this does and does not establish: it proves the \
         hazard is real and severe once the type system stops refusing to share the \
         tree. It does NOT isolate the root read-modify-write as the cause — the \
         recursive descent and the node writes are equally unsynchronised, and a loss \
         rate this high cannot be explained by root splits alone.",
        missing.len(),
        &missing[..missing.len().min(8)]
    );
}
