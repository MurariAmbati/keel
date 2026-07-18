use std::sync::Arc;

use keel_db::Database;
use keel_rng::Rng;
use keel_sql::{parse_statement, MemDb};
use keel_vfs::{BlockFile, MemDisk};

fn fresh() -> (Database, MemDb) {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    (Database::open(disk, 64).unwrap(), MemDb::new())
}

fn run_both(db: &Database, mem: &mut MemDb, sql: &str) {
    db.execute(sql).unwrap();
    mem.execute(&parse_statement(sql).unwrap()).unwrap();
}

fn compare(db: &Database, mem: &mut MemDb, sql: &str, seed: u64) {
    let got = db.execute(sql).unwrap().unwrap();
    let want = mem
        .execute(&parse_statement(sql).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(got.columns, want.columns, "seed {seed} cols: `{sql}`");
    assert_eq!(got.rows, want.rows, "seed {seed} rows: `{sql}`");
}

#[test]
fn streaming_aggregation_differential() {
    for seed in 0..30u64 {
        let (db, mut mem) = fresh();
        run_both(
            &db,
            &mut mem,
            "CREATE TABLE t (id INT, g INT, v INT, p DOUBLE)",
        );
        let mut rng = Rng::seed(seed ^ 0xA66);
        for id in 0..60i64 {
            let g = (rng.below(5)) as i64;
            let vexpr = if rng.below(9) == 0 {
                "NULL".to_string()
            } else {
                (rng.below(20) as i64).to_string()
            };
            let p = rng.below(1000) as f64 / 10.0;
            run_both(
                &db,
                &mut mem,
                &format!("INSERT INTO t VALUES ({id}, {g}, {vexpr}, {p})"),
            );
        }

        let before = db.agg_streams();
        let queries = [
            "SELECT g, COUNT(*), SUM(v) FROM t GROUP BY g ORDER BY g",
            "SELECT g, MIN(v), MAX(v), AVG(v) FROM t GROUP BY g ORDER BY g",
            "SELECT g, COUNT(*) FROM t GROUP BY g HAVING COUNT(*) > 10 ORDER BY g",
            "SELECT COUNT(*), SUM(v), AVG(p) FROM t",
            "SELECT g, SUM(v) + 1, COUNT(*) * 2 FROM t GROUP BY g ORDER BY g",
            "SELECT g, COUNT(DISTINCT v) FROM t GROUP BY g ORDER BY g",
            "SELECT g, COUNT(*), COUNT(v) FROM t WHERE p > 20 GROUP BY g ORDER BY g",
            "SELECT COUNT(*), SUM(v) FROM t WHERE g > 100",
            "SELECT g, CASE WHEN COUNT(*) > 12 THEN 'big' ELSE 'small' END FROM t GROUP BY g ORDER BY g",
        ];
        for q in queries {
            compare(&db, &mut mem, q, seed);
        }
        assert_eq!(
            db.agg_streams(),
            before + queries.len() as u64,
            "seed {seed}: every aggregate query should use the streaming aggregate path"
        );
    }
}
