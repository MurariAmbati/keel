use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_db::Database;
use keel_dbcheck::check_file;
use keel_faultfs::{FaultConfig, FaultDisk};
use keel_types::Value;
use keel_vfs::BlockFile;
use std::sync::Arc;

const FRAMES: usize = 16;

fn int_of(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn concurrent_db(file: Arc<dyn BlockFile>) -> Database<PageCache> {
    let cache = PageCache::open_formatted(file, FRAMES, Arc::new(NoWal), PageFormat::keel_page());
    Database::with_pager(cache).expect("open over PageCache")
}

#[test]
fn power_loss_preserves_sql_state_on_the_concurrent_cache() {
    for seed in 0..12u64 {
        let disk = FaultDisk::new(FaultConfig::benign(), seed);
        let file: Arc<dyn BlockFile> = Arc::new(disk.handle());

        {
            let db = concurrent_db(file.clone());
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
            db.checkpoint().unwrap();
        }
        disk.crash();

        let db = concurrent_db(file.clone());

        let n = db.execute("SELECT COUNT(*) FROM acct").unwrap().unwrap();
        assert_eq!(
            int_of(&n.rows[0][0]),
            180,
            "seed {seed}: row count after power loss (200 inserted, 20 with k=7 deleted)"
        );

        let gone = db
            .execute("SELECT id FROM acct WHERE k = 7")
            .unwrap()
            .unwrap();
        assert_eq!(gone.rows.len(), 0, "seed {seed}: deleted rows came back");

        let updated = db
            .execute("SELECT id, bal FROM acct WHERE k = 3 ORDER BY id")
            .unwrap()
            .unwrap();
        assert_eq!(updated.rows.len(), 20, "seed {seed}: k=3 rows");
        for row in &updated.rows {
            let id = int_of(&row[0]);
            assert_eq!(
                int_of(&row[1]),
                id * 100 + 1,
                "seed {seed}: the UPDATE did not survive for id {id}"
            );
        }

        assert!(
            check_file(&*file).unwrap().ok(),
            "seed {seed}: dbcheck found the file inconsistent after power loss"
        );
    }
}
