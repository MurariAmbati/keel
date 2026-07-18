//! A TPC-H-shaped analytical subset (§8) — the "credible TPC-H fight", correctness
//! half. KEEL now has everything the classic queries need — multi-way hash joins
//! with cost-based ordering, hash aggregation, GROUP BY / HAVING, ORDER BY / LIMIT
//! — so this runs Q1-, Q3-, and Q6-flavored queries over a synthetic TPC-H-lite
//! dataset and checks each result against the reference oracle. Integer surrogates
//! stand in for TPC-H's decimals so sums are order-independent and the differential
//! is exact. Counters assert the streaming join and aggregate paths actually served
//! the queries (not the materializing fallback). The head-to-head timing against
//! SQLite / DuckDB is the remaining half — it needs those engines installed, which
//! this environment lacks.

use std::sync::Arc;

use keel_db::Database;
use keel_rng::Rng;
use keel_sql::{parse_statement, MemDb};
use keel_vfs::{BlockFile, MemDisk};

fn build() -> (Database, MemDb) {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open(disk, 128).unwrap();
    let mut mem = MemDb::new();
    let ddl = [
        "CREATE TABLE customer (c_id INT, c_seg INT)",
        "CREATE TABLE orders (o_id INT, o_cust INT)",
        "CREATE TABLE lineitem (l_id INT, l_order INT, l_qty INT, l_price INT, l_discount INT, l_flag INT)",
    ];
    for s in ddl {
        db.execute(s).unwrap();
        mem.execute(&parse_statement(s).unwrap()).unwrap();
    }
    let mut rng = Rng::seed(0x7C_9A_11);
    let both = |db: &Database, mem: &mut MemDb, sql: &str| {
        db.execute(sql).unwrap();
        mem.execute(&parse_statement(sql).unwrap()).unwrap();
    };
    for c in 0..20i64 {
        both(
            &db,
            &mut mem,
            &format!("INSERT INTO customer VALUES ({c}, {})", rng.below(5)),
        );
    }
    for o in 0..100i64 {
        both(
            &db,
            &mut mem,
            &format!("INSERT INTO orders VALUES ({o}, {})", rng.below(20)),
        );
    }
    for l in 0..400i64 {
        let order = rng.below(100) as i64;
        let qty = 1 + rng.below(50) as i64;
        let price = 100 + rng.below(900) as i64;
        let disc = rng.below(10) as i64;
        let flag = rng.below(3) as i64;
        both(
            &db,
            &mut mem,
            &format!("INSERT INTO lineitem VALUES ({l}, {order}, {qty}, {price}, {disc}, {flag})"),
        );
    }
    (db, mem)
}

fn compare(db: &Database, mem: &mut MemDb, sql: &str) {
    let got = db.execute(sql).unwrap().unwrap();
    let want = mem
        .execute(&parse_statement(sql).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(got.columns, want.columns, "columns: `{sql}`");
    assert_eq!(got.rows, want.rows, "rows: `{sql}`");
}

#[test]
fn tpch_subset_matches_oracle() {
    let (db, mut mem) = build();

    let q6 = "SELECT SUM(l_price) FROM lineitem \
              WHERE l_discount >= 2 AND l_discount <= 4 AND l_qty < 25";
    let agg_before = db.agg_streams();
    compare(&db, &mut mem, q6);

    let q1 = "SELECT l_flag, COUNT(*), SUM(l_qty), SUM(l_price), AVG(l_qty) \
              FROM lineitem GROUP BY l_flag ORDER BY l_flag";
    compare(&db, &mut mem, q1);
    assert!(
        db.agg_streams() >= agg_before + 2,
        "Q6 and Q1 should run on the streaming aggregate path"
    );

    let q3 = "SELECT o.o_id, SUM(l.l_price) \
              FROM customer c JOIN orders o ON c.c_id = o.o_cust \
              JOIN lineitem l ON o.o_id = l.l_order \
              WHERE c.c_seg = 3 GROUP BY o.o_id ORDER BY o.o_id LIMIT 10";
    let join_before = db.join_streams();
    compare(&db, &mut mem, q3);
    assert!(
        db.join_streams() > join_before,
        "Q3 should run on the streaming hash-join path"
    );

    let q1h = "SELECT l_flag, COUNT(*) FROM lineitem \
               GROUP BY l_flag HAVING COUNT(*) > 100 ORDER BY l_flag";
    compare(&db, &mut mem, q1h);
}
