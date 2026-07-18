use std::time::Instant;

use keel_rng::Rng;
use keel_sql::parse_expr;
use keel_sql::refengine::eval_public;
use keel_types::{ColumnType, Value};
use keel_vexec::{filter, Batch};

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let reps: usize = 9;

    println!("KEEL tuple-vs-vector ablation — {n} rows, {reps} reps (§9)\n");

    let names: Vec<(String, String)> = ["a", "b", "f"]
        .iter()
        .map(|c| ("t".to_string(), c.to_string()))
        .collect();
    let types = [ColumnType::Int, ColumnType::Int, ColumnType::Double];

    let mut rng = Rng::seed(0xB0A7);
    let rows: Vec<Vec<Value>> = (0..n)
        .map(|_| {
            vec![
                Value::Int(rng.below(100) as i32),
                Value::Int(rng.below(100) as i32),
                Value::Double(rng.below(100) as f64),
            ]
        })
        .collect();
    let batch = Batch::from_rows(names.clone(), &rows);
    let _ = types;

    let pred = parse_expr("a > 20 AND b < 80 AND f >= 10").unwrap();

    let mut row_ms = Vec::new();
    let mut row_count = 0usize;
    for _ in 0..reps {
        let t = Instant::now();
        let mut c = 0usize;
        for row in &rows {
            if matches!(eval_public(&pred, &names, row), Ok(Value::Bool(true))) {
                c += 1;
            }
        }
        row_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        row_count = c;
    }

    let mut vec_ms = Vec::new();
    let mut vec_count = 0usize;
    for _ in 0..reps {
        let t = Instant::now();
        let out = filter(&pred, &batch).unwrap();
        vec_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        vec_count = out.len;
    }

    assert_eq!(
        row_count, vec_count,
        "engines disagree — the ablation is invalid"
    );

    let (row_med, row_mad) = median_mad(&mut row_ms);
    let (vec_med, vec_mad) = median_mad(&mut vec_ms);
    let row_tput = n as f64 / (row_med / 1000.0) / 1e6;
    let vec_tput = n as f64 / (vec_med / 1000.0) / 1e6;

    println!(
        "predicate: a > 20 AND b < 80 AND f >= 10   ({row_count} rows pass, {:.1}%)",
        100.0 * row_count as f64 / n as f64
    );
    println!(
        "{:<16} {:>10} {:>10} {:>14}",
        "engine", "ms (med)", "± MAD", "M rows/s"
    );
    println!("{}", "-".repeat(54));
    println!(
        "{:<16} {:>10.2} {:>10.2} {:>14.1}",
        "row-at-a-time", row_med, row_mad, row_tput
    );
    println!(
        "{:<16} {:>10.2} {:>10.2} {:>14.1}",
        "vectorized", vec_med, vec_mad, vec_tput
    );
    println!("{}", "-".repeat(54));
    println!("speedup (vector / row): {:.2}x", row_med / vec_med);
    println!("\nNote: KEEL's Value is a tagged enum, so this understates a true\ncolumnar engine (native arrays, SIMD); it isolates the dispatch-\namortization effect only — the honest, in-scope part of §9.");
}

fn median_mad(xs: &mut [f64]) -> (f64, f64) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = xs[xs.len() / 2];
    let mut dev: Vec<f64> = xs.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (med, dev[dev.len() / 2])
}
