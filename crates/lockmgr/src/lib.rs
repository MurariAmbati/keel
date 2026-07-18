//! The lock manager — strict two-phase locking with deadlock detection (§5.1,
//! D6 lane T1).
//!
//! This is the lane that exercises ARIES undo for real: aborts must physically
//! roll back, and 2PL is the schedule discipline that produces them. It is a
//! hierarchical lock table over resources (a table, a row) in modes
//! `{IS, IX, S, SIX, X}` with the standard compatibility matrix, grant/wait
//! queues, and a **waits-for cycle detector** that names a youngest victim when a
//! deadlock forms. Strict 2PL: every lock is held to commit/abort, then released
//! together (`release_all`).
//!
//! It is built and tested single-threaded first (D3): scripted lock schedules
//! prove the compatibility matrix, the wait behavior, upgrades, and — the point —
//! deadlock detection, before any thread touches it. Running it under real
//! latching is the next phase.

use std::collections::{HashMap, HashSet};

/// A transaction id. Younger transactions have larger ids (used for victim
/// selection).
pub type TxnId = u64;

/// A lockable resource: a table (intention locks) or a specific row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Resource {
    Table(u32),
    Row(u32, u64),
}

/// Lock modes, weakest to strongest for intention purposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Intention-shared.
    IS,
    /// Intention-exclusive.
    IX,
    /// Shared.
    S,
    /// Shared + intention-exclusive.
    SIX,
    /// Exclusive.
    X,
}

impl Mode {
    fn idx(self) -> usize {
        match self {
            Mode::IS => 0,
            Mode::IX => 1,
            Mode::S => 2,
            Mode::SIX => 3,
            Mode::X => 4,
        }
    }

    /// Are two already-held modes compatible? (The standard multi-granularity
    /// matrix; rows/cols in IS, IX, S, SIX, X order.)
    pub fn compatible(self, other: Mode) -> bool {
        const M: [[bool; 5]; 5] = [
            [true, true, true, true, false],
            [true, true, false, false, false],
            [true, false, true, false, false],
            [true, false, false, false, false],
            [false, false, false, false, false],
        ];
        M[self.idx()][other.idx()]
    }
}

/// The outcome of a lock request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    /// The lock is now held.
    Granted,
    /// Incompatible with a current holder — the caller would block (the request
    /// is queued).
    Waiting,
    /// Granting the wait would create a cycle; abort `victim` to break it.
    Deadlock { victim: TxnId },
}

#[derive(Default)]
struct Entry {
    granted: Vec<(TxnId, Mode)>,
    waiting: Vec<(TxnId, Mode)>,
}

/// The lock table. A single shard here; production shards by resource hash (§5.1)
/// — the interface is identical.
#[derive(Default)]
pub struct LockManager {
    table: HashMap<Resource, Entry>,
    /// waits_for[t] = the set of transactions `t` is blocked behind.
    waits_for: HashMap<TxnId, HashSet<TxnId>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request `mode` on `res` for `txn` under strict 2PL. Grants immediately if
    /// compatible with all *other* holders; otherwise queues the request and
    /// records waits-for edges, returning `Waiting` — unless doing so closes a
    /// cycle, in which case `Deadlock{victim}` (the youngest transaction in the
    /// cycle) and no edge is added.
    pub fn lock(&mut self, txn: TxnId, res: Resource, mode: Mode) -> Grant {
        let entry = self.table.entry(res).or_default();

        if entry.granted.iter().any(|&(t, _)| t == txn) {
            return Grant::Granted;
        }

        let conflict_holders: Vec<TxnId> = entry
            .granted
            .iter()
            .filter(|&&(t, m)| t != txn && !mode.compatible(m))
            .map(|&(t, _)| t)
            .collect();

        if conflict_holders.is_empty() && entry.waiting.is_empty() {
            entry.granted.push((txn, mode));
            return Grant::Granted;
        }

        let mut wait_on: HashSet<TxnId> = conflict_holders.into_iter().collect();
        for &(t, m) in &entry.waiting {
            if t != txn && !mode.compatible(m) {
                wait_on.insert(t);
            }
        }

        if let Some(victim) = self.would_deadlock(txn, &wait_on) {
            return Grant::Deadlock { victim };
        }

        self.waits_for
            .entry(txn)
            .or_default()
            .extend(wait_on.iter().copied());
        self.table.get_mut(&res).unwrap().waiting.push((txn, mode));
        Grant::Waiting
    }

    /// Release every lock held or waited-for by `txn` (strict 2PL: at
    /// commit/abort), then grant any now-satisfiable waiters.
    pub fn release_all(&mut self, txn: TxnId) {
        self.waits_for.remove(&txn);
        for edges in self.waits_for.values_mut() {
            edges.remove(&txn);
        }
        let resources: Vec<Resource> = self.table.keys().copied().collect();
        for res in resources {
            let entry = self.table.get_mut(&res).unwrap();
            entry.granted.retain(|&(t, _)| t != txn);
            entry.waiting.retain(|&(t, _)| t != txn);
            self.promote_waiters(res);
            if self.table[&res].granted.is_empty() && self.table[&res].waiting.is_empty() {
                self.table.remove(&res);
            }
        }
    }

