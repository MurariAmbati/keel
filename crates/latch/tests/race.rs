use keel_latch::{write_two_ordered, LatchTable};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn get_or_install_is_atomic_under_races() {
    const THREADS: usize = 8;
    const ITERS: usize = 5_000;
    const KEYS: u32 = 4;

    let table: Arc<LatchTable<i64>> = Arc::new(LatchTable::new());
    let make_calls = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let table = table.clone();
        let make_calls = make_calls.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ ((tid as u64 + 1) << 32);
            for _ in 0..ITERS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let pid = (s % KEYS as u64) as u32;
                let latch = table.get_or_install(pid, || {
                    make_calls.fetch_add(1, Ordering::Relaxed);
                    0i64
                });
                assert_eq!(latch.id(), pid, "installed latch must guard the asked page");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        table.installs(),
        KEYS as u64,
        "exactly one install per distinct key — no double-install under races"
    );
    assert_eq!(
        make_calls.load(Ordering::Relaxed),
        KEYS as u64,
        "the make closure ran exactly once per key"
    );
    assert_eq!(table.len(), KEYS as usize);
}

#[test]
fn exclusive_latch_serializes_writes() {
    const THREADS: i64 = 8;
    const INCR: i64 = 20_000;

    let table: LatchTable<AtomicI64> = LatchTable::new();
    let latch = table.get_or_install(0, || AtomicI64::new(0));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let latch = latch.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..INCR {
                let g = latch.write();
                let v = g.load(Ordering::Relaxed);
                g.store(v + 1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        latch.read().load(Ordering::Relaxed),
        THREADS * INCR,
        "no lost updates -> the write latch excluded all concurrent writers"
    );
}

#[test]
fn shared_latches_are_concurrent() {
    const READERS: usize = 6;

    let table: LatchTable<i64> = LatchTable::new();
    let latch = table.get_or_install(0, || 77i64);
    let barrier = Arc::new(Barrier::new(READERS));
    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..READERS {
        let latch = latch.clone();
        let barrier = barrier.clone();
        let current = current.clone();
        let peak = peak.clone();
        handles.push(thread::spawn(move || {
            let g = latch.read();
            assert_eq!(*g, 77, "readers all see the committed value");
            let now = current.fetch_add(1, Ordering::SeqCst) + 1;
            let mut m = peak.load(Ordering::SeqCst);
            while now > m {
                match peak.compare_exchange(m, now, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(seen) => m = seen,
                }
            }
            barrier.wait();
            current.fetch_sub(1, Ordering::SeqCst);
            drop(g);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        peak.load(Ordering::SeqCst),
        READERS,
        "every reader held the shared latch simultaneously"
    );
}

#[test]
fn write_two_ordered_never_deadlocks_and_conserves() {
    const ROUNDS: i64 = 50_000;
    const START: i64 = 1_000_000;

    let table: LatchTable<i64> = LatchTable::new();
    let a = table.get_or_install(1, || START);
    let b = table.get_or_install(2, || START);

    let t1 = {
        let (a, b) = (a.clone(), b.clone());
        thread::spawn(move || {
            for _ in 0..ROUNDS {
                let (mut ga, mut gb) = write_two_ordered(&a, &b);
                *ga -= 1;
                *gb += 1;
            }
        })
    };
    let t2 = {
        let (a, b) = (a.clone(), b.clone());
        thread::spawn(move || {
            for _ in 0..ROUNDS {
                let (mut gb, mut ga) = write_two_ordered(&b, &a);
                *gb -= 1;
                *ga += 1;
            }
        })
    };
    t1.join().unwrap();
    t2.join().unwrap();

    let (fa, fb) = (*a.read(), *b.read());
    assert_eq!(fa + fb, 2 * START, "money conserved across paired updates");
    assert_eq!(
        fa, START,
        "t1 sent ROUNDS a->b, t2 sent ROUNDS b->a — net zero"
    );
    assert_eq!(fb, START);
}
