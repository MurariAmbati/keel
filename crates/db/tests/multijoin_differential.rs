//! Multi-way join differential — the streaming hash-join fold across 3 and 4
//! tables, in chain and star shapes, checked against the reference oracle.
//!
//! `try_stream_join` folds the FROM tables left-deep, extracting one equijoin per
//! step; earlier per-surface tests only exercised two tables. Here random 3- and
//! 4-table joins run through the streaming path and must match the materializing
//! reference engine exactly, with a `join_streams` assertion proving the hash-join
//! path (not the fallback) served them. Every failure replays from its `seed`.

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
fn three_and_four_way_join_differential() {
    for seed in 0..25u64 {
        let (db, mut mem) = fresh();
        for ddl in [
            "CREATE TABLE a (id INT, k1 INT)",
            "CREATE TABLE b (k1 INT, k2 INT, w INT)",
            "CREATE TABLE c (k2 INT, k3 INT, z INT)",
            "CREATE TABLE d (k3 INT, q INT)",
        ] {
            run_both(&db, &mut mem, ddl);
        }
        let mut rng = Rng::seed(seed ^ 0x3C4);
        for id in 0..20i64 {
            run_both(
                &db,
                &mut mem,
                &format!("INSERT INTO a VALUES ({id}, {})", rng.below(4)),
            );
        }
        for _ in 0..24 {
            run_both(
                &db,
                &mut mem,
                &format!(
                    "INSERT INTO b VALUES ({}, {}, {})",
                    rng.below(4),
                    rng.below(5),
                    rng.below(100)
                ),
            );
        }
        for _ in 0..24 {
            run_both(
                &db,
                &mut mem,
                &format!(
                    "INSERT INTO c VALUES ({}, {}, {})",
                    rng.below(5),
                    rng.below(3),
                    rng.below(100)
                ),
            );
        }
        for _ in 0..12 {
            run_both(
                &db,
                &mut mem,
                &format!(
                    "INSERT INTO d VALUES ({}, {})",
                    rng.below(3),
                    rng.below(100)
                ),
            );
        }

        let before = db.join_streams();
        let queries = [
            "SELECT a.id, b.w, c.z FROM a JOIN b ON a.k1 = b.k1 JOIN c ON b.k2 = c.k2 \
             ORDER BY a.id, b.w, c.z",
            "SELECT a.id, c.z FROM a JOIN b ON a.k1 = b.k1 JOIN c ON b.k2 = c.k2 \
             WHERE b.w > 50 ORDER BY a.id, c.z",
            "SELECT a.id, d.q FROM a JOIN b ON a.k1 = b.k1 JOIN c ON b.k2 = c.k2 \
             JOIN d ON c.k3 = d.k3 ORDER BY a.id, d.q",
            "SELECT a.id, b.w, c.z FROM a JOIN b ON a.k1 = b.k1 LEFT JOIN c ON b.k2 = c.k2 \
             ORDER BY a.id, b.w, c.z",
        ];
        for q in queries {
            compare(&db, &mut mem, q, seed);
        }
        assert_eq!(
            db.join_streams(),
            before + queries.len() as u64,
            "seed {seed}: all multi-way joins should use the streaming hash-join path"
        );
    }
}
