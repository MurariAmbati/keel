//! Differential test (§7.1): the storage-backed `Database` must return exactly
//! what the in-memory reference engine returns for the same data and queries.
//! This validates the storage roundtrip — record encode/decode, the self-hosting
//! catalog, and the heap scan — against the trusted semantic oracle.
//!
//! Replays from `seed`.

use std::sync::Arc;

use keel_db::Database;
use keel_rng::Rng;
use keel_sql::gen::{random_predicate, random_value, Gen};
use keel_sql::parse_statement;
use keel_sql::refengine::MemDb;
use keel_sql::{Expr, FromClause, OrderKey, Select, SelectItem, TableRef};
use keel_types::ColumnType;
use keel_vfs::{BlockFile, MemDisk};

fn run_both(db: &Database, mem: &mut MemDb, sql: &str) {
    let a = db
        .execute(sql)
        .unwrap_or_else(|e| panic!("db failed on `{sql}`: {e}"));
    let stmt = parse_statement(sql).unwrap();
    let b = mem
        .execute(&stmt)
        .unwrap_or_else(|e| panic!("mem failed on `{sql}`: {e}"));
    match (a, b) {
        (Some(ra), Some(rb)) => {
            assert_eq!(ra.columns, rb.columns, "column mismatch for `{sql}`");
            assert_eq!(ra.rows, rb.rows, "row mismatch for `{sql}`");
        }
        (None, None) => {}
        _ => panic!("one engine produced a result set and the other didn't for `{sql}`"),
    }
}

#[test]
fn storage_matches_reference_engine() {
    for seed in 0..12u64 {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open(disk, 16).unwrap();
        let mut mem = MemDb::new();
        let mut rng = Rng::seed(seed);

        for ddl in [
            "CREATE TABLE emp (id BIGINT, dept INT, sal DOUBLE, name VARCHAR(16), active BOOL)",
            "CREATE TABLE dept (id INT, budget BIGINT)",
        ] {
            db.execute(ddl).unwrap();
            mem.execute(&parse_statement(ddl).unwrap()).unwrap();
        }

        let names = ["ann", "bob", "cy", "dee"];
        for i in 0..60u64 {
            let dept = rng.below(4) as i64;
            let sal = if rng.one_in(7) {
                "NULL".to_string()
            } else {
                format!("{}.5", rng.range(30, 120))
            };
            let nm = names[rng.below(names.len() as u64) as usize];
            let active = if rng.chance(0.5) { "true" } else { "false" };
            let sql = format!("INSERT INTO emp VALUES ({i}, {dept}, {sal}, '{nm}', {active})");
            db.execute(&sql).unwrap();
            mem.execute(&parse_statement(&sql).unwrap()).unwrap();
        }
        for d in 0..4u64 {
            let sql = format!("INSERT INTO dept VALUES ({d}, {})", rng.range(1000, 9000));
            db.execute(&sql).unwrap();
            mem.execute(&parse_statement(&sql).unwrap()).unwrap();
        }

        let queries = [
            "SELECT id, name FROM emp WHERE sal > 80 ORDER BY id",
            "SELECT id FROM emp WHERE sal IS NULL ORDER BY id",
            "SELECT id FROM emp WHERE sal IS NOT NULL AND active ORDER BY id",
            "SELECT dept, COUNT(*), SUM(sal), MIN(sal), MAX(sal) FROM emp GROUP BY dept ORDER BY dept",
            "SELECT dept FROM emp GROUP BY dept HAVING COUNT(*) > 5 ORDER BY dept",
            "SELECT DISTINCT name FROM emp ORDER BY name",
            "SELECT name, CASE WHEN sal >= 90 THEN 'hi' ELSE 'lo' END FROM emp WHERE id < 10 ORDER BY id",
            "SELECT id FROM emp WHERE dept IN (1, 3) ORDER BY id",
            "SELECT id FROM emp WHERE dept NOT IN (0) ORDER BY id",
            "SELECT emp.id, dept.budget FROM emp JOIN dept ON emp.dept = dept.id ORDER BY emp.id",
            "SELECT emp.id, dept.budget FROM emp LEFT JOIN dept ON emp.dept = dept.id ORDER BY emp.id",
            "SELECT id FROM emp WHERE sal = (SELECT MAX(sal) FROM emp) ORDER BY id",
            "SELECT AVG(sal) FROM emp",
            "SELECT COUNT(DISTINCT dept) FROM emp",
            "SELECT id FROM emp ORDER BY sal DESC, id LIMIT 5",
        ];
        for q in queries {
            run_both(&db, &mut mem, q);
        }
    }
}

/// Two independent engines, random predicates: the storage engine's **streaming
/// (Volcano) executor** vs the **reference engine**, over random NULL-heavy
/// predicates (§7.1). `SELECT * FROM t WHERE <p> ORDER BY id` is streaming-
/// eligible, so this directly differentials the two executors.
#[test]
fn streaming_executor_matches_reference_engine() {
    let cols: Vec<(String, ColumnType)> = vec![
        ("id".into(), ColumnType::BigInt),
        ("a".into(), ColumnType::Int),
        ("b".into(), ColumnType::Int),
        ("f".into(), ColumnType::Double),
        ("s".into(), ColumnType::Varchar(4)),
        ("bo".into(), ColumnType::Bool),
    ];
    for seed in 0..80u64 {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open(disk, 16).unwrap();
        let mut mem = MemDb::new();
        let ddl = "CREATE TABLE t (id BIGINT, a INT, b INT, f DOUBLE, s VARCHAR(4), bo BOOL)";
        db.execute(ddl).unwrap();
        mem.execute(&parse_statement(ddl).unwrap()).unwrap();

        let mut g = Gen::new(seed);
        for i in 0..50u64 {
            let a = fmt_val(random_value(&mut g, ColumnType::Int, true));
            let b = fmt_val(random_value(&mut g, ColumnType::Int, true));
            let f = fmt_val(random_value(&mut g, ColumnType::Double, true));
            let s = match random_value(&mut g, ColumnType::Varchar(4), true) {
                keel_types::Value::Null => "NULL".to_string(),
                keel_types::Value::Text(t) => format!("'{t}'"),
                _ => unreachable!(),
            };
            let bo = fmt_val(random_value(&mut g, ColumnType::Bool, true));
            let sql = format!("INSERT INTO t VALUES ({i}, {a}, {b}, {f}, {s}, {bo})");
            db.execute(&sql).unwrap();
            mem.execute(&parse_statement(&sql).unwrap()).unwrap();
        }

        for _ in 0..15 {
            let p = random_predicate(&mut g, &cols, 3);
            let q = Select {
                distinct: false,
                items: vec![SelectItem::Wildcard],
                from: Some(FromClause {
                    first: TableRef {
                        table: "t".into(),
                        alias: None,
                    },
                    joins: Vec::new(),
                }),
                filter: Some(p.clone()),
                group_by: Vec::new(),
                having: None,
                order_by: vec![OrderKey {
                    expr: Expr::col("id"),
                    asc: true,
                }],
                limit: None,
            };
            let a = db.query(&q).unwrap();
            let b = mem.query(&q).unwrap();
            assert_eq!(
                a.rows, b.rows,
                "seed {seed}: streaming vs reference diverged for {p:?}"
            );
        }
    }
}

fn fmt_val(v: keel_types::Value) -> String {
    use keel_types::Value;
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::Double(d) => format!("{d}"),
        Value::Text(t) => format!("'{t}'"),
    }
}
