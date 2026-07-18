use std::collections::{BTreeSet, HashMap};

pub type TxnId = u64;
pub const INVALID: TxnId = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    InProgress,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, Default)]
pub struct Clog {
    status: HashMap<TxnId, Status>,
}

impl Clog {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&mut self, t: TxnId, s: Status) {
        self.status.insert(t, s);
    }
    pub fn get(&self, t: TxnId) -> Status {
        self.status.get(&t).copied().unwrap_or(Status::InProgress)
    }
    fn is_committed(&self, t: TxnId) -> bool {
        self.get(t) == Status::Committed
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub xmax: TxnId,
    pub in_flight: BTreeSet<TxnId>,
}

impl Snapshot {
    fn precedes(&self, t: TxnId) -> bool {
        t < self.xmax && !self.in_flight.contains(&t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub xmin: TxnId,
    pub xmax: TxnId,
}

impl Version {
    pub fn new(xmin: TxnId) -> Self {
        Version {
            xmin,
            xmax: INVALID,
        }
    }
}

pub fn visible(v: &Version, snap: &Snapshot, clog: &Clog, reader: TxnId) -> bool {
    let insert_visible = if v.xmin == reader {
        true
    } else {
        clog.is_committed(v.xmin) && snap.precedes(v.xmin)
    };
    if !insert_visible {
        return false;
    }
    if v.xmax == INVALID {
        return true;
    }
    let delete_visible = if v.xmax == reader {
        true
    } else {
        clog.is_committed(v.xmax) && snap.precedes(v.xmax)
    };
    !delete_visible
}

#[derive(Clone, Debug)]
struct Chain {
    versions: Vec<(Version, i64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MvccError {
    WriteConflict,
    NotVisible,
    Serialization,
}

pub struct Txn {
    pub id: TxnId,
    pub snapshot: Snapshot,
    written: BTreeSet<usize>,
}

#[derive(Default)]
pub struct MvccStore {
    rows: Vec<Chain>,
    clog: Clog,
    next_txn: TxnId,
    active: BTreeSet<TxnId>,
}

impl MvccStore {
    pub fn new() -> Self {
        MvccStore {
            rows: Vec::new(),
            clog: Clog::new(),
            next_txn: 1,
            active: BTreeSet::new(),
        }
    }

    pub fn bootstrap_row(&mut self, value: i64) -> usize {
        let t = self.next_txn;
        self.next_txn += 1;
        self.clog.set(t, Status::Committed);
        self.rows.push(Chain {
            versions: vec![(Version::new(t), value)],
        });
        self.rows.len() - 1
    }

    pub fn begin(&mut self) -> Txn {
        let id = self.next_txn;
        self.next_txn += 1;
        self.clog.set(id, Status::InProgress);
        let snapshot = Snapshot {
            xmax: id,
            in_flight: self.active.clone(),
        };
        self.active.insert(id);
        Txn {
            id,
            snapshot,
            written: BTreeSet::new(),
        }
    }

    pub fn read(&self, txn: &Txn, row: usize) -> Result<i64, MvccError> {
        let chain = &self.rows[row];
        for (v, val) in chain.versions.iter().rev() {
            if visible(v, &txn.snapshot, &self.clog, txn.id) {
                return Ok(*val);
            }
        }
        Err(MvccError::NotVisible)
    }

    pub fn update(&mut self, txn: &mut Txn, row: usize, value: i64) -> Result<(), MvccError> {
        let idx = {
            let chain = &self.rows[row];
            chain
                .versions
                .iter()
                .rposition(|(v, _)| self.clog.get(v.xmin) != Status::Aborted)
                .expect("a row always retains its committed bootstrap version")
        };
        let creator = self.rows[row].versions[idx].0.xmin;
        if creator != txn.id && !(self.clog.is_committed(creator) && txn.snapshot.precedes(creator))
        {
            return Err(MvccError::WriteConflict);
        }
        let chain = &mut self.rows[row];
        chain.versions[idx].0.xmax = txn.id;
        chain.versions.push((
            Version {
                xmin: txn.id,
                xmax: INVALID,
            },
            value,
        ));
        txn.written.insert(row);
        Ok(())
    }

    pub fn commit(&mut self, txn: Txn) {
        self.clog.set(txn.id, Status::Committed);
        self.active.remove(&txn.id);
    }

    pub fn abort(&mut self, txn: Txn) {
        self.clog.set(txn.id, Status::Aborted);
        self.active.remove(&txn.id);
    }

    pub fn version_count(&self, row: usize) -> usize {
        self.rows[row].versions.len()
    }

    pub fn vacuum(&mut self) -> usize {
        let no_active = self.active.is_empty();
        let mut reclaimed = 0;
        for r in 0..self.rows.len() {
            let old_len = self.rows[r].versions.len();
            let newest_committed = self.rows[r]
                .versions
                .iter()
                .rposition(|(v, _)| self.clog.is_committed(v.xmin));
            let mut kept = Vec::with_capacity(old_len);
            for i in 0..old_len {
                let (v, val) = self.rows[r].versions[i];
                let status = self.clog.get(v.xmin);
                if status == Status::Aborted {
                    continue;
                }
                if no_active
                    && status == Status::Committed
                    && newest_committed.is_some_and(|nc| i < nc)
                {
                    continue;
                }
                kept.push((v, val));
            }
            if kept.is_empty() {
                let keep = newest_committed
                    .map(|nc| self.rows[r].versions[nc])
                    .unwrap_or_else(|| *self.rows[r].versions.last().unwrap());
                kept.push(keep);
            }
            reclaimed += old_len - kept.len();
            self.rows[r].versions = kept;
        }
        reclaimed
    }
}

impl Default for Txn {
    fn default() -> Self {
        Txn {
            id: INVALID,
            snapshot: Snapshot {
                xmax: 1,
                in_flight: BTreeSet::new(),
            },
            written: BTreeSet::new(),
        }
    }
}

#[derive(Debug)]
struct SsiTxnMeta {
    snapshot: Snapshot,
    reads: BTreeSet<usize>,
    writes: BTreeSet<usize>,
    concurrent: BTreeSet<TxnId>,
    out_edges: BTreeSet<TxnId>,
    in_conflict: bool,
}

#[derive(Default)]
pub struct SsiStore {
    rows: Vec<Chain>,
    clog: Clog,
    next_txn: TxnId,
    active: BTreeSet<TxnId>,
    meta: HashMap<TxnId, SsiTxnMeta>,
}

impl SsiStore {
    pub fn new() -> Self {
        SsiStore {
            rows: Vec::new(),
            clog: Clog::new(),
            next_txn: 1,
            active: BTreeSet::new(),
            meta: HashMap::new(),
        }
    }

    pub fn bootstrap_row(&mut self, value: i64) -> usize {
        let t = self.next_txn;
        self.next_txn += 1;
        self.clog.set(t, Status::Committed);
        self.rows.push(Chain {
            versions: vec![(Version::new(t), value)],
        });
        self.rows.len() - 1
    }

    pub fn begin(&mut self) -> TxnId {
        let id = self.next_txn;
        self.next_txn += 1;
        self.clog.set(id, Status::InProgress);
        let snapshot = Snapshot {
            xmax: id,
            in_flight: self.active.clone(),
        };
        let concurrent = self.active.clone();
        for &a in &concurrent {
            if let Some(m) = self.meta.get_mut(&a) {
                m.concurrent.insert(id);
            }
        }
        self.active.insert(id);
        self.meta.insert(
            id,
            SsiTxnMeta {
                snapshot,
                reads: BTreeSet::new(),
                writes: BTreeSet::new(),
                concurrent,
                out_edges: BTreeSet::new(),
                in_conflict: false,
            },
        );
        id
    }

    pub fn read(&mut self, txn: TxnId, row: usize) -> Result<i64, MvccError> {
        let concurrent = self.meta[&txn].concurrent.clone();
        self.meta.get_mut(&txn).unwrap().reads.insert(row);
        for w in concurrent {
            if self.meta.get(&w).is_some_and(|m| m.writes.contains(&row)) {
                self.meta.get_mut(&txn).unwrap().out_edges.insert(w);
                self.meta.get_mut(&w).unwrap().in_conflict = true;
            }
        }
        let snap = self.meta[&txn].snapshot.clone();
        for (v, val) in self.rows[row].versions.iter().rev() {
            if visible(v, &snap, &self.clog, txn) {
                return Ok(*val);
            }
        }
        Err(MvccError::NotVisible)
    }

    pub fn update(&mut self, txn: TxnId, row: usize, value: i64) -> Result<(), MvccError> {
        let (newest, _) = *self.rows[row].versions.last().unwrap();
        let creator = newest.xmin;
        let snap = self.meta[&txn].snapshot.clone();
        if creator != txn && !(self.clog.is_committed(creator) && snap.precedes(creator)) {
            return Err(MvccError::WriteConflict);
        }
        let concurrent = self.meta[&txn].concurrent.clone();
        for r in concurrent {
            if self.meta.get(&r).is_some_and(|m| m.reads.contains(&row)) {
                self.meta.get_mut(&r).unwrap().out_edges.insert(txn);
                self.meta.get_mut(&txn).unwrap().in_conflict = true;
            }
        }
        let chain = &mut self.rows[row];
        chain.versions.last_mut().unwrap().0.xmax = txn;
        chain.versions.push((
            Version {
                xmin: txn,
                xmax: INVALID,
            },
            value,
        ));
        self.meta.get_mut(&txn).unwrap().writes.insert(row);
        Ok(())
    }

    pub fn commit(&mut self, txn: TxnId) -> Result<(), MvccError> {
        let (has_in, out_to_committed) = {
            let m = &self.meta[&txn];
            let out = m.out_edges.iter().any(|t| self.clog.is_committed(*t));
            (m.in_conflict, out)
        };
        if has_in && out_to_committed {
            self.abort(txn);
            return Err(MvccError::Serialization);
        }
        self.clog.set(txn, Status::Committed);
        self.active.remove(&txn);
        Ok(())
    }

    pub fn abort(&mut self, txn: TxnId) {
        self.clog.set(txn, Status::Aborted);
        self.active.remove(&txn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(xmax: TxnId, in_flight: &[TxnId]) -> Snapshot {
        Snapshot {
            xmax,
            in_flight: in_flight.iter().copied().collect(),
        }
    }

    #[test]
    fn visibility_basic_cases() {
        let mut clog = Clog::new();
        clog.set(1, Status::Committed);
        clog.set(2, Status::Aborted);
        clog.set(3, Status::InProgress);
        let s = snap(4, &[3]);

        assert!(visible(&Version::new(1), &s, &clog, INVALID));
        assert!(!visible(&Version::new(2), &s, &clog, INVALID));
        assert!(!visible(&Version::new(3), &s, &clog, 99));
        assert!(visible(&Version::new(3), &s, &clog, 3));
        assert!(!visible(&Version { xmin: 1, xmax: 1 }, &s, &clog, INVALID));
        assert!(visible(&Version { xmin: 1, xmax: 2 }, &s, &clog, INVALID));
        assert!(visible(&Version { xmin: 1, xmax: 3 }, &s, &clog, 99));
        assert!(!visible(&Version::new(5), &s, &clog, INVALID));
    }

    #[test]
    fn visibility_invariants_exhaustive() {
        let txns = [1u64, 2, 3];
        let statuses = [Status::InProgress, Status::Committed, Status::Aborted];
        let mut checked = 0u64;
        for &s1 in &statuses {
            for &s2 in &statuses {
                for &s3 in &statuses {
                    let mut clog = Clog::new();
                    clog.set(1, s1);
                    clog.set(2, s2);
                    clog.set(3, s3);
                    for xmin in txns {
                        for xmax in [INVALID, 1, 2, 3] {
                            for sx in [2u64, 3, 4] {
                                for infl in [vec![], vec![1], vec![2], vec![3], vec![2, 3]] {
                                    let s = snap(sx, &infl);
                                    for reader in [INVALID, 1, 2, 3] {
                                        let v = Version { xmin, xmax };
                                        let vis = visible(&v, &s, &clog, reader);
                                        checked += 1;

                                        if clog.get(xmin) == Status::Aborted && xmin != reader {
                                            assert!(
                                                !vis,
                                                "aborted insert visible: {v:?} r{reader}"
                                            );
                                        }
                                        if vis {
                                            let ins_ok = xmin == reader
                                                || (clog.is_committed(xmin) && s.precedes(xmin));
                                            assert!(ins_ok, "visible with bad insert: {v:?}");
                                        }
                                        if xmax == INVALID
                                            && clog.is_committed(xmin)
                                            && s.precedes(xmin)
                                        {
                                            assert!(vis, "committed live not visible: {v:?}");
                                        }
                                        if xmax == INVALID && xmin == reader && reader != INVALID {
                                            assert!(vis, "own live insert not visible: {v:?}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 5000, "expected a large matrix, got {checked}");
    }

    #[test]
    fn snapshot_read_sees_pre_state() {
        let mut db = MvccStore::new();
        let x = db.bootstrap_row(100);

        let t1 = db.begin();
        let mut t2 = db.begin();
        db.update(&mut t2, x, 200).unwrap();
        db.commit(t2);

        assert_eq!(db.read(&t1, x).unwrap(), 100);

        let t3 = db.begin();
        assert_eq!(db.read(&t3, x).unwrap(), 200);
    }

    #[test]
    fn first_updater_wins() {
        let mut db = MvccStore::new();
        let x = db.bootstrap_row(0);

        let mut t1 = db.begin();
        let mut t2 = db.begin();
        db.update(&mut t1, x, 1).unwrap();
        assert_eq!(db.update(&mut t2, x, 2), Err(MvccError::WriteConflict));
        db.commit(t1);
        db.abort(t2);
    }

    #[test]
    fn aborted_version_does_not_poison_future_updates() {
        let mut db = MvccStore::new();
        let x = db.bootstrap_row(0);

        let mut t1 = db.begin();
        db.update(&mut t1, x, 1).unwrap();
        db.abort(t1);

        let mut t2 = db.begin();
        assert_eq!(
            db.update(&mut t2, x, 2),
            Ok(()),
            "aborted tail version poisoned the update (KEEL-0003)"
        );
        db.commit(t2);
        let t3 = db.begin();
        assert_eq!(db.read(&t3, x).unwrap(), 2);

        for i in 0..5 {
            let mut ta = db.begin();
            db.update(&mut ta, x, 100 + i).unwrap();
            db.abort(ta);
            let mut tb = db.begin();
            db.update(&mut tb, x, 200 + i).unwrap();
            db.commit(tb);
        }
        let tf = db.begin();
        assert_eq!(db.read(&tf, x).unwrap(), 204);
    }

    #[test]
    fn vacuum_reclaims_dead_versions_safely() {
        let mut db = MvccStore::new();
        let x = db.bootstrap_row(0);

        for i in 1..=5 {
            let mut t = db.begin();
            db.update(&mut t, x, i).unwrap();
            db.commit(t);
            let mut a = db.begin();
            db.update(&mut a, x, 999).unwrap();
            db.abort(a);
        }
        assert!(db.version_count(x) > 6, "versions should have accumulated");

        let reclaimed = db.vacuum();
        assert!(reclaimed > 0);
        assert_eq!(
            db.version_count(x),
            1,
            "only the newest committed version remains"
        );
        let t = db.begin();
        assert_eq!(
            db.read(&t, x).unwrap(),
            5,
            "vacuum must not change the visible value"
        );
        db.commit(t);

        let old = db.begin();
        let mut w = db.begin();
        db.update(&mut w, x, 7).unwrap();
        db.commit(w);
        let reclaimed2 = db.vacuum();
        assert_eq!(
            db.read(&old, x).unwrap(),
            5,
            "active reader still sees its snapshot value"
        );
        let fresh = db.begin();
        assert_eq!(
            db.read(&fresh, x).unwrap(),
            7,
            "a fresh reader sees the latest"
        );
        db.commit(fresh);
        db.commit(old);
        let _ = reclaimed2;
    }

    #[test]
    fn write_skew_is_permitted_under_si() {
        let mut db = MvccStore::new();
        let d1 = db.bootstrap_row(1);
        let d2 = db.bootstrap_row(1);

        let mut t1 = db.begin();
        let mut t2 = db.begin();

        let on_call_t1 = db.read(&t1, d1).unwrap() + db.read(&t1, d2).unwrap();
        let on_call_t2 = db.read(&t2, d1).unwrap() + db.read(&t2, d2).unwrap();
        assert_eq!(on_call_t1, 2);
        assert_eq!(on_call_t2, 2);

        db.update(&mut t1, d1, 0).unwrap();
        db.update(&mut t2, d2, 0).unwrap();
        db.commit(t1);
        db.commit(t2);

        let t3 = db.begin();
        let on_call = db.read(&t3, d1).unwrap() + db.read(&t3, d2).unwrap();
        assert_eq!(on_call, 0, "SI permitted write skew: both doctors off call");
    }

    #[test]
    fn ssi_forbids_write_skew() {
        let mut db = SsiStore::new();
        let d1 = db.bootstrap_row(1);
        let d2 = db.bootstrap_row(1);

        let t1 = db.begin();
        let t2 = db.begin();

        assert_eq!(db.read(t1, d1).unwrap() + db.read(t1, d2).unwrap(), 2);
        assert_eq!(db.read(t2, d1).unwrap() + db.read(t2, d2).unwrap(), 2);
        db.update(t1, d1, 0).unwrap();
        db.update(t2, d2, 0).unwrap();

        assert_eq!(db.commit(t1), Ok(()));
        assert_eq!(db.commit(t2), Err(MvccError::Serialization));

        let t3 = db.begin();
        let on_call = db.read(t3, d1).unwrap() + db.read(t3, d2).unwrap();
        assert_eq!(on_call, 1, "SSI must preserve >= 1 on call");
    }

    #[test]
    fn ssi_allows_benign_concurrency() {
        let mut db = SsiStore::new();
        let a = db.bootstrap_row(10);
        let b = db.bootstrap_row(20);

        let t1 = db.begin();
        let t2 = db.begin();
        assert_eq!(db.read(t1, a).unwrap(), 10);
        assert_eq!(db.read(t2, b).unwrap(), 20);
        db.update(t1, a, 11).unwrap();
        db.update(t2, b, 21).unwrap();
        assert_eq!(db.commit(t1), Ok(()));
        assert_eq!(db.commit(t2), Ok(()), "no dangerous structure -> no abort");

        let t3 = db.begin();
        assert_eq!(db.read(t3, a).unwrap(), 11);
        assert_eq!(db.read(t3, b).unwrap(), 21);
    }

    #[test]
    fn ssi_read_only_never_aborts() {
        let mut db = SsiStore::new();
        let a = db.bootstrap_row(5);

        let reader = db.begin();
        let writer = db.begin();
        assert_eq!(db.read(reader, a).unwrap(), 5);
        db.update(writer, a, 6).unwrap();
        assert_eq!(db.commit(writer), Ok(()));
        assert_eq!(db.commit(reader), Ok(()));
    }
}
