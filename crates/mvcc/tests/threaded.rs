use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use keel_mvcc::{MvccError, MvccStore};

const ACCOUNTS: usize = 6;
const THREADS: usize = 8;
const TRANSFERS: usize = 300;
const START: i64 = 1000;

struct Bank {
    store: Mutex<MvccStore>,
    rows: Vec<usize>,
    conflicts: AtomicU64,
}

impl Bank {
    fn new() -> Arc<Self> {
        let mut store = MvccStore::new();
        let rows = (0..ACCOUNTS).map(|_| store.bootstrap_row(START)).collect();
        Arc::new(Bank {
            store: Mutex::new(store),
            rows,
            conflicts: AtomicU64::new(0),
        })
    }

    fn transfer(&self, from: usize, to: usize, amt: i64) {
        let lo = from.min(to);
        let hi = from.max(to);
        let (dlo, dhi) = if from == lo { (-amt, amt) } else { (amt, -amt) };
        for attempt in 0..100_000 {
            let mut txn = self.store.lock().unwrap().begin();
            let vlo = self
                .store
                .lock()
                .unwrap()
                .read(&txn, self.rows[lo])
                .unwrap();
            let vhi = self
                .store
                .lock()
                .unwrap()
                .read(&txn, self.rows[hi])
                .unwrap();

            let up1 = self
                .store
                .lock()
                .unwrap()
                .update(&mut txn, self.rows[lo], vlo + dlo);
            if up1 == Err(MvccError::WriteConflict) {
                self.retry(txn, attempt);
                continue;
            }
            up1.unwrap();

            let up2 = self
                .store
                .lock()
                .unwrap()
                .update(&mut txn, self.rows[hi], vhi + dhi);
            if up2 == Err(MvccError::WriteConflict) {
                self.retry(txn, attempt);
                continue;
            }
            up2.unwrap();

            self.store.lock().unwrap().commit(txn);
            return;
        }
        panic!("transfer never committed within the attempt cap (livelock?)");
    }

    fn retry(&self, txn: keel_mvcc::Txn, attempt: usize) {
        self.conflicts.fetch_add(1, Ordering::Relaxed);
        self.store.lock().unwrap().abort(txn);
        for _ in 0..(attempt & 63) {
            thread::yield_now();
        }
    }

    fn total(&self) -> i64 {
        let mut store = self.store.lock().unwrap();
        let txn = store.begin();
        self.rows
            .iter()
            .map(|&r| store.read(&txn, r).unwrap())
            .sum()
    }
}

#[test]
fn first_updater_wins_prevents_lost_updates_under_threads() {
    let bank = Bank::new();
    let mut handles = Vec::new();
    for tid in 0..THREADS {
        let bank = bank.clone();
        handles.push(thread::spawn(move || {
            let mut s = 0xD1B5_4A32_D192_ED03u64 ^ ((tid as u64 + 1) << 40);
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
                bank.transfer(from, to, amt);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        bank.total(),
        ACCOUNTS as i64 * START,
        "money must be conserved: first-updater-wins failed to stop a lost update"
    );
    assert!(
        bank.conflicts.load(Ordering::Relaxed) > 0,
        "expected some write-write conflicts across the run"
    );
}
