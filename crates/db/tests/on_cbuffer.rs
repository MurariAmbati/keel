use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_db::Database;
use keel_pager::RecoveryPager;
use keel_vfs::{BlockFile, MemDisk};
use std::sync::Arc;

const FRAMES: usize = 16;

const WORKLOAD: &[&str] = &[
    "CREATE TABLE dept (id BIGINT, name TEXT)",
    "CREATE TABLE emp (id BIGINT, dept_id BIGINT, name TEXT, salary BIGINT)",
    "INSERT INTO dept VALUES (1, 'eng'), (2, 'ops'), (3, 'sales')",
    "CREATE INDEX idx_emp_id ON emp (id)",
];

fn run<P: RecoveryPager>(db: &Database<P>) -> Vec<String> {
    let mut out = Vec::new();
    for stmt in WORKLOAD {
        db.execute(stmt).expect(stmt);
    }
    for i in 0..400i64 {
        let d = (i % 3) + 1;
        db.execute(&format!(
            "INSERT INTO emp VALUES ({i}, {d}, 'name{i}', {})",
            1000 + i
        ))
        .expect("insert emp");
    }
    db.execute("UPDATE emp SET salary = salary + 500 WHERE dept_id = 1")
        .expect("update");
    db.execute("DELETE FROM emp WHERE id < 20").expect("delete");
    db.analyze().expect("analyze");

    out.push(format!(
        "{:?}",
        db.execute("SELECT name, salary FROM emp WHERE id = 123")
            .expect("indexed lookup")
            .unwrap()
    ));
    out.push(format!(
        "{:?}",
        db.execute(
            "SELECT d.name, COUNT(*), SUM(e.salary) FROM emp e JOIN dept d ON e.dept_id = d.id \
             GROUP BY d.name ORDER BY d.name"
        )
        .expect("join+agg")
        .unwrap()
    ));
    out.push(format!(
        "{:?}",
        db.execute("SELECT id, salary FROM emp WHERE id >= 100 AND id <= 110 ORDER BY id")
            .expect("range")
            .unwrap()
    ));
    out.push(format!(
        "{:?}",
        db.execute("SELECT COUNT(*) FROM emp")
            .expect("count")
            .unwrap()
    ));
    out
}

fn concurrent_db(disk: Arc<dyn BlockFile>) -> Database<PageCache> {
    let cache = PageCache::open_formatted(disk, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    Database::with_pager(cache).expect("open over PageCache")
}

#[test]
fn the_whole_sql_engine_agrees_on_both_pools() {
    let disk_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let via_buffer = {
        let db = Database::open(disk_a, FRAMES).expect("open over BufferPool");
        run(&db)
    };

    let disk_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let via_cache = run(&concurrent_db(disk_b));

    assert_eq!(
        via_buffer.len(),
        via_cache.len(),
        "different number of results"
    );
    for (i, (a, b)) in via_buffer.iter().zip(via_cache.iter()).enumerate() {
        assert_eq!(a, b, "query {i} differs between the two pools");
    }
    assert!(
        via_buffer[3].contains("380"),
        "expected 400 inserts minus 20 deletes (id 0..19) = 380 rows, got {}",
        via_buffer[3]
    );
}

#[test]
fn a_database_on_the_concurrent_cache_survives_checkpoint_and_reopen() {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let expected = {
        let db = concurrent_db(disk.clone());
        let out = run(&db);
        db.checkpoint().expect("checkpoint");
        out
    };

    let db = concurrent_db(disk);
    let after = vec![
        format!(
            "{:?}",
            db.execute("SELECT name, salary FROM emp WHERE id = 123")
                .unwrap()
                .unwrap()
        ),
        format!(
            "{:?}",
            db.execute(
                "SELECT d.name, COUNT(*), SUM(e.salary) FROM emp e JOIN dept d ON e.dept_id = d.id \
                 GROUP BY d.name ORDER BY d.name"
            )
            .unwrap().unwrap()
        ),
        format!(
            "{:?}",
            db.execute("SELECT id, salary FROM emp WHERE id >= 100 AND id <= 110 ORDER BY id")
                .unwrap()
                .unwrap()
        ),
        format!(
            "{:?}",
            db.execute("SELECT COUNT(*) FROM emp").unwrap().unwrap()
        ),
    ];
    assert_eq!(
        expected, after,
        "the database did not come back identical after checkpoint + reopen on PageCache"
    );
}
