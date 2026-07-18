use std::process::ExitCode;

use keel_dbcheck::check_file;
use keel_vfs::OsFile;

fn main() -> ExitCode {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: dbcheck <data-file>");
            return ExitCode::from(2);
        }
    };
    let file = match OsFile::open_readonly(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dbcheck: cannot open {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let report = match check_file(&file) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dbcheck: io error: {e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "pages={} heap_pages={} tuples={} stubs={} targets={} violations={}",
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
        println!("OK");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
