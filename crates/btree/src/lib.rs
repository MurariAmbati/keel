use keel_buffer::{BufferError, BufferPool, PageId};
use keel_heap::Rid;
use keel_page::{raw, PageType};
use keel_pager::Pager;

pub const NIL: PageId = u32::MAX;

const RID_LEN: usize = 6;

#[derive(Debug)]
pub enum BtreeError {
    Buffer(BufferError),
    BadNode(PageId),
    KeyTooLarge,
}

impl std::fmt::Display for BtreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtreeError::Buffer(e) => write!(f, "{e}"),
            BtreeError::BadNode(p) => write!(f, "page {p} is not a B-tree node"),
            BtreeError::KeyTooLarge => write!(f, "entry too large for a node"),
        }
    }
}
impl std::error::Error for BtreeError {}
impl From<BufferError> for BtreeError {
    fn from(e: BufferError) -> Self {
        BtreeError::Buffer(e)
    }
}
impl From<keel_pager::PagerError> for BtreeError {
    fn from(e: keel_pager::PagerError) -> Self {
        BtreeError::Buffer(match e {
            keel_pager::PagerError::Io(e) => BufferError::Io(e),
            keel_pager::PagerError::Corrupt(p) => BufferError::Corrupt(p),
            keel_pager::PagerError::Exhausted => BufferError::Exhausted,
        })
    }
}

pub type Result<T> = std::result::Result<T, BtreeError>;

#[derive(Clone, Debug)]
struct Leaf {
    entries: Vec<(Vec<u8>, Rid)>,
    prev: PageId,
    next: PageId,
}

#[derive(Clone, Debug)]
struct Internal {
    keys: Vec<Vec<u8>>,
    children: Vec<PageId>,
}

enum Node {
    Leaf(Leaf),
    Internal(Internal),
}

fn leaf_ser_len(entries: &[(Vec<u8>, Rid)]) -> usize {
    2 + entries
        .iter()
        .map(|(k, _)| 2 + k.len() + RID_LEN)
        .sum::<usize>()
}
fn internal_ser_len(keys: &[Vec<u8>]) -> usize {
    2 + 4 + keys.iter().map(|k| 2 + k.len() + 4).sum::<usize>()
}

fn rd_u16(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}
fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn put_rid(b: &mut [u8], rid: Rid) {
    b[0..4].copy_from_slice(&rid.page.to_le_bytes());
    b[4..6].copy_from_slice(&rid.slot.to_le_bytes());
}
fn get_rid(b: &[u8]) -> Rid {
    Rid::new(rd_u32(b, 0), u16::from_le_bytes([b[4], b[5]]))
}

fn parse_leaf(body: &[u8], extra: u64) -> Leaf {
    let count = rd_u16(body, 0);
    let mut pos = 2;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let klen = rd_u16(body, pos);
        pos += 2;
        let key = body[pos..pos + klen].to_vec();
        pos += klen;
        let rid = get_rid(&body[pos..pos + RID_LEN]);
        pos += RID_LEN;
        entries.push((key, rid));
    }
    Leaf {
        entries,
        prev: (extra >> 32) as u32,
        next: (extra & 0xFFFF_FFFF) as u32,
    }
}

fn parse_internal(body: &[u8]) -> Internal {
    let count = rd_u16(body, 0);
    let mut pos = 2;
    let child0 = rd_u32(body, pos);
    pos += 4;
    let mut children = Vec::with_capacity(count + 1);
    let mut keys = Vec::with_capacity(count);
    children.push(child0);
    for _ in 0..count {
        let klen = rd_u16(body, pos);
        pos += 2;
        keys.push(body[pos..pos + klen].to_vec());
        pos += klen;
        children.push(rd_u32(body, pos));
        pos += 4;
    }
    Internal { keys, children }
}

