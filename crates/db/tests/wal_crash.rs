//! SQL redo crash campaign — DML through the logical WAL (the rung-1 analog at the
//! query surface).
//!
//! In logged mode every mutating statement is appended to the redo log and fsynced
//! *before* it is applied, and the data file is held under no-steal, so the log is
//! the sole durable record. These tests pull the power and require that the
//! committed statements are reconstructed **exactly** from the durable log onto an
//! empty data disk — the SQL-level statement of the property the bank-accounts
//! campaign proved physically — and that a torn (half-written) tail record is
//! dropped atomically, never half-applied. Every failure replays from its `seed`.

use std::sync::Arc;

use keel_db::Database;
use keel_rng::Rng;
use keel_sql::{parse_statement, MemDb};
use keel_vfs::{BlockFile, MemDisk};

const LOG_MAGIC: u32 = 0x4B4C_4F47;

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
    assert_eq!(got.rows, want.rows, "seed {seed}: `{sql}`");
}

/// The committed statement stream is reconstructed from the durable log onto a
/// fresh (empty) data disk, across a randomized workload including an index and a
/// DROP-and-recreate.
#[test]
fn logged_recovery_reconstructs_committed_state() {
    for seed in 0..16u64 {
        let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let mut mem = MemDb::new();

        {
            let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
            let db = Database::open_logged(data, log.clone(), 512).unwrap();

            run_both(&db, &mut mem, "CREATE TABLE a (id INT, k INT, v INT)");
            run_both(&db, &mut mem, "CREATE TABLE b (id INT, k INT, w INT)");
            db.execute("CREATE INDEX ixa ON a (k)").unwrap();

            let mut rng = Rng::seed(seed ^ 0xBEEF);
            let mut next = 0i64;
            for _ in 0..25 {
                let (k, v) = (rng.below(4) as i64, rng.below(10) as i64);
                let vexpr = if rng.below(8) == 0 {
                    "NULL".to_string()
                } else {
                    v.to_string()
                };
                run_both(
                    &db,
                    &mut mem,
                    &format!("INSERT INTO a VALUES ({next}, {k}, {vexpr})"),
                );
                next += 1;
                let (bk, w) = (rng.below(4) as i64, rng.below(10) as i64);
                run_both(
                    &db,
                    &mut mem,
                    &format!("INSERT INTO b VALUES ({next}, {bk}, {w})"),
                );
                next += 1;
            }
            run_both(&db, &mut mem, "UPDATE a SET v = v + 1 WHERE k = 1");
            run_both(&db, &mut mem, "DELETE FROM a WHERE v < 2");
            run_both(&db, &mut mem, "UPDATE a SET k = 3 WHERE k = 0");
            run_both(&db, &mut mem, "DROP TABLE b");
            run_both(&db, &mut mem, "CREATE TABLE b (id INT, k INT, w INT)");
            run_both(
                &db,
                &mut mem,
                "INSERT INTO b VALUES (900, 3, 5), (901, 1, 6)",
            );
        }

        let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data2, log.clone(), 512).unwrap();

        let mut names = db.table_names();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()], "seed {seed}");
        for q in [
            "SELECT id, k, v FROM a ORDER BY id",
            "SELECT COUNT(*), SUM(v) FROM a",
            "SELECT id FROM a WHERE k = 3 ORDER BY id",
            "SELECT a.id, b.w FROM a JOIN b ON a.k = b.k ORDER BY a.id, b.w",
            "SELECT id, w FROM b ORDER BY id",
        ] {
            compare(&db, &mut mem, q, seed);
        }
    }
}

/// A committed transaction is atomic across a crash: every statement in the
/// `BEGIN … COMMIT` bracket survives, applied in order.
#[test]
fn transaction_commit_is_atomic_across_crash() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO t VALUES (2, 20)").unwrap();
        db.execute("UPDATE t SET v = v + 100 WHERE id = 1").unwrap();
        db.execute("COMMIT").unwrap();
    }
    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let rs = db
        .execute("SELECT id, v FROM t ORDER BY id")
        .unwrap()
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.rows[0][1], keel_types::Value::Int(110));
    assert_eq!(rs.rows[1][1], keel_types::Value::Int(20));
}

