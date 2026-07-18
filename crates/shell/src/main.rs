use std::process::ExitCode;
use std::sync::Arc;

use keel_buffer::BufferPool;
use keel_db::Database;
use keel_dbcheck::check_file;
use keel_heap::HeapFile;
use keel_types::{decode_record, encode_record, ColumnDef, ColumnType, Schema, Value};
use keel_vfs::{BlockFile, OsFile};

fn demo_schema() -> Schema {
    Schema::new(vec![
        ColumnDef::new("id", ColumnType::BigInt, true),
        ColumnDef::new("name", ColumnType::Varchar(32), false),
        ColumnDef::new("score", ColumnType::Double, false),
        ColumnDef::new("active", ColumnType::Bool, false),
    ])
}

fn demo(path: &str, n: i64) -> Result<(), Box<dyn std::error::Error>> {
    let schema = demo_schema();

    let _ = std::fs::remove_file(path);
    let file: Arc<dyn BlockFile> = Arc::new(OsFile::open(path)?);

    {
        let bp = BufferPool::open_default(file.clone(), 64)?;
        let heap = HeapFile::open(&bp)?;
        for i in 0..n {
            let row = vec![
                Value::BigInt(i),
                Value::Text(format!("row-{i:05}")),
                Value::Double((i as f64) * 1.5),
                Value::Bool(i % 2 == 0),
            ];
            let rec = encode_record(&schema, &row)?;
            heap.insert(&rec)?;
        }
        bp.checkpoint()?;
        println!(
            "wrote {n} rows across {} pages; buffer {:?}",
            bp.page_count(),
            bp.stats()
        );
    }

    {
        let bp = BufferPool::open_default(file.clone(), 64)?;
        let heap = HeapFile::open(&bp)?;
        let rows = heap.scan()?;
        println!("scanned {} rows back", rows.len());
        for (rid, rec) in rows.iter().take(3) {
            let vals = decode_record(&schema, rec)?;
            let printed: Vec<String> = vals.iter().map(|v| v.to_string()).collect();
            println!("  {rid:?} => ({})", printed.join(", "));
        }
        if rows.len() as i64 != n {
            return Err(format!("scan returned {} rows, expected {n}", rows.len()).into());
        }
    }

    let report = check_file(&*file)?;
    println!(
        "dbcheck: pages={} heap_pages={} tuples={} stubs={} targets={} violations={}",
        report.pages,
        report.heap_pages,
        report.tuples,
        report.stubs,
        report.targets,
        report.violations.len()
    );
    for v in &report.violations {
        println!("  VIOLATION: {v:?}");
    }
    if report.ok() {
        println!("dbcheck: OK");
        Ok(())
    } else {
        Err("dbcheck found violations".into())
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "demo".to_string());
    match cmd.as_str() {
        "demo" => {
            let path = args.next().unwrap_or_else(|| "keel-demo.db".to_string());
            let n: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5000);
            match demo(&path, n) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("demo failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "check" => {
            let path = match args.next() {
                Some(p) => p,
                None => {
                    eprintln!("usage: keel check <file>");
                    return ExitCode::from(2);
                }
            };
            match OsFile::open_readonly(&path).map(|f| check_file(&f)) {
                Ok(Ok(r)) => {
                    println!("{r:?}");
                    if r.ok() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                _ => ExitCode::from(2),
            }
        }
        "sql" => {
            let path = args.next().unwrap_or_else(|| "keel-sql.db".to_string());
            match sql_repl(&path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("sql session failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "sqllog" => {
            let data = args.next().unwrap_or_else(|| "keel-log.db".to_string());
            let log = args.next().unwrap_or_else(|| "keel-log.wal".to_string());
            match sql_logged(&data, &log) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("sqllog session failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!("keel: unknown command '{other}' (try: demo, check, sql, sqllog)");
            ExitCode::from(2)
        }
    }
}

fn sql_repl(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    let file: Arc<dyn BlockFile> = Arc::new(OsFile::open(path)?);
    let db = Database::open(file, 256)?;

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    for stmt in input.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        match db.execute(stmt) {
            Ok(Some(rs)) => print_table(&rs),
            Ok(None) => println!("OK"),
            Err(e) => eprintln!("error: {e}"),
        }
    }
    Ok(())
}

fn sql_logged(data: &str, log: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    let data_file: Arc<dyn BlockFile> = Arc::new(OsFile::open(data)?);
    let log_file: Arc<dyn BlockFile> = Arc::new(OsFile::open(log)?);
    let db = Database::open_logged(data_file, log_file, 256)?;
    if db.replay_count() > 0 {
        println!("recovered: replayed {} log record(s)", db.replay_count());
    }

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    for stmt in input.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        match stmt {
            ".compact" => match db.compact() {
                Ok(()) => println!("compacted"),
                Err(e) => eprintln!("error: {e}"),
            },
            ".analyze" => match db.analyze() {
                Ok(()) => println!("analyzed"),
                Err(e) => eprintln!("error: {e}"),
            },
            _ => match db.execute(stmt) {
                Ok(Some(rs)) => print_table(&rs),
                Ok(None) => println!("OK"),
                Err(e) => eprintln!("error: {e}"),
            },
        }
    }
    Ok(())
}

fn print_table(rs: &keel_db::ResultSet) {
    let mut widths: Vec<usize> = rs.columns.iter().map(|c| c.len()).collect();
    let cells: Vec<Vec<String>> = rs
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.len());
        }
    }
    let sep: String = widths
        .iter()
        .map(|w| "-".repeat(w + 2))
        .collect::<Vec<_>>()
        .join("+");
    let fmt_row = |cols: &[String]| -> String {
        cols.iter()
            .enumerate()
            .map(|(i, c)| format!(" {c:<width$} ", width = widths[i]))
            .collect::<Vec<_>>()
            .join("|")
    };
    println!("{}", fmt_row(&rs.columns));
    println!("{sep}");
    for row in &cells {
        println!("{}", fmt_row(row));
    }
    println!(
        "({} row{})",
        rs.rows.len(),
        if rs.rows.len() == 1 { "" } else { "s" }
    );
}
