//! The lock manager under **real threads** (P7, §5.1) — the single-threaded
//! scripted tests prove the matrix and the deadlock detector in isolation; this
//! proves the whole thing works as a concurrency-control service when many OS
//! threads contend for it.
//!
//! A bank of accounts is protected *only* by the lock manager (the data itself has
//! no mutex — each account is an atomic, and correctness rests on the manager
//! granting no two conflicting X locks at once). Threads run random transfers; if
//! the manager is correct, money is conserved exactly. Two acquisition disciplines
//! are exercised: **sorted** (canonical order — deadlock-free) and **random**
//! (deadlocks form and the detector must break them, the transaction retrying),
//! and both must conserve the total.
//!
//! The synchronization is real: every `lock`/`release_all` goes through the shared
//! `Mutex<LockManager>`, whose lock/unlock ordering establishes happens-before
//! between a releaser and the next acquirer, so the atomic account reads/writes in
//! the critical section are correctly ordered when the manager enforces exclusion.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use keel_lockmgr::{Grant, LockManager, Mode, Resource, TxnId};

const ACCOUNTS: usize = 8;
const THREADS: usize = 8;
const TRANSFERS: usize = 400;
const START: i64 = 1000;

struct Bank {
    lm: Mutex<LockManager>,
    accounts: Vec<AtomicI64>,
    next_txn: AtomicU64,
}

impl Bank {
    fn new() -> Self {
        Bank {
            lm: Mutex::new(LockManager::new()),
            accounts: (0..ACCOUNTS).map(|_| AtomicI64::new(START)).collect(),
            next_txn: AtomicU64::new(1),
        }
    }

    fn fresh_txn(&self) -> TxnId {
        self.next_txn.fetch_add(1, Ordering::Relaxed)
    }

    /// Acquire an X lock on `res` for `txn`. `true` = held; `false` = the request
    /// would deadlock (caller aborts and retries). A `Waiting` grant spins until the
    /// holder releases and the FIFO queue promotes us.
    fn acquire(&self, txn: TxnId, res: Resource) -> bool {
        let grant = self.lm.lock().unwrap().lock(txn, res, Mode::X);
        match grant {
            Grant::Granted => true,
            Grant::Deadlock { .. } => false,
            Grant::Waiting => loop {
                if self
                    .lm
                    .lock()
                    .unwrap()
                    .holders(res)
                    .iter()
                    .any(|(t, _)| *t == txn)
                {
                    return true;
                }
                thread::yield_now();
            },
        }
    }

    /// Transfer `amt` from account `from` to `to` under 2PL. `sorted` picks the
    /// lock-acquisition order: canonical (deadlock-free) or as-given (deadlock-prone).
    fn transfer(&self, from: usize, to: usize, amt: i64, sorted: bool) {
        let (r0, r1) = if sorted && from > to {
            (Resource::Row(0, to as u64), Resource::Row(0, from as u64))
        } else {
            (Resource::Row(0, from as u64), Resource::Row(0, to as u64))
        };
        loop {
            let txn = self.fresh_txn();
            let got = self.acquire(txn, r0) && self.acquire(txn, r1);
            if got {
                let fi = from;
                let ti = to;
                let a = self.accounts[fi].load(Ordering::Relaxed);
                let b = self.accounts[ti].load(Ordering::Relaxed);
                self.accounts[fi].store(a - amt, Ordering::Relaxed);
                self.accounts[ti].store(b + amt, Ordering::Relaxed);
                self.lm.lock().unwrap().release_all(txn);
                return;
            }
            self.lm.lock().unwrap().release_all(txn);
            thread::yield_now();
        }
    }

    fn total(&self) -> i64 {
        self.accounts
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .sum()
    }
}

fn run(sorted: bool) {
    let bank = Arc::new(Bank::new());
    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let bank = bank.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ ((tid as u64 + 1) << 32);
            let mut next = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s
            };
            for _ in 0..TRANSFERS {
                let from = (next() as usize) % ACCOUNTS;
                let mut to = (next() as usize) % ACCOUNTS;
                if to == from {
                    to = (to + 1) % ACCOUNTS;
                }
                let amt = 1 + (next() % 10) as i64;
                bank.transfer(from, to, amt, sorted);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        bank.total(),
        ACCOUNTS as i64 * START,
        "money must be conserved: the lock manager failed to enforce mutual exclusion"
    );
}

/// Deadlock-free discipline (sorted acquisition): no aborts, pure mutual-exclusion
/// stress. Conservation proves the manager serializes conflicting X holders.
#[test]
fn sorted_transfers_conserve_money() {
    run(true);
}

/// Deadlock-prone discipline (random acquisition): cycles form, the waits-for
/// detector names victims, and the aborted transactions retry — yet money is still
/// conserved and, crucially, no thread hangs (every join returns).
#[test]
fn random_transfers_trigger_and_survive_deadlocks() {
    run(false);
}
