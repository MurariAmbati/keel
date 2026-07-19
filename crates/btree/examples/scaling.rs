//! Insert throughput against thread count — the baseline any latch protocol must beat.
//!
//! The whole-tree `RwLock` is correct and buys nothing: it was measured at 0.77s single-threaded
//! versus 0.81s on four threads. That is one data point, and one data point cannot distinguish
//! "no speedup" from "actively degrading under contention", which are different problems with
//! different fixes. This prints the curve.
//!
//! Run with `cargo run -p keel-btree --example scaling --release`. Debug timings are dominated
//! by bounds checks and are not worth comparing.

use std::sync::{Arc, Barrier};
use std::time::Instant;

use keel_btree::BTree;
use keel_cbuffer::{NoWal, PageCache, PageFormat};
use keel_heap::Rid;
use keel_vfs::{BlockFile, MemDisk};

const TOTAL: usize = 24_000;

fn key(n: usize) -> [u8; 8] {
    (n as u64).to_be_bytes()
}

fn run(threads: usize) -> f64 {
    let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let cache = PageCache::open_formatted(disk, 1024, Arc::new(NoWal), PageFormat::keel_page());
    let tree = BTree::create(&cache).expect("create");
    let per = TOTAL / threads;
    let barrier = Arc::new(Barrier::new(threads));

    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            let barrier = Arc::clone(&barrier);
            let tree = &tree;
            s.spawn(move || {
                barrier.wait();
                for i in 0..per {
                    let n = i * threads + t;
                    tree.insert(
                        &key(n),
                        Rid {
                            page: n as u32,
                            slot: 0,
                        },
                    )
                    .expect("insert");
                }
            });
        }
    });
    start.elapsed().as_secs_f64()
}

fn main() {
    println!("{TOTAL} inserts, shared tree, times are the best of 3\n");
    println!(
        "{:>7}  {:>9}  {:>12}  {:>8}",
        "threads", "seconds", "inserts/sec", "speedup"
    );

    let mut base = 0.0;
    for threads in [1usize, 2, 4, 8] {
        let mut best = f64::MAX;
        for _ in 0..3 {
            best = best.min(run(threads));
        }
        if threads == 1 {
            base = best;
        }
        println!(
            "{threads:>7}  {best:>9.3}  {:>12.0}  {:>7.2}x",
            TOTAL as f64 / best,
            base / best
        );
    }

    println!(
        "\nA speedup column that stays near 1.00x is the whole-tree lock doing exactly what it \n\
         was measured to do. Below 1.00x means contention is making it actively worse than \n\
         running single-threaded, which is a stronger argument for fine-grained latching than \n\
         'no speedup' alone."
    );
}
