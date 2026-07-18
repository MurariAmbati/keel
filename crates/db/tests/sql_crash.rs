use std::sync::Arc;

use keel_db::Database;
use keel_dbcheck::check_file;
use keel_faultfs::{FaultConfig, FaultDisk};
use keel_types::Value;
use keel_vfs::BlockFile;

fn int_of(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        _ => panic!("expected integer, got {v:?}"),
    }
}

#[test]
fn benign_crash_preserves_sql_state() {
    for seed in 0..12u64 {
        let disk = FaultDisk::new(FaultConfig::benign(), seed);
        let file: Arc<dyn BlockFile> = Arc::new(disk.handle());

        {
            let db = Database::open(file.clone(), 16).unwrap();
            db.execute("CREATE TABLE acct (id INT, k INT, bal BIGINT)")
                .unwrap();
            for i in 0..200i64 {
                db.execute(&format!(
                    "INSERT INTO acct VALUES ({i}, {}, {})",
                    i % 10,
                    i * 100
                ))
                .unwrap();
            }
            db.execute("CREATE INDEX ix ON acct (k)").unwrap();
            db.execute("UPDATE acct SET bal = bal + 1 WHERE k = 3")
                .unwrap();
            db.execute("DELETE FROM acct WHERE k = 7").unwrap();
        }
        disk.crash();

        let db = Database::open(file.clone(), 16).unwrap();

        let n = db.execute("SELECT COUNT(*) FROM acct").unwrap().unwrap();
        assert_eq!(
            int_of(&n.rows[0][0]),
            180,
            "seed {seed}: row count after crash"
        );

        let gone = db
            .execute("SELECT id FROM acct WHERE k = 7")
            .unwrap()
            .unwrap();
        assert_eq!(gone.rows.len(), 0, "seed {seed}: deleted rows came back");

        let before = db.index_lookups();
        let updated = db
            .execute("SELECT id, bal FROM acct WHERE k = 3 ORDER BY id")
            .unwrap()
            .unwrap();
        let ids: Vec<i64> = (0..200i64).filter(|i| i % 10 == 3).collect();
        assert_eq!(updated.rows.len(), ids.len(), "seed {seed}: k=3 rows");
        for (row, id) in updated.rows.iter().zip(&ids) {
            assert_eq!(int_of(&row[0]), *id, "seed {seed}");
            assert_eq!(
                int_of(&row[1]),
                id * 100 + 1,
                "seed {seed}: UPDATE not durable for id {id}"
            );
        }
        assert!(
            db.index_lookups() > before,
            "seed {seed}: index should still serve the k=3 lookup after crash"
        );

        assert!(
            check_file(&*file).unwrap().ok(),
            "seed {seed}: dbcheck failed after benign SQL crash"
        );
    }
}

#[test]
fn benign_crash_preserves_multiple_tables() {
    let disk = FaultDisk::new(FaultConfig::benign(), 99);
    let file: Arc<dyn BlockFile> = Arc::new(disk.handle());
    {
        let db = Database::open(file.clone(), 16).unwrap();
        db.execute("CREATE TABLE a (id INT, v BIGINT)").unwrap();
        db.execute("CREATE TABLE b (id INT, w BIGINT)").unwrap();
        for i in 0..50i64 {
            db.execute(&format!("INSERT INTO a VALUES ({i}, {})", i * 2))
                .unwrap();
            db.execute(&format!("INSERT INTO b VALUES ({i}, {})", i * 3))
                .unwrap();
        }
        db.execute("DELETE FROM a WHERE id >= 40").unwrap();
    }
    disk.crash();

    let db = Database::open(file.clone(), 16).unwrap();
    let mut names = db.table_names();
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(
        int_of(&db.execute("SELECT COUNT(*) FROM a").unwrap().unwrap().rows[0][0]),
        40,
        "table a survived with its DELETE applied"
    );
    assert_eq!(
        int_of(&db.execute("SELECT SUM(w) FROM b").unwrap().unwrap().rows[0][0]),
        (0..50i64).map(|i| i * 3).sum::<i64>(),
        "table b survived intact"
    );
    assert!(check_file(&*file).unwrap().ok(), "dbcheck after crash");
}