fn write_leaf(body: &mut [u8], leaf: &Leaf) {
    body[0..2].copy_from_slice(&(leaf.entries.len() as u16).to_le_bytes());
    let mut pos = 2;
    for (k, rid) in &leaf.entries {
        body[pos..pos + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
        pos += 2;
        body[pos..pos + k.len()].copy_from_slice(k);
        pos += k.len();
        put_rid(&mut body[pos..pos + RID_LEN], *rid);
        pos += RID_LEN;
    }
}

fn write_internal(body: &mut [u8], node: &Internal) {
    body[0..2].copy_from_slice(&(node.keys.len() as u16).to_le_bytes());
    let mut pos = 2;
    body[pos..pos + 4].copy_from_slice(&node.children[0].to_le_bytes());
    pos += 4;
    for (i, k) in node.keys.iter().enumerate() {
        body[pos..pos + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
        pos += 2;
        body[pos..pos + k.len()].copy_from_slice(k);
        pos += k.len();
        body[pos..pos + 4].copy_from_slice(&node.children[i + 1].to_le_bytes());
        pos += 4;
    }
}

impl Internal {
    fn child_index(&self, key: &[u8]) -> usize {
        let mut i = 0;
        while i < self.keys.len() && key >= self.keys[i].as_slice() {
            i += 1;
        }
        i
    }
}

fn split_at_bytes(sizes: impl Iterator<Item = usize>, n: usize) -> usize {
    let sizes: Vec<usize> = sizes.collect();
    let total: usize = sizes.iter().sum();
    let mut acc = 0;
    for (i, s) in sizes.iter().enumerate() {
        acc += s;
        if acc * 2 >= total {
            return (i + 1).clamp(1, n - 1);
        }
    }
    (n / 2).clamp(1, n - 1)
}

pub struct BTree<'a, P: Pager = BufferPool> {
    bp: &'a P,
    root: std::cell::Cell<PageId>,
    meta: Option<PageId>,
}

impl<'a, P: Pager> BTree<'a, P> {
    pub fn create(bp: &'a P) -> Result<Self> {
        assert_eq!(
            Pager::page_count(bp),
            0,
            "BTree::create expects an empty pool"
        );
        let meta = alloc(bp, PageType::Meta)?;
        let root = alloc(bp, PageType::BTreeLeaf)?;
        let tree = BTree {
            bp,
            root: std::cell::Cell::new(root),
            meta: Some(meta),
        };
        tree.store_leaf(
            root,
            &Leaf {
                entries: Vec::new(),
                prev: NIL,
                next: NIL,
            },
        )?;
        tree.write_meta_root(root)?;
        Ok(tree)
    }

    pub fn open(bp: &'a P) -> Result<Self> {
        let root = bp.with_page(0, |b| raw::extra(b) as u32)?;
        Ok(BTree {
            bp,
            root: std::cell::Cell::new(root),
            meta: Some(0),
        })
    }

    pub fn create_rooted(bp: &'a P) -> Result<Self> {
        let root = alloc(bp, PageType::BTreeLeaf)?;
        let tree = BTree {
            bp,
            root: std::cell::Cell::new(root),
            meta: None,
        };
        tree.store_leaf(
            root,
            &Leaf {
                entries: Vec::new(),
                prev: NIL,
                next: NIL,
            },
        )?;
        Ok(tree)
    }

    pub fn open_rooted(bp: &'a P, root: PageId) -> Self {
        BTree {
            bp,
            root: std::cell::Cell::new(root),
            meta: None,
        }
    }

    pub fn root(&self) -> PageId {
        self.root.get()
    }

    fn write_meta_root(&self, root: PageId) -> Result<()> {
        let Some(meta) = self.meta else {
            return Ok(());
        };
        self.bp.with_page_mut(meta, |b| {
            raw::set_page_type(b, PageType::Meta);
            raw::set_extra(b, root as u64);
        })?;
        Ok(())
    }

    fn load(&self, pid: PageId) -> Result<Node> {
        self.bp.with_page(pid, |bytes| {
            let extra = raw::extra(bytes);
            let body = raw::body(bytes);
            match raw::page_type(bytes) {
                Some(PageType::BTreeLeaf) => Ok(Node::Leaf(parse_leaf(body, extra))),
                Some(PageType::BTreeInternal) => Ok(Node::Internal(parse_internal(body))),
                _ => Err(BtreeError::BadNode(pid)),
            }
        })?
    }

    fn load_leaf(&self, pid: PageId) -> Result<Leaf> {
        match self.load(pid)? {
            Node::Leaf(l) => Ok(l),
            Node::Internal(_) => Err(BtreeError::BadNode(pid)),
        }
    }

    fn store_leaf(&self, pid: PageId, leaf: &Leaf) -> Result<()> {
        self.bp.with_page_mut(pid, |b| {
            raw::set_page_type(b, PageType::BTreeLeaf);
            raw::set_extra(b, ((leaf.prev as u64) << 32) | (leaf.next as u64));
            let body = raw::body_mut(b);
            body.iter_mut().for_each(|x| *x = 0);
            write_leaf(body, leaf);
        })?;
        Ok(())
    }

    fn store_internal(&self, pid: PageId, node: &Internal) -> Result<()> {
        self.bp.with_page_mut(pid, |b| {
            raw::set_page_type(b, PageType::BTreeInternal);
            let body = raw::body_mut(b);
            body.iter_mut().for_each(|x| *x = 0);
            write_internal(body, node);
        })?;
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Rid>> {
        let mut pid = self.root.get();
        loop {
            match self.load(pid)? {
                Node::Leaf(leaf) => {
                    return Ok(leaf
                        .entries
                        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                        .ok()
                        .map(|i| leaf.entries[i].1));
                }
                Node::Internal(node) => pid = node.children[node.child_index(key)],
            }
        }
    }

    pub fn insert(&self, key: &[u8], rid: Rid) -> Result<()> {
        if let Some((sep, right_pid)) = self.insert_rec(self.root.get(), key, rid)? {
            let old_root = self.root.get();
            let new_root = alloc(self.bp, PageType::BTreeInternal)?;
            self.store_internal(
                new_root,
                &Internal {
                    keys: vec![sep],
                    children: vec![old_root, right_pid],
                },
            )?;
            self.root.set(new_root);
            self.write_meta_root(new_root)?;
        }
        Ok(())
    }

    fn insert_rec(&self, pid: PageId, key: &[u8], rid: Rid) -> Result<Option<(Vec<u8>, PageId)>> {
        match self.load(pid)? {
            Node::Leaf(mut leaf) => {
                match leaf
                    .entries
                    .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                {
                    Ok(i) => {
                        leaf.entries[i].1 = rid;
                        self.store_leaf(pid, &leaf)?;
                        return Ok(None);
                    }
                    Err(i) => leaf.entries.insert(i, (key.to_vec(), rid)),
                }
                if leaf_ser_len(&leaf.entries) <= raw::BODY_CAPACITY {
                    self.store_leaf(pid, &leaf)?;
                    return Ok(None);
                }
                if leaf.entries.len() < 2 {
                    return Err(BtreeError::KeyTooLarge);
                }
                let s = split_at_bytes(
                    leaf.entries.iter().map(|(k, _)| 2 + k.len() + RID_LEN),
                    leaf.entries.len(),
                );
                let right_entries = leaf.entries.split_off(s);
                let sep = right_entries[0].0.clone();
                let right_pid = alloc(self.bp, PageType::BTreeLeaf)?;
                let old_next = leaf.next;
                let right = Leaf {
                    entries: right_entries,
                    prev: pid,
                    next: old_next,
                };
                leaf.next = right_pid;
                self.store_leaf(pid, &leaf)?;
                self.store_leaf(right_pid, &right)?;
                if old_next != NIL {
                    let mut nn = self.load_leaf(old_next)?;
                    nn.prev = right_pid;
                    self.store_leaf(old_next, &nn)?;
                }
                Ok(Some((sep, right_pid)))
            }
            Node::Internal(mut node) => {
                let i = node.child_index(key);
                let child = node.children[i];
                match self.insert_rec(child, key, rid)? {
                    None => Ok(None),
                    Some((sep, right_pid)) => {
                        node.keys.insert(i, sep);
                        node.children.insert(i + 1, right_pid);
                        if internal_ser_len(&node.keys) <= raw::BODY_CAPACITY {
                            self.store_internal(pid, &node)?;
                            return Ok(None);
                        }
                        let s = split_at_bytes(
                            node.keys.iter().map(|k| 2 + k.len() + 4),
                            node.keys.len(),
                        );
                        let mid = node.keys[s].clone();
                        let right_keys = node.keys.split_off(s + 1);
                        let up = node.keys.pop().unwrap();
                        debug_assert_eq!(up, mid);
                        let right_children = node.children.split_off(s + 1);
                        let right_pid = alloc(self.bp, PageType::BTreeInternal)?;
                        let right = Internal {
                            keys: right_keys,
                            children: right_children,
                        };
                        self.store_internal(pid, &node)?;
                        self.store_internal(right_pid, &right)?;
                        Ok(Some((mid, right_pid)))
                    }
                }
            }
        }
    }

    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        let mut pid = self.root.get();
        loop {
            match self.load(pid)? {
                Node::Internal(node) => pid = node.children[node.child_index(key)],
                Node::Leaf(mut leaf) => {
                    match leaf
                        .entries
                        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                    {
                        Ok(i) => {
                            leaf.entries.remove(i);
                            self.store_leaf(pid, &leaf)?;
                            return Ok(true);
                        }
                        Err(_) => return Ok(false),
                    }
                }
            }
        }
    }

    fn find_leaf(&self, key: &[u8]) -> Result<PageId> {
        let mut pid = self.root.get();
        loop {
            match self.load(pid)? {
                Node::Leaf(_) => return Ok(pid),
                Node::Internal(node) => pid = node.children[node.child_index(key)],
            }
        }
    }

    fn leftmost_leaf(&self) -> Result<PageId> {
        let mut pid = self.root.get();
        loop {
            match self.load(pid)? {
                Node::Leaf(_) => return Ok(pid),
                Node::Internal(node) => pid = node.children[0],
            }
        }
    }

    pub fn range(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<Vec<(Vec<u8>, Rid)>> {
        let mut out = Vec::new();
        let mut cur = self.find_leaf(lo)?;
        while cur != NIL {
            let leaf = self.load_leaf(cur)?;
            for (k, rid) in &leaf.entries {
                if k.as_slice() < lo {
                    continue;
                }
                if let Some(h) = hi {
                    if k.as_slice() >= h {
                        return Ok(out);
                    }
                }
                out.push((k.clone(), *rid));
            }
            cur = leaf.next;
        }
        Ok(out)
    }

    pub fn scan_all(&self) -> Result<Vec<(Vec<u8>, Rid)>> {
        let mut out = Vec::new();
        let mut cur = self.leftmost_leaf()?;
        while cur != NIL {
            let leaf = self.load_leaf(cur)?;
            out.extend(leaf.entries.iter().map(|(k, r)| (k.clone(), *r)));
            cur = leaf.next;
        }
        Ok(out)
    }

    pub fn check(&self) -> Result<CheckReport> {
        let mut report = CheckReport::default();
        let mut leaf_depths = Vec::new();
        let mut dfs_leaves = Vec::new();
        self.check_node(
            self.root.get(),
            None,
            None,
            0,
            &mut report,
            &mut leaf_depths,
            &mut dfs_leaves,
        )?;

        if let Some(&d0) = leaf_depths.first() {
            if leaf_depths.iter().any(|&d| d != d0) {
                report
                    .violations
                    .push(format!("unbalanced: leaf depths vary {leaf_depths:?}"));
            }
            report.height = d0;
        }

        let mut chain = Vec::new();
        let mut prev_seen: Option<PageId> = None;
        let mut last_key: Option<Vec<u8>> = None;
        let mut cur = self.leftmost_leaf()?;
        while cur != NIL {
            let leaf = self.load_leaf(cur)?;
            if leaf.prev != prev_seen.unwrap_or(NIL) {
                report.violations.push(format!(
                    "leaf {cur} prev={} but predecessor was {:?}",
                    leaf.prev, prev_seen
                ));
            }
            for (k, _) in &leaf.entries {
                if let Some(lk) = &last_key {
                    if k <= lk {
                        report
                            .violations
                            .push(format!("keys not ascending across chain at leaf {cur}"));
                    }
                }
                last_key = Some(k.clone());
            }
            chain.push(cur);
            prev_seen = Some(cur);
            cur = leaf.next;
        }
        if chain != dfs_leaves {
            report.violations.push(format!(
                "sibling chain {chain:?} != DFS leaf order {dfs_leaves:?}"
            ));
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn check_node(
        &self,
        pid: PageId,
        low: Option<&[u8]>,
        high: Option<&[u8]>,
        depth: u32,
        report: &mut CheckReport,
        leaf_depths: &mut Vec<u32>,
        dfs_leaves: &mut Vec<PageId>,
    ) -> Result<()> {
        report.nodes += 1;
        match self.load(pid)? {
            Node::Leaf(leaf) => {
                report.leaves += 1;
                report.entries += leaf.entries.len() as u64;
                leaf_depths.push(depth);
                dfs_leaves.push(pid);
                for w in leaf.entries.windows(2) {
                    if w[0].0 >= w[1].0 {
                        report
                            .violations
                            .push(format!("leaf {pid}: keys not strictly ascending"));
                    }
                }
                for (k, _) in &leaf.entries {
                    if !in_bounds(k, low, high) {
                        report
                            .violations
                            .push(format!("leaf {pid}: key out of subtree bounds"));
                    }
                }
                if leaf_ser_len(&leaf.entries) > raw::BODY_CAPACITY {
                    report
                        .violations
                        .push(format!("leaf {pid}: overflows page"));
                }
            }
            Node::Internal(node) => {
                if node.children.len() != node.keys.len() + 1 {
                    report
                        .violations
                        .push(format!("internal {pid}: children != keys+1"));
                }
                for w in node.keys.windows(2) {
                    if w[0] >= w[1] {
                        report
                            .violations
                            .push(format!("internal {pid}: separators not ascending"));
                    }
                }
                for k in &node.keys {
                    if !in_bounds(k, low, high) {
                        report
                            .violations
                            .push(format!("internal {pid}: separator out of bounds"));
                    }
                }
                for (i, &child) in node.children.iter().enumerate() {
                    let clow = if i == 0 {
                        low
                    } else {
                        Some(node.keys[i - 1].as_slice())
                    };
                    let chigh = if i == node.keys.len() {
                        high
                    } else {
                        Some(node.keys[i].as_slice())
                    };
                    self.check_node(
                        child,
                        clow,
                        chigh,
                        depth + 1,
                        report,
                        leaf_depths,
                        dfs_leaves,
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn in_bounds(k: &[u8], low: Option<&[u8]>, high: Option<&[u8]>) -> bool {
    low.map(|l| k >= l).unwrap_or(true) && high.map(|h| k < h).unwrap_or(true)
}

fn alloc<P: Pager>(bp: &P, pt: PageType) -> Result<PageId> {
    Ok(bp.alloc_raw(pt)?)
}

#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    pub nodes: u64,
    pub leaves: u64,
    pub entries: u64,
    pub height: u32,
    pub violations: Vec<String>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_buffer::BufferPool;
    use keel_keys::encode_value;
    use keel_rng::Rng;
    use keel_types::{ColumnType, Value};
    use keel_vfs::{BlockFile, MemDisk};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn fresh(frames: usize) -> BufferPool {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        BufferPool::open_default(disk, frames).unwrap()
    }

    fn rid(i: u64) -> Rid {
        Rid::new((i >> 16) as u32, (i & 0xFFFF) as u16)
    }

    #[test]
    fn insert_get_basic() {
        let bp = fresh(32);
        let t = BTree::create(&bp).unwrap();
        for i in 0..1000u64 {
            let k = encode_value(ColumnType::BigInt, &Value::BigInt(i as i64));
            t.insert(&k, rid(i)).unwrap();
        }
        for i in 0..1000u64 {
            let k = encode_value(ColumnType::BigInt, &Value::BigInt(i as i64));
            assert_eq!(t.get(&k).unwrap(), Some(rid(i)));
        }
        let miss = encode_value(ColumnType::BigInt, &Value::BigInt(999999));
        assert_eq!(t.get(&miss).unwrap(), None);
        assert!(t.check().unwrap().ok());
        assert!(
            t.check().unwrap().height >= 1,
            "1000 keys should force splits"
        );
    }

    #[test]
    fn replace_existing_key() {
        let bp = fresh(16);
        let t = BTree::create(&bp).unwrap();
        let k = encode_value(ColumnType::Int, &Value::Int(42));
        t.insert(&k, rid(1)).unwrap();
        t.insert(&k, rid(2)).unwrap();
        assert_eq!(t.get(&k).unwrap(), Some(rid(2)));
        assert_eq!(t.scan_all().unwrap().len(), 1);
    }

    #[test]
    fn range_scan_is_ordered_and_bounded() {
        let bp = fresh(32);
        let t = BTree::create(&bp).unwrap();
        for i in 0..500u64 {
            let k = encode_value(ColumnType::BigInt, &Value::BigInt(i as i64));
            t.insert(&k, rid(i)).unwrap();
        }
        let lo = encode_value(ColumnType::BigInt, &Value::BigInt(100));
        let hi = encode_value(ColumnType::BigInt, &Value::BigInt(200));
        let got = t.range(&lo, Some(&hi)).unwrap();
        assert_eq!(got.len(), 100);
        for (idx, (_, r)) in got.iter().enumerate() {
            assert_eq!(*r, rid(100 + idx as u64));
        }
    }

    #[test]
    fn persists_across_reopen() {
        let disk = Arc::new(MemDisk::new());
        {
            let bp = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, 32).unwrap();
            let t = BTree::create(&bp).unwrap();
            for i in 0..2000u64 {
                let k = encode_value(ColumnType::BigInt, &Value::BigInt(i as i64));
                t.insert(&k, rid(i)).unwrap();
            }
            bp.checkpoint().unwrap();
        }
        let bp = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, 32).unwrap();
        let t = BTree::open(&bp).unwrap();
        assert!(t.check().unwrap().ok());
        for i in 0..2000u64 {
            let k = encode_value(ColumnType::BigInt, &Value::BigInt(i as i64));
            assert_eq!(
                t.get(&k).unwrap(),
                Some(rid(i)),
                "key {i} lost after reopen"
            );
        }
    }

    #[test]
    fn fuzz_vs_btreemap() {
        for seed in 0..12u64 {
            let bp = fresh(8);
            let t = BTree::create(&bp).unwrap();
            let mut model: BTreeMap<Vec<u8>, Rid> = BTreeMap::new();
            let mut rng = Rng::seed(seed);

            for step in 0..6000u64 {
                let key = adversarial_key(&mut rng);
                match rng.below(3) {
                    0 => {
                        let r = rid(step);
                        t.insert(&key, r).unwrap();
                        model.insert(key, r);
                    }
                    1 => {
                        let existed_model = model.remove(&key).is_some();
                        let existed_tree = t.delete(&key).unwrap();
                        assert_eq!(existed_model, existed_tree, "seed {seed}: delete mismatch");
                    }
                    _ => {
                        assert_eq!(
                            t.get(&key).unwrap(),
                            model.get(&key).copied(),
                            "seed {seed}: get mismatch"
                        );
                    }
                }
                if step % 500 == 0 {
                    let report = t.check().unwrap();
                    assert!(
                        report.ok(),
                        "seed {seed} step {step}: {:?}",
                        report.violations
                    );
                    let scanned: Vec<_> = t.scan_all().unwrap();
                    let expected: Vec<_> = model.iter().map(|(k, r)| (k.clone(), *r)).collect();
                    assert_eq!(scanned, expected, "seed {seed} step {step}: scan diverged");
                }
            }

            let scanned: Vec<_> = t.scan_all().unwrap();
            let expected: Vec<_> = model.iter().map(|(k, r)| (k.clone(), *r)).collect();
            assert_eq!(scanned, expected, "seed {seed}: final scan diverged");
            assert!(t.check().unwrap().ok(), "seed {seed}: final check failed");
        }
    }

    fn adversarial_key(rng: &mut Rng) -> Vec<u8> {
        match rng.below(5) {
            0 => Vec::new(),
            1 => vec![0u8; rng.below(4) as usize],
            2 => {
                let mut k = b"prefix/".to_vec();
                k.extend((0..rng.below(6)).map(|_| b'a' + rng.below(3) as u8));
                k
            }
            3 => encode_value(ColumnType::Int, &Value::Int(rng.below(64) as i32 - 32)),
            _ => (0..rng.below(40))
                .map(|_| b'a' + rng.below(4) as u8)
                .collect(),
        }
    }
}