/// A rolled-back transaction leaves no trace; a crash before COMMIT is equivalent
/// (its buffered statements were never logged).
#[test]
fn transaction_rollback_and_uncommitted_crash_leave_no_trace() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.execute("INSERT INTO t VALUES (3)").unwrap();
        db.execute("ROLLBACK").unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (4)").unwrap();
    }
    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let rs = db.execute("SELECT id FROM t ORDER BY id").unwrap().unwrap();
    assert_eq!(rs.rows.len(), 1, "only the auto-committed row survives");
    assert_eq!(rs.rows[0][0], keel_types::Value::Int(1));
}

/// Read-your-writes: a SELECT inside an open transaction sees the transaction's own
/// buffered mutations; a SELECT *outside* it (before commit) does not; and the
/// choice of COMMIT vs ROLLBACK decides what becomes visible afterward.
#[test]
fn transaction_read_your_writes() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data, log, 256).unwrap();
    db.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (2, 20)").unwrap();
    db.execute("UPDATE t SET v = v + 5 WHERE id = 1").unwrap();
    let inside = db
        .execute("SELECT id, v FROM t ORDER BY id")
        .unwrap()
        .unwrap();
    assert_eq!(
        inside.rows.len(),
        2,
        "read-your-writes: sees the buffered insert"
    );
    assert_eq!(inside.rows[0][1], keel_types::Value::Int(15));
    assert_eq!(inside.rows[1][1], keel_types::Value::Int(20));
    db.execute("ROLLBACK").unwrap();
    let after = db
        .execute("SELECT id, v FROM t ORDER BY id")
        .unwrap()
        .unwrap();
    assert_eq!(after.rows.len(), 1);
    assert_eq!(after.rows[0][1], keel_types::Value::Int(10));

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3, 30)").unwrap();
    db.execute("COMMIT").unwrap();
    let committed = db.execute("SELECT id FROM t ORDER BY id").unwrap().unwrap();
    assert_eq!(committed.rows.len(), 2);
    assert_eq!(committed.rows[1][0], keel_types::Value::Int(3));
}

/// A crash *during* commit — the batch's statement records reached the log but the
/// commit marker did not — discards the whole batch on recovery.
#[test]
fn crash_between_statements_and_commit_marker_discards_batch() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let end = log.size().unwrap();
    let mut extra = Vec::new();
    for sql in ["INSERT INTO t VALUES (2)", "INSERT INTO t VALUES (3)"] {
        let mut payload = vec![b'S'];
        payload.extend_from_slice(sql.as_bytes());
        extra.extend_from_slice(&raw_frame(&payload));
    }
    log.write_at(&extra, end).unwrap();
    log.sync().unwrap();

    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let rs = db.execute("SELECT id FROM t ORDER BY id").unwrap().unwrap();
    assert_eq!(rs.rows.len(), 1, "the un-committed batch is discarded");
    assert_eq!(rs.rows[0][0], keel_types::Value::Int(1));
}

