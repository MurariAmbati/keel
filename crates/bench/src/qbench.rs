use std::sync::Arc;
use std::time::Instant;

use keel_db::Database;
use keel_rng::Rng;
use keel_vfs::{BlockFile, MemDisk};

fn main() {
    let orders: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let lines_per_order = 2usize;
    let lineitems = orders * lines_per_order;
    let reps = 7;

    println!("KEEL end-to-end query benchmark (§8)");
    println!("  orders={orders}  lineitem={lineitems}  reps={reps}\n");

    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open(disk, 512).unwrap();
    db.execute("CREATE TABLE orders (o_id INT, o_cust INT, o_total INT)")
        .unwrap();
    db.execute("CREATE TABLE lineitem (l_id INT, l_order INT, l_qty INT)")
        .unwrap();

    let load = Instant::now();
    let mut rng = Rng::seed(0x0DDB);
    let n_cust = (orders / 10).max(1) as u64;
    bulk_insert(&db, "orders", orders, 200, |i| {
        format!("({i}, {}, {})", rng.below(n_cust), rng.below(1000))
    });
    let mut rng2 = Rng::seed(0x11FE);
    bulk_insert(&db, "lineitem", lineitems, 200, |i| {
        format!("({i}, {}, {})", (i as u64) % orders as u64, rng2.below(50))
    });
    db.execute("CREATE INDEX ix_cust ON orders (o_cust)")
        .unwrap();
    db.analyze().unwrap();
    println!(
        "load + index + analyze: {:.0} ms\n",
        load.elapsed().as_secs_f64() * 1000.0
    );

    let scan_target = orders;
    time_query(
        &db,
        "filtered scan",
        "SELECT o_id FROM orders WHERE o_total > 900",
        scan_target,
        reps,
    );
    time_query(
        &db,
        "indexed lookup",
        "SELECT o_id, o_total FROM orders WHERE o_cust = 7",
        orders,
        reps,
    );
    time_query(
        &db,
        "hash join",
        "SELECT o_id, l_qty FROM orders JOIN lineitem ON orders.o_id = lineitem.l_order",
        orders + lineitems,
        reps,
    );

    println!("\nindex point-lookups served: {}", db.index_lookups());
    println!("hash-join queries served:   {}", db.join_streams());
}

fn bulk_insert(
    db: &Database,
    table: &str,
    count: usize,
    batch: usize,
    mut row: impl FnMut(usize) -> String,
) {
    let mut i = 0;
    while i < count {
        let hi = (i + batch).min(count);
        let mut sql = format!("INSERT INTO {table} VALUES ");
        for j in i..hi {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&row(j));
        }
        db.execute(&sql).unwrap();
        i = hi;
    }
}

fn time_query(db: &Database, label: &str, sql: &str, input_rows: usize, reps: usize) {
    let mut ms = Vec::new();
    let mut out_rows = 0usize;
    for _ in 0..reps {
        let t = Instant::now();
        let rs = db.execute(sql).unwrap().unwrap();
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        out_rows = rs.rows.len();
    }
    let (med, mad) = median_mad(&mut ms);
    let tput = input_rows as f64 / (med / 1000.0) / 1e6;
    println!(
        "{label:<16} {med:>8.2} ms ± {mad:>6.2}   {tput:>8.1} M input-rows/s   ({out_rows} rows out)"
    );
}

fn median_mad(xs: &mut [f64]) -> (f64, f64) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = xs[xs.len() / 2];
    let mut dev: Vec<f64> = xs.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (med, dev[dev.len() / 2])
}
