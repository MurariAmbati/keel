//! Race oracle for the pin/evict handshake (D-LATCH-1 `Frame`).
//!
//! The property under test is the one the buffer's `choose_victim` cannot
//! guarantee today: a frame is **never evicted while pinned**, and **never
//! pinned after eviction begins**. Because the pin count and the `evicted`
//! tombstone live under one mutex, the two decisions are atomic; if they were
//! ever split across separate locks a victim could be chosen the instant another
//! thread pinned it, and this test would see a pinned-yet-evicted frame.

use keel_latch::Frame;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn pin_and_evict_never_overlap() {
    const FRAMES: usize = 32;
    const PINNERS: usize = 6;
    const ITERS: usize = 20_000;

    let frames: Arc<Vec<Frame<i64>>> = Arc::new(
        (0..FRAMES)
            .map(|i| Frame::new(i as u32, i as i64))
            .collect(),
    );
    let violations = Arc::new(AtomicU64::new(0));
    let evicted = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    for tid in 0..PINNERS {
        let frames = frames.clone();
        let violations = violations.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0xD1B5_4A32_D192_ED03u64 ^ ((tid as u64 + 1) << 40);
            for _ in 0..ITERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let idx = (s as usize) % FRAMES;
                if let Some(g) = frames[idx].pin() {
                    if frames[idx].is_evicted() {
                        violations.fetch_add(1, Ordering::Relaxed);
                    }
                    assert_eq!(*g.latch().read(), idx as i64);
                    drop(g);
                }
            }
        }));
    }

    {
        let frames = frames.clone();
        let evicted = evicted.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0x2545_F491_4F6C_DD1Du64;
            for _ in 0..ITERS * PINNERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let idx = (s as usize) % FRAMES;
                if frames[idx].try_evict() {
                    evicted.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        violations.load(Ordering::Relaxed),
        0,
        "a frame was observed evicted while a pin was held"
    );
    assert!(
        evicted.load(Ordering::Relaxed) > 0,
        "the evictor made no progress — the test would be vacuous"
    );
    let tombstoned = frames.iter().filter(|f| f.is_evicted()).count() as u64;
    assert_eq!(
        tombstoned,
        evicted.load(Ordering::Relaxed),
        "each successful eviction tombstones exactly one frame"
    );
    for f in frames.iter() {
        assert_eq!(f.pin_count(), 0, "no pin was leaked");
    }
}