/// Build a StmtLog frame `[MAGIC][len][crc32(payload)][payload]`.
fn raw_frame(payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&LOG_MAGIC.to_le_bytes());
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&keel_page::crc32(payload).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// Log compaction bounds recovery: after `compact`, reopening replays only the
/// snapshot + post-compact tail (not the whole history), reconstructs the exact
/// state, and survives a crash after the compaction.
#[test]
fn compaction_bounds_recovery_and_preserves_state() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        for i in 0..60i64 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i})"))
                .unwrap();
        }
        for _ in 0..5 {
            db.execute("UPDATE t SET v = v + 1 WHERE id < 30").unwrap();
        }
        db.execute("DELETE FROM t WHERE id >= 50").unwrap();
        db.execute("CREATE INDEX ix ON t (v)").unwrap();

        db.compact().unwrap();
        db.execute("INSERT INTO t VALUES (100, 100)").unwrap();
        db.execute("UPDATE t SET v = 7 WHERE id = 0").unwrap();
    }

    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let n = db.execute("SELECT COUNT(*) FROM t").unwrap().unwrap();
    assert_eq!(n.rows[0][0], keel_types::Value::BigInt(51));
    let v0 = db.execute("SELECT v FROM t WHERE id = 0").unwrap().unwrap();
    assert_eq!(v0.rows[0][0], keel_types::Value::Int(7));
    let v100 = db
        .execute("SELECT id FROM t WHERE id = 100")
        .unwrap()
        .unwrap();
    assert_eq!(v100.rows.len(), 1);
    assert!(
        db.replay_count() < 70,
        "compaction should bound recovery replay, got {}",
        db.replay_count()
    );
}

/// Every value type and its edge cases must survive a compaction round-trip — the
/// snapshot emits each row as an `INSERT` literal (`value_to_sql`), so a mistake in
/// formatting a negative double, an i32 boundary, an embedded quote, an empty
/// string, or a NULL would corrupt the reconstructed state.
#[test]
fn compaction_round_trips_all_value_types() {
    use keel_types::Value;
    type Sample = (i64, i64, Option<f64>, bool, Option<&'static str>);
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let rows: Vec<Sample> = vec![
        (0, 9_000_000_000, Some(0.0), true, Some("hello")),
        (1, -9_000_000_000, Some(-1.5), false, Some("")),
        (
            2,
            2_147_483_647,
            Some(1000000.0),
            true,
            Some("with 'quotes' inside"),
        ),
        (3, -2_147_483_648, Some(0.001), false, Some("a,b,c")),
        (4, 42, None, true, None),
    ];
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT, big BIGINT, d DOUBLE, flag BOOL, s VARCHAR(64))")
            .unwrap();
        for (id, big, d, flag, s) in &rows {
            let dsql = d.map(|x| x.to_string()).unwrap_or_else(|| "NULL".into());
            let ssql = s
                .map(|x| format!("'{}'", x.replace('\'', "''")))
                .unwrap_or_else(|| "NULL".into());
            db.execute(&format!(
                "INSERT INTO t VALUES ({id}, {big}, {dsql}, {flag}, {ssql})"
            ))
            .unwrap();
        }
        db.compact().unwrap();
    }
    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let rs = db
        .execute("SELECT id, big, d, flag, s FROM t ORDER BY id")
        .unwrap()
        .unwrap();
    assert_eq!(rs.rows.len(), rows.len());
    for (row, (id, big, d, flag, s)) in rs.rows.iter().zip(&rows) {
        assert_eq!(row[0], Value::Int(*id as i32), "id");
        assert_eq!(row[1], Value::BigInt(*big), "big");
        assert_eq!(
            row[2],
            d.map(Value::Double).unwrap_or(Value::Null),
            "double for id {id}"
        );
        assert_eq!(row[3], Value::Bool(*flag), "bool");
        assert_eq!(
            row[4],
            s.map(|x| Value::Text(x.to_string())).unwrap_or(Value::Null),
            "text for id {id}"
        );
    }
}

