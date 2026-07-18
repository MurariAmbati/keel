//! Race oracle for the concurrent CLOCK page cache (D-LATCH `ClockPool`).
//!
//! This is the whole protocol under load: more distinct pages than frames, so
//! threads continuously evict one another's *unpinned* pages, while the invariant
//! is that a *pinned* page is never evicted. Each thread reads every page it
//! acquires and checks the value equals the id — so any race between victim
//! selection and pinning (a frame reloaded with a different page while pinned)
//! shows up as a mismatched value, not as silent corruption.

use keel_latch::ClockPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn concurrent_clock_pool_keeps_pinned_pages_correct() {
    const CAP: usize = 8;
    const THREADS: usize = 6;
    const PAGES: u32 = 24;
    const ITERS: usize = 30_000;

    let pool: Arc<ClockPool<i64>> = Arc::new(ClockPool::new(CAP));
    let acquired = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let pool = pool.clone();
        let acquired = acquired.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0xA076_1D64_78BD_642Fu64 ^ ((tid as u64 + 1) << 48);
            for _ in 0..ITERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let pid = (s % PAGES as u64) as u32;
                let g = pool
                    .acquire(pid, || pid as i64)
                    .expect("THREADS < CAP: an unpinned victim always exists");
                assert_eq!(
                    g.value(),
                    pid as i64,
                    "a pinned page was evicted and reloaded under us"
                );
                assert_eq!(g.pid(), pid);
                acquired.fetch_add(1, Ordering::Relaxed);
                drop(g);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        acquired.load(Ordering::Relaxed),
        (THREADS * ITERS) as u64,
        "every acquire succeeded"
    );
    assert!(
        pool.loads() > CAP as u64,
        "eviction never happened — the test would be vacuous"
    );
    assert!(pool.evictions() > 0, "replacement was never exercised");
    assert_eq!(pool.live_pins(), 0, "all pins released");
}
