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
