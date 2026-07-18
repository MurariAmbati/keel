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

/// `Database` is deliberately NOT asserted `Sync` here, on either pool.
///
/// It cannot be: `RefCell`/`Cell` fields make it `!Sync` no matter what pool it
/// holds. Adding `assert_sync::<Database<PageCache>>()` would fail to compile — that
/// failure is the honest boundary of this migration, and this note exists so nobody
/// (including me) later assumes the swap delivered concurrent SQL.
///
/// The remaining work to actually get there is: convert `Database`'s interior
/// mutability (and `HeapFile`'s `fsm`/`cursor`/`stats`, and `BTree`'s `root`) from
/// `RefCell`/`Cell` to `Mutex`/atomics. That is a separate, independently testable
/// change with its own failure modes — not a continuation of the pool swap.
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