/// Randomized compaction round-trip: for each seed, load a table of random values
/// of every type (arbitrary i32/i64, arbitrary f64, text with quotes and specials,
/// NULLs, bools), compact, reopen, and compare the reconstructed state to a `MemDb`
/// oracle fed the identical INSERTs. This fuzzes `value_to_sql` far past the
/// hand-picked edge cases: any value that fails to round-trip through the snapshot's
/// SQL-literal form diverges from the oracle.
#[test]
fn compaction_round_trip_random_values_vs_oracle() {
    for seed in 0..25u64 {
        let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let mut mem = MemDb::new();
        let ddl = "CREATE TABLE t (id INT, big BIGINT, d DOUBLE, flag BOOL, s VARCHAR(64))";
        {
            let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
            let db = Database::open_logged(data, log.clone(), 256).unwrap();
            db.execute(ddl).unwrap();
            mem.execute(&parse_statement(ddl).unwrap()).unwrap();

            let mut rng = Rng::seed(seed ^ 0x5A17);
            let chars: Vec<char> = "abcXYZ 0'9,_-".chars().collect();
            for id in 0..30i64 {
                let big = rng.next_u64() as i64;
                let d = loop {
                    let f = f64::from_bits(rng.next_u64());
                    if f.is_finite() {
                        break f;
                    }
                };
                let flag = rng.below(2) == 0;
                let dsql = if rng.below(9) == 0 {
                    "NULL".to_string()
                } else {
                    d.to_string()
                };
                let ssql = if rng.below(9) == 0 {
                    "NULL".to_string()
                } else {
                    let len = rng.below(8) as usize;
                    let raw: String = (0..len)
                        .map(|_| chars[rng.below(chars.len() as u64) as usize])
                        .collect();
                    format!("'{}'", raw.replace('\'', "''"))
                };
                let ins = format!("INSERT INTO t VALUES ({id}, {big}, {dsql}, {flag}, {ssql})");
                db.execute(&ins).unwrap();
                mem.execute(&parse_statement(&ins).unwrap()).unwrap();
            }
            db.compact().unwrap();
        }
        let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data2, log.clone(), 256).unwrap();
        let q = "SELECT id, big, d, flag, s FROM t ORDER BY id";
        let got = db.execute(q).unwrap().unwrap();
        let want = mem.execute(&parse_statement(q).unwrap()).unwrap().unwrap();
        assert_eq!(
            got.rows, want.rows,
            "seed {seed}: compaction round-trip diverged"
        );
    }
}

/// A crash *during* compaction (the begin marker and part of the snapshot reached
/// the log, but the closing end marker did not) is ignored on recovery: the prior
/// history still reconstructs the correct state.
#[test]
fn torn_compaction_is_ignored() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
    }
    let end = log.size().unwrap();
    let mut extra = Vec::new();
    for payload in [vec![b'B'], {
        let mut p = vec![b'S'];
        p.extend_from_slice(b"CREATE TABLE t (id INT)");
        p
    }] {
        extra.extend_from_slice(&raw_frame(&payload));
    }
    log.write_at(&extra, end).unwrap();
    log.sync().unwrap();

    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let rs = db.execute("SELECT id FROM t ORDER BY id").unwrap().unwrap();
    assert_eq!(
        rs.rows.len(),
        2,
        "torn compaction ignored; original state intact"
    );
}

/// A statement whose log frame is torn (a crash mid-append, before fsync returns)
/// is dropped whole on recovery — never half-applied. We simulate the torn tail by
/// writing a partial frame after the committed records.
#[test]
fn torn_log_tail_is_dropped_atomically() {
    let log = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    {
        let data = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open_logged(data, log.clone(), 256).unwrap();
        db.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO t VALUES (2, 20)").unwrap();
    }
    let end = log.size().unwrap();
    let mut torn = Vec::new();
    torn.extend_from_slice(&LOG_MAGIC.to_le_bytes());
    torn.extend_from_slice(&1000u32.to_le_bytes());
    torn.extend_from_slice(&0u32.to_le_bytes());
    torn.extend_from_slice(b"INSERT INTO t VALUES (3, 30)");
    log.write_at(&torn, end).unwrap();
    log.sync().unwrap();

    let data2 = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open_logged(data2, log.clone(), 256).unwrap();
    let rs = db
        .execute("SELECT id, v FROM t ORDER BY id")
        .unwrap()
        .unwrap();
    assert_eq!(rs.rows.len(), 2, "torn tail must not add a row");
    assert_eq!(
        rs.rows[1][0],
        keel_types::Value::Int(2),
        "only committed rows 1 and 2 survive"
    );
}
