use std::sync::Arc;
use std::time::Instant;

use keel_db::Database;
use keel_rng::Rng;
use keel_vfs::{BlockFile, MemDisk};

fn main() {
    let lineitems: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let orders = (lineitems / 4).max(1);
    let customers = (orders / 10).max(1);
    let reps = 7;

    println!("KEEL TPC-H-subset benchmark (§8)");
    println!("  customer={customers}  orders={orders}  lineitem={lineitems}  reps={reps}\n");

    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let db = Database::open(disk, 1024).unwrap();
    for ddl in [
        "CREATE TABLE customer (c_id INT, c_seg INT)",
        "CREATE TABLE orders (o_id INT, o_cust INT)",
        "CREATE TABLE lineitem (l_id INT, l_order INT, l_qty INT, l_price INT, l_discount INT, l_flag INT)",
    ] {
        db.execute(ddl).unwrap();
    }

    let load = Instant::now();
    let mut rc = Rng::seed(0xC0FFEE);
    bulk(&db, "customer", customers, 200, |i| {
        format!("({i}, {})", rc.below(5))
    });
    let mut ro = Rng::seed(0x0DE);
    bulk(&db, "orders", orders, 200, |i| {
        format!("({i}, {})", ro.below(customers as u64))
    });
    let mut rl = Rng::seed(0x11FE);
    bulk(&db, "lineitem", lineitems, 100, |i| {
        format!(
            "({i}, {}, {}, {}, {}, {})",
            rl.below(orders as u64),
            1 + rl.below(50),
            100 + rl.below(900),
            rl.below(10),
            rl.below(3)
        )
    });
    println!("load: {:.0} ms\n", load.elapsed().as_secs_f64() * 1000.0);

    time(
        &db,
        "Q6 filtered SUM",
        "SELECT SUM(l_price) FROM lineitem WHERE l_discount >= 2 AND l_discount <= 4 AND l_qty < 25",
        reps,
    );
    time(
        &db,
        "Q1 group aggregate",
        "SELECT l_flag, COUNT(*), SUM(l_qty), SUM(l_price), AVG(l_qty) FROM lineitem \
         GROUP BY l_flag ORDER BY l_flag",
        reps,
    );
    time(
        &db,
        "Q3 join+agg",
        "SELECT o.o_id, SUM(l.l_price) FROM customer c JOIN orders o ON c.c_id = o.o_cust \
         JOIN lineitem l ON o.o_id = l.l_order WHERE c.c_seg = 3 \
         GROUP BY o.o_id ORDER BY o.o_id LIMIT 10",
        reps,
    );

    println!(
        "\njoin-stream queries: {}   aggregate-stream queries: {}",
        db.join_streams(),
        db.agg_streams()
    );
}

fn bulk(
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

fn time(db: &Database, label: &str, sql: &str, reps: usize) {
    let mut ms = Vec::new();
    let mut rows = 0usize;
    for _ in 0..reps {
        let t = Instant::now();
        let rs = db.execute(sql).unwrap().unwrap();
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        rows = rs.rows.len();
    }
    let (med, mad) = median_mad(&mut ms);
    println!("{label:<20} {med:>8.2} ms ± {mad:>6.2}   ({rows} rows out)");
}

fn median_mad(xs: &mut [f64]) -> (f64, f64) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = xs[xs.len() / 2];
    let mut dev: Vec<f64> = xs.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (med, dev[dev.len() / 2])
}