    /// Grant any waiters on `res` now compatible with the granted set (FIFO, so
    /// the queue can't starve a request behind a long-lived incompatible one).
    fn promote_waiters(&mut self, res: Resource) {
        loop {
            let entry = match self.table.get(&res) {
                Some(e) => e,
                None => return,
            };
            let Some(&(t, m)) = entry.waiting.first() else {
                return;
            };
            let compatible = entry.granted.iter().all(|&(_, gm)| m.compatible(gm));
            if !compatible {
                return;
            }
            let entry = self.table.get_mut(&res).unwrap();
            entry.waiting.remove(0);
            entry.granted.push((t, m));
            self.waits_for.remove(&t);
        }
    }

    /// Would adding `txn -> wait_on` edges create a cycle in the waits-for graph?
    /// If so, return the youngest transaction (largest id) on the cycle as the
    /// victim.
    fn would_deadlock(&self, txn: TxnId, wait_on: &HashSet<TxnId>) -> Option<TxnId> {
        for &start in wait_on {
            let mut stack = vec![start];
            let mut seen = HashSet::new();
            let mut path = vec![txn];
            while let Some(node) = stack.pop() {
                if node == txn {
                    path.push(node);
                    return Some(*path.iter().chain(wait_on.iter()).max().unwrap());
                }
                if !seen.insert(node) {
                    continue;
                }
                path.push(node);
                if let Some(edges) = self.waits_for.get(&node) {
                    for &n in edges {
                        stack.push(n);
                    }
                }
            }
        }
        None
    }

    /// Debug: current holders of a resource.
    pub fn holders(&self, res: Resource) -> Vec<(TxnId, Mode)> {
        self.table
            .get(&res)
            .map(|e| e.granted.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Resource = Resource::Row(1, 10);
    const B: Resource = Resource::Row(1, 20);

    #[test]
    fn compatibility_matrix() {
        assert!(Mode::S.compatible(Mode::S));
        assert!(Mode::IS.compatible(Mode::IX));
        assert!(!Mode::S.compatible(Mode::X));
        assert!(!Mode::X.compatible(Mode::X));
        assert!(!Mode::S.compatible(Mode::IX));
        assert!(Mode::IS.compatible(Mode::SIX));
        assert!(!Mode::SIX.compatible(Mode::IX));
    }

    #[test]
    fn shared_locks_coexist_exclusive_waits() {
        let mut lm = LockManager::new();
        assert_eq!(lm.lock(1, A, Mode::S), Grant::Granted);
        assert_eq!(lm.lock(2, A, Mode::S), Grant::Granted);
        assert_eq!(lm.lock(3, A, Mode::X), Grant::Waiting);
        lm.release_all(1);
        lm.release_all(2);
        assert_eq!(lm.holders(A), vec![(3, Mode::X)]);
    }

    #[test]
    fn exclusive_blocks_then_grants() {
        let mut lm = LockManager::new();
        assert_eq!(lm.lock(1, A, Mode::X), Grant::Granted);
        assert_eq!(lm.lock(2, A, Mode::S), Grant::Waiting);
        lm.release_all(1);
        assert_eq!(lm.holders(A), vec![(2, Mode::S)]);
    }

    #[test]
    fn deadlock_detected_with_youngest_victim() {
        let mut lm = LockManager::new();
        assert_eq!(lm.lock(1, A, Mode::X), Grant::Granted);
        assert_eq!(lm.lock(2, B, Mode::X), Grant::Granted);
        assert_eq!(lm.lock(1, B, Mode::X), Grant::Waiting);
        assert_eq!(lm.lock(2, A, Mode::X), Grant::Deadlock { victim: 2 });
    }

    #[test]
    fn no_false_deadlock() {
        let mut lm = LockManager::new();
        assert_eq!(lm.lock(1, A, Mode::X), Grant::Granted);
        assert_eq!(lm.lock(2, A, Mode::X), Grant::Waiting);
        assert_eq!(lm.lock(3, A, Mode::X), Grant::Waiting);
        lm.release_all(1);
        assert_eq!(lm.holders(A), vec![(2, Mode::X)]);
    }

    #[test]
    fn three_way_deadlock() {
        let mut lm = LockManager::new();
        let c = Resource::Row(1, 30);
        lm.lock(1, A, Mode::X);
        lm.lock(2, B, Mode::X);
        lm.lock(3, c, Mode::X);
        assert_eq!(lm.lock(1, B, Mode::X), Grant::Waiting);
        assert_eq!(lm.lock(2, c, Mode::X), Grant::Waiting);
        assert_eq!(lm.lock(3, A, Mode::X), Grant::Deadlock { victim: 3 });
    }
}
