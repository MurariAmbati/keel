//! The acceptance harness for any B+tree concurrency protocol.
//!
//! `root_split_race.rs` asks one question — is every inserted key still reachable. That was
//! enough to prove the atomic-root conversion loses rows, but it is far too weak to accept a
//! latch protocol. A tree can answer `get` correctly for every key while having duplicated a
//! separator, mis-ordered a leaf, or wedged two threads against each other.
//!
//! So this file is deliberately harder, and is written BEFORE the protocol it will judge:
//!
//! * `concurrent_writers_match_a_btreemap_oracle` — the project's standard differential shape.
//!   The full `scan_all()` must equal a `BTreeMap` built from the same inserts: same length,
//!   same order, same rids, no duplicates. Catches loss, duplication and mis-ordering, where
//!   reachability catches only loss.
//! * `readers_never_observe_a_torn_tree` — readers run *concurrently with* writers. A key that
//!   has been observed present must never later read absent, and every `range` result must come
//!   back sorted. This is the half a whole-tree write lock gets for free and a fine-grained
//!   protocol must earn.
//! * every test runs under a watchdog, because the failure mode of a bad latch protocol is a
//!   hang, and a hung test is indistinguishable from a slow one until CI times out.
//!
//! Validation of the harness itself is in the branch history: it passes against the whole-tree
//! `RwLock` (known-correct) and fails against the bare atomic root (known-broken). A harness
//! that has only ever been run against correct code has not been shown to detect anything.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use keel_btree::BTree;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_heap::Rid;
use keel_vfs::{BlockFile, MemDisk};

fn key(n: usize) -> [u8; 8] {
    (n as u64).to_be_bytes()
}

fn rid(n: usize) -> Rid {
    Rid {
        page: n as u32,
        slot: (n % 251) as u16,
    }
}

/// Aborts the process if the body has not finished within `secs`.
///
/// A latch protocol that deadlocks produces a test that never returns, which reads as "still
/// running" rather than "broken". Turning that into a hard, labelled failure is the difference
/// between a diagnosable bug and a mysterious CI timeout.
fn under_watchdog<R>(name: &'static str, secs: u64, body: impl FnOnce() -> R) -> R {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if flag.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!(
            "WATCHDOG: `{name}` made no progress for {secs}s — treating as a deadlock in the \
             latch protocol, not a slow machine. Aborting so it fails loudly."
        );
        std::process::exit(101);
    });
    let out = body();
    done.store(true, Ordering::Relaxed);
    out
}

fn with_tree<R>(f: impl FnOnce(&BTree<'_, PageCache>) -> R) -> R {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk, 512, Arc::new(NoWal), PageFormat::keel_page());
    let tree = BTree::create(&cache).expect("create tree");
    f(&tree)
}

#[test]
fn concurrent_writers_match_a_btreemap_oracle() {
    const THREADS: usize = 4;
    const PER_THREAD: usize = 1200;

    under_watchdog("concurrent_writers_match_a_btreemap_oracle", 120, || {
        with_tree(|tree| {
            let barrier = Arc::new(Barrier::new(THREADS));
            std::thread::scope(|s| {
                for t in 0..THREADS {
                    let barrier = Arc::clone(&barrier);
                    s.spawn(move || {
                        barrier.wait();
                        for i in 0..PER_THREAD {
                            let n = i * THREADS + t;
                            tree.insert(&key(n), rid(n)).expect("insert must not error");
                        }
                    });
                }
            });

            let mut oracle = BTreeMap::new();
            for t in 0..THREADS {
                for i in 0..PER_THREAD {
                    let n = i * THREADS + t;
                    oracle.insert(key(n).to_vec(), rid(n));
                }
            }

            let got = tree.scan_all().expect("scan_all must not error");

            assert_eq!(
                got.len(),
                oracle.len(),
                "scan returned {} entries, oracle has {} — a fine-grained protocol that loses \
                 or duplicates entries fails here even when every key is still reachable",
                got.len(),
                oracle.len()
            );

            let expected: Vec<_> = oracle.into_iter().collect();
            for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
                assert_eq!(
                    (&g.0, &g.1),
                    (&e.0, &e.1),
                    "scan diverges from the oracle at position {i}: a full scan must be sorted \
                     and must carry the rid the key was inserted with"
                );
            }

            let report = tree.check().expect("check must not error");
            assert!(
                report.ok(),
                "tree invariants violated after concurrent inserts"
            );
        })
    });
}

#[test]
fn readers_never_observe_a_torn_tree() {
    const WRITERS: usize = 3;
    const READERS: usize = 3;
    const PER_WRITER: usize = 900;

    under_watchdog("readers_never_observe_a_torn_tree", 120, || {
        with_tree(|tree| {
            let stop = AtomicBool::new(false);
            let vanished = AtomicUsize::new(0);
            let unsorted = AtomicUsize::new(0);
            let barrier = Arc::new(Barrier::new(WRITERS + READERS));

            std::thread::scope(|s| {
                for t in 0..WRITERS {
                    let barrier = Arc::clone(&barrier);
                    let stop = &stop;
                    s.spawn(move || {
                        barrier.wait();
                        for i in 0..PER_WRITER {
                            let n = i * WRITERS + t;
                            tree.insert(&key(n), rid(n)).expect("insert must not error");
                        }
                        if t == 0 {
                            stop.store(true, Ordering::Release);
                        }
                    });
                }

                for _ in 0..READERS {
                    let barrier = Arc::clone(&barrier);
                    let (stop, vanished, unsorted) = (&stop, &vanished, &unsorted);
                    s.spawn(move || {
                        barrier.wait();
                        let mut seen: Vec<usize> = Vec::new();
                        while !stop.load(Ordering::Acquire) {
                            for n in (0..WRITERS * PER_WRITER).step_by(97) {
                                if tree.get(&key(n)).expect("get must not error").is_some() {
                                    seen.push(n);
                                }
                            }
                            for &n in &seen {
                                if tree.get(&key(n)).expect("get must not error").is_none() {
                                    vanished.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            let r = tree
                                .range(&key(0), Some(&key(WRITERS * PER_WRITER)))
                                .expect("range must not error");
                            if r.windows(2).any(|w| w[0].0 > w[1].0) {
                                unsorted.fetch_add(1, Ordering::Relaxed);
                            }
                            seen.clear();
                        }
                    });
                }
            });

            assert_eq!(
                vanished.load(Ordering::Relaxed),
                0,
                "a key that was observed present later read as absent — a reader descended into \
                 a node that a concurrent split had already detached"
            );
            assert_eq!(
                unsorted.load(Ordering::Relaxed),
                0,
                "a range scan came back out of order — a reader observed a split midway through"
            );

            let report = tree.check().expect("check must not error");
            assert!(report.ok(), "tree invariants violated after mixed load");
        })
    });
}
