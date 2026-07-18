use std::sync::Arc;

use keel_db::Database;
use keel_rng::Rng;
use keel_sql::{parse_statement, MemDb};
use keel_vfs::{BlockFile, MemDisk};

fn fresh_pair() -> (Database, MemDb) {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    (Database::open(disk, 32).unwrap(), MemDb::new())
}

fn run_both(db: &Database, mem: &mut MemDb, sql: &str) {
    let d = db.execute(sql);
    let m = mem.execute(&parse_statement(sql).unwrap());
    assert_eq!(
        d.is_ok(),
        m.is_ok(),
        "accept/reject diverged for `{sql}`: db={:?} mem={:?}",
        d.as_ref().map(|_| ()),
        m.as_ref().map(|_| ())
    );
}

fn compare(db: &Database, mem: &mut MemDb, sql: &str, seed: u64) {
    let got = db.execute(sql).unwrap().unwrap();
    let want = mem
        .execute(&parse_statement(sql).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(got.columns, want.columns, "seed {seed} columns: `{sql}`");
    assert_eq!(got.rows, want.rows, "seed {seed} rows: `{sql}`");
}

const CREATE_A: &str = "CREATE TABLE a (id INT, k INT, v INT)";
const CREATE_B: &str = "CREATE TABLE b (id INT, k INT, w INT)";

const QUERIES: &[&str] = &[
    "SELECT id, k, v FROM a WHERE v > 5 ORDER BY id",
    "SELECT id FROM a WHERE k = 2 ORDER BY id",
    "SELECT DISTINCT k FROM a ORDER BY k",
    "SELECT a.id, b.w FROM a JOIN b ON a.k = b.k ORDER BY a.id, b.w",
    "SELECT a.id, a.v, b.w FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.id, b.w",
    "SELECT k, COUNT(*), SUM(v) FROM a GROUP BY k HAVING COUNT(*) >= 1 ORDER BY k",
    "SELECT id, v FROM a ORDER BY v, id LIMIT 5",
    "SELECT COUNT(*) FROM a WHERE v IS NULL",
];

#[test]
fn full_lifecycle_differential() {
    for seed in 0..30u64 {
        let (db, mut mem) = fresh_pair();
        run_both(&db, &mut mem, CREATE_A);
        run_both(&db, &mut mem, CREATE_B);
        db.execute("CREATE INDEX ixa ON a (k)").unwrap();

        let mut rng = Rng::seed(seed ^ 0xF00D);
        let mut next_id: i64 = 0;

        for _ in 0..20 {
            let (k, v) = (rng.below(4) as i64, rng.below(10) as i64);
            let vexpr = if rng.below(8) == 0 {
                "NULL".to_string()
            } else {
                v.to_string()
            };
            run_both(
                &db,
                &mut mem,
                &format!("INSERT INTO a VALUES ({next_id}, {k}, {vexpr})"),
            );
            next_id += 1;
            let (bk, w) = (rng.below(6) as i64, rng.below(10) as i64);
            run_both(
                &db,
                &mut mem,
                &format!("INSERT INTO b VALUES ({next_id}, {bk}, {w})"),
            );
            next_id += 1;
        }

        for step in 0..40 {
            match rng.below(5) {
                0 => {
                    let (k, v) = (rng.below(4) as i64, rng.below(10) as i64);
                    let vexpr = if rng.below(8) == 0 {
                        "NULL".to_string()
                    } else {
                        v.to_string()
                    };
                    run_both(
                        &db,
                        &mut mem,
                        &format!("INSERT INTO a VALUES ({next_id}, {k}, {vexpr})"),
                    );
                    next_id += 1;
                }
                1 => {
                    let thr = rng.below(10) as i64;
                    run_both(
                        &db,
                        &mut mem,
                        &format!("UPDATE a SET v = v + 1 WHERE v < {thr}"),
                    );
                }
                2 => {
                    let (from, to) = (rng.below(4) as i64, rng.below(4) as i64);
                    run_both(
                        &db,
                        &mut mem,
                        &format!("UPDATE a SET k = {to} WHERE k = {from}"),
                    );
                }
                3 => {
                    let thr = rng.below(12) as i64;
                    run_both(&db, &mut mem, &format!("DELETE FROM a WHERE v < {thr}"));
                }
                _ => {
                    if step % 7 == 0 {
                        run_both(&db, &mut mem, "DROP TABLE b");
                        run_both(&db, &mut mem, CREATE_B);
                        for _ in 0..8 {
                            let (bk, w) = (rng.below(6) as i64, rng.below(10) as i64);
                            run_both(
                                &db,
                                &mut mem,
                                &format!("INSERT INTO b VALUES ({next_id}, {bk}, {w})"),
                            );
                            next_id += 1;
                        }
                    }
                }
            }

            for q in QUERIES {
                compare(&db, &mut mem, q, seed);
            }
        }
    }
}
