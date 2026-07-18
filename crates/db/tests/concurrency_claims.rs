//! What the engine swap actually bought — and what it did **not**.
//!
//! The migration (D-PAGER-1…5c) put `heap`, `btree`, `db`, and `wal` on a page cache
//! that is itself `Send + Sync`. It would be easy to read that as "KEEL now serves
//! SQL concurrently". It does not, and this file pins the real boundary down as
//! compile-time facts so the claim cannot drift.
//!
//! `Database` still owns single-threaded interior mutability — `RefCell` catalog,
//! index list, stats and txn buffer, plus several `Cell` counters — so it is `!Sync`
//! *regardless of which pool it holds*. Swapping the pool removed the buffer layer
//! as a concurrency blocker; it did not remove the engine's own. Concurrent SQL
//! still requires either a `Mutex<Database>` (what `db`'s existing threaded test
//! does) or converting those fields, which is separate work.
//!
//! Stated as a ladder:
//!   * `PageCache`            — `Send + Sync`   ✅ genuinely concurrent
//!   * `Database<PageCache>`  — `Send`, `!Sync` ⛔ still serialized by the engine
//!
//! The value delivered is therefore real but narrower than "concurrent SQL": the
//! storage layer is no longer the thing standing in the way, and it has been proven
//! equivalent to the old one at every layer plus under power loss.

use keel_cbuffer::PageCache;
use keel_db::Database;

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

#[test]
fn the_concurrent_cache_really_is_send_and_sync() {
    assert_send::<PageCache>();
    assert_sync::<PageCache>();
}

#[test]
fn the_database_is_send_so_it_can_move_between_threads() {
    assert_send::<Database>();
    assert_send::<Database<PageCache>>();
}

/// The `!Sync` half of the ladder, as a compile-time fact rather than a comment.
///
/// This file's whole purpose is that the boundary cannot drift, but until D-PAGER-9
/// the negative half was prose — and it drifted: it used to recommend converting
/// `HeapFile`'s and `BTree`'s cells "to `Mutex`/atomics", which D-PAGER-9 shows is the
/// wrong primitive for `BTree::root`. Prose cannot fail a build; this can.
///
/// Hand-rolled rather than pulled from `static_assertions`, because the workspace has
/// no external dependencies. Two blanket impls overlap exactly when `$x: Sync`, so the
/// inferred parameter becomes ambiguous and the build fails.
macro_rules! assert_not_sync {
    ($($x:ty),+ $(,)?) => {
        $(const _: fn() = || {
            trait AmbiguousIfSync<A> {
                fn probe() {}
            }
            impl<T: ?Sized> AmbiguousIfSync<()> for T {}
            impl<T: ?Sized + Sync> AmbiguousIfSync<[(); 0]> for T {}
            let _ = <$x as AmbiguousIfSync<_>>::probe;
        };)+
    };
}

assert_not_sync!(
    Database,
    Database<PageCache>,
    keel_heap::HeapFile<'static, PageCache>,
    keel_btree::BTree<'static, PageCache>,
);

/// Why the three types above are `!Sync`, and what it would actually take to change
/// that — corrected by D-PAGER-8/9, which superseded the original advice here.
///
/// * `Database` — four `RefCell`s. D-PAGER-8 instrumented them, found one re-entrant
///   borrow (`q_error` holding `stats` across `select`), and removed it. Deadlock-clear.
/// * `HeapFile` — one `RefCell` (`fsm`, two leaf borrow sites, cannot re-enter) plus
///   `Cell` counters.
/// * `BTree` — a single `Cell<PageId>` root.
///
/// The deadlock surface is closed. What remains is **atomicity, not deadlock**:
/// `BTree::insert` reads `root`, performs a whole recursive descent, a page allocation
/// and an internal-node write, then writes `root`. An `AtomicU32` would satisfy `Sync`
/// and still lose a concurrent root split, leaking the loser's subtree. A root split
/// needs mutual exclusion over the *operation*. See D-PAGER-9.
#[test]
fn shared_use_across_threads_still_needs_a_mutex() {
    use keel_cbuffer::{NoWal, PageFormat};
    use keel_vfs::{BlockFile, MemDisk};
    use std::sync::{Arc, Mutex};

    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk, 16, Arc::new(NoWal), PageFormat::keel_page());
    let db = Arc::new(Mutex::new(Database::with_pager(cache).unwrap()));

    db.lock()
        .unwrap()
        .execute("CREATE TABLE t (id BIGINT, v BIGINT)")
        .unwrap();

    let mut handles = Vec::new();
    for t in 0..4i64 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..25i64 {
                let id = t * 25 + i;
                db.lock()
                    .unwrap()
                    .execute(&format!("INSERT INTO t VALUES ({id}, {})", id * 2))
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let n = db
        .lock()
        .unwrap()
        .execute("SELECT COUNT(*) FROM t")
        .unwrap()
        .unwrap();
    assert_eq!(
        format!("{:?}", n.rows[0][0]),
        "BigInt(100)",
        "every insert from every thread must have landed exactly once"
    );
}
