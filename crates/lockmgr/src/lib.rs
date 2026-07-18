use std::collections::{HashMap, HashSet};

pub type TxnId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Resource {
    Table(u32),
    Row(u32, u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    IS,
    IX,
    S,
    SIX,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Granted,
    Waiting,
    Deadlock { victim: TxnId },
}

#[derive(Default)]
struct Entry {
    granted: Vec<(TxnId, Mode)>,
    waiting: Vec<(TxnId, Mode)>,
}

#[derive(Default)]
pub struct LockManager {
    table: HashMap<Resource, Entry>,
    waits_for: HashMap<TxnId, HashSet<TxnId>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

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
