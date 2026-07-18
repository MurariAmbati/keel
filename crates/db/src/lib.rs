use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use keel_btree::BTree;
use keel_buffer::{BufferError, BufferPool};
use keel_heap::{HeapError, HeapFile, Rid};
use keel_pager::RecoveryPager;
use keel_sql::refengine::{coerce_into, eval_literal, MemDb};
use keel_sql::{parse_statement, ParseError, Stmt};
use keel_types::{decode_record, encode_record, ColumnDef, ColumnType, Schema, Value};
use keel_vfs::BlockFile;

mod exec;
mod wal;

use wal::StmtLog;

pub use keel_sql::refengine::{ResultSet, Row};

const CATALOG_TID: u16 = 0;
const INDEX_TID: u16 = 1;
const FIRST_USER_TID: u16 = 2;

#[derive(Clone, Debug)]
struct IndexMeta {
    name: String,
    table_id: u16,
    col_index: usize,
    col_type: ColumnType,
    root: u32,
    catalog_rid: Rid,
}

fn rid_bytes(rid: Rid) -> [u8; 6] {
    let mut b = [0u8; 6];
    b[0..4].copy_from_slice(&rid.page.to_le_bytes());
    b[4..6].copy_from_slice(&rid.slot.to_le_bytes());
    b
}
fn index_key(col_type: ColumnType, v: &Value, rid: Rid) -> Vec<u8> {
    let mut k = keel_keys::encode_value(col_type, v);
    k.extend_from_slice(&rid_bytes(rid));
    k
}

#[derive(Debug)]
pub enum DbError {
    Parse(ParseError),
    Exec(String),
    Heap(HeapError),
    Buffer(BufferError),
}
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Parse(e) => write!(f, "{e}"),
            DbError::Exec(e) => write!(f, "{e}"),
            DbError::Heap(e) => write!(f, "{e}"),
            DbError::Buffer(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for DbError {}
impl From<ParseError> for DbError {
    fn from(e: ParseError) -> Self {
        DbError::Parse(e)
    }
}
impl From<HeapError> for DbError {
    fn from(e: HeapError) -> Self {
        DbError::Heap(e)
    }
}
impl From<keel_pager::PagerError> for DbError {
    fn from(e: keel_pager::PagerError) -> Self {
        DbError::Buffer(match e {
            keel_pager::PagerError::Io(e) => BufferError::Io(e),
            keel_pager::PagerError::Corrupt(p) => BufferError::Corrupt(p),
            keel_pager::PagerError::Exhausted => BufferError::Exhausted,
        })
    }
}
impl From<BufferError> for DbError {
    fn from(e: BufferError) -> Self {
        DbError::Buffer(e)
    }
}
impl From<keel_sql::refengine::ExecError> for DbError {
    fn from(e: keel_sql::refengine::ExecError) -> Self {
        DbError::Exec(e.0)
    }
}
fn exec_err<T>(m: impl Into<String>) -> Result<T, DbError> {
    Err(DbError::Exec(m.into()))
}
type R<T> = Result<T, DbError>;

pub struct Database<P: RecoveryPager = BufferPool> {
    bp: P,
    catalog: RefCell<BTreeMap<String, (u16, Schema)>>,
    indexes: RefCell<Vec<IndexMeta>>,
    next_tid: Cell<u16>,
    index_lookups: Cell<u64>,
    join_streams: Cell<u64>,
    agg_streams: Cell<u64>,
    stats: RefCell<HashMap<u16, keel_stats::TableStats>>,
    log: Option<StmtLog>,
    txn: RefCell<Option<Vec<String>>>,
    replayed: Cell<u64>,
}

const REC_STMT: u8 = b'S';
const REC_COMMIT: u8 = b'C';
const REC_SNAP_BEGIN: u8 = b'B';
const REC_SNAP_END: u8 = b'E';
const COMMIT_RECORD: &[u8] = &[REC_COMMIT];
const SNAP_BEGIN_RECORD: &[u8] = &[REC_SNAP_BEGIN];
const SNAP_END_RECORD: &[u8] = &[REC_SNAP_END];

fn stmt_record(sql: &str) -> Vec<u8> {
    let mut r = Vec::with_capacity(1 + sql.len());
    r.push(REC_STMT);
    r.extend_from_slice(sql.as_bytes());
    r
}

fn value_to_sql(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Double(d) => {
            let s = d.to_string();
            if s.contains(['.', 'e', 'E', 'n', 'N']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

fn type_to_sql(t: ColumnType) -> String {
    match t {
        ColumnType::Bool => "BOOL".to_string(),
        ColumnType::Int => "INT".to_string(),
        ColumnType::BigInt => "BIGINT".to_string(),
        ColumnType::Double => "DOUBLE".to_string(),
        ColumnType::Varchar(n) => format!("VARCHAR({n})"),
    }
}

const INDEX_CROSSOVER: f64 = 0.2;

impl Database<BufferPool> {
    pub fn open(file: Arc<dyn BlockFile>, frames: usize) -> R<Self> {
        let bp = BufferPool::open_default(file, frames)?;
        let db = Database {
            bp,
            catalog: RefCell::new(BTreeMap::new()),
            indexes: RefCell::new(Vec::new()),
            next_tid: Cell::new(FIRST_USER_TID),
            index_lookups: Cell::new(0),
            join_streams: Cell::new(0),
            agg_streams: Cell::new(0),
            stats: RefCell::new(HashMap::new()),
            log: None,
            txn: RefCell::new(None),
            replayed: Cell::new(0),
        };
        db.load_catalog()?;
        Ok(db)
    }

    pub fn open_logged(
        data: Arc<dyn BlockFile>,
        log: Arc<dyn BlockFile>,
        frames: usize,
    ) -> R<Self> {
        let bp = BufferPool::open_default(data, frames)?;
        bp.set_no_steal();
        let db = Database {
            bp,
            catalog: RefCell::new(BTreeMap::new()),
            indexes: RefCell::new(Vec::new()),
            next_tid: Cell::new(FIRST_USER_TID),
            index_lookups: Cell::new(0),
            join_streams: Cell::new(0),
            agg_streams: Cell::new(0),
            stats: RefCell::new(HashMap::new()),
            log: Some(StmtLog::open(log)),
            txn: RefCell::new(None),
            replayed: Cell::new(0),
        };
        db.load_catalog()?;
        db.recover()?;
        Ok(db)
    }
}

impl<P: RecoveryPager> Database<P> {
    pub fn with_pager(bp: P) -> R<Self> {
        let db = Database {
            bp,
            catalog: RefCell::new(BTreeMap::new()),
            indexes: RefCell::new(Vec::new()),
            next_tid: Cell::new(FIRST_USER_TID),
            index_lookups: Cell::new(0),
            join_streams: Cell::new(0),
            agg_streams: Cell::new(0),
            stats: RefCell::new(HashMap::new()),
            log: None,
            txn: RefCell::new(None),
            replayed: Cell::new(0),
        };
        db.load_catalog()?;
        Ok(db)
    }

    fn recover(&self) -> R<()> {
        let records = self
            .log
            .as_ref()
            .unwrap()
            .replay()
            .map_err(|e| DbError::Exec(format!("log replay: {e}")))?;

        let last_end = records
            .iter()
            .rposition(|r| r.first() == Some(&REC_SNAP_END));
        let (snap_range, tail_start) = match last_end {
            Some(end) => {
                let begin = records[..end]
                    .iter()
                    .rposition(|r| r.first() == Some(&REC_SNAP_BEGIN))
                    .ok_or_else(|| DbError::Exec("compaction end without begin".into()))?;
                (Some(begin + 1..end), end + 1)
            }
            None => (None, 0),
        };
        let tail_stop = records[tail_start..]
            .iter()
            .position(|r| r.first() == Some(&REC_SNAP_BEGIN))
            .map(|p| tail_start + p)
            .unwrap_or(records.len());

        let sql_of = |rec: &[u8]| -> R<String> {
            String::from_utf8(rec[1..].to_vec())
                .map_err(|_| DbError::Exec("non-utf8 log record".into()))
        };
        let mut replayed = 0u64;

        if let Some(range) = snap_range {
            for rec in &records[range] {
                if rec.first() == Some(&REC_STMT) {
                    self.dispatch(parse_statement(&sql_of(rec)?)?)?;
                    replayed += 1;
                }
            }
        }
        let mut pending: Vec<String> = Vec::new();
        for rec in &records[tail_start..tail_stop] {
            match rec.first().copied() {
                Some(REC_STMT) => pending.push(sql_of(rec)?),
                Some(REC_COMMIT) => {
                    for sql in pending.drain(..) {
                        self.dispatch(parse_statement(&sql)?)?;
                        replayed += 1;
                    }
                }
                Some(REC_SNAP_BEGIN) | Some(REC_SNAP_END) => {}
                _ => return exec_err("unknown log record kind"),
            }
        }
        self.replayed.set(replayed);
        Ok(())
    }

    pub fn replay_count(&self) -> u64 {
        self.replayed.get()
    }

    pub fn compact(&self) -> R<()> {
        let log = self
            .log
            .as_ref()
            .ok_or_else(|| DbError::Exec("compact requires logged mode".into()))?;
        let script = self.snapshot_script()?;
        let append = |rec: &[u8]| {
            log.append(rec)
                .map_err(|e| DbError::Exec(format!("compact: {e}")))
        };
        append(SNAP_BEGIN_RECORD)?;
        for stmt in &script {
            append(&stmt_record(stmt))?;
        }
        append(SNAP_END_RECORD)?;
        Ok(())
    }

    fn snapshot_script(&self) -> R<Vec<String>> {
        let mut tables: Vec<(String, u16, Schema)> = self
            .catalog
            .borrow()
            .iter()
            .map(|(n, (t, s))| (n.clone(), *t, s.clone()))
            .collect();
        tables.sort_by_key(|(_, t, _)| *t);

        let mut out = Vec::new();
        for (name, tid, schema) in &tables {
            let cols: Vec<String> = schema
                .columns
                .iter()
                .map(|c| {
                    let mut s = format!("{} {}", c.name, type_to_sql(c.ty));
                    if c.not_null {
                        s.push_str(" NOT NULL");
                    }
                    s
                })
                .collect();
            out.push(format!("CREATE TABLE {name} ({})", cols.join(", ")));
            for row in self.scan_table(*tid, schema)? {
                let vals: Vec<String> = row.iter().map(value_to_sql).collect();
                out.push(format!("INSERT INTO {name} VALUES ({})", vals.join(", ")));
            }
        }
        for m in self.indexes.borrow().iter() {
            if let Some((tname, _, schema)) = tables.iter().find(|(_, t, _)| *t == m.table_id) {
                let col = &schema.columns[m.col_index].name;
                out.push(format!("CREATE INDEX {} ON {tname} ({col})", m.name));
            }
        }
        Ok(out)
    }

    fn load_catalog(&self) -> R<()> {
        let heap = HeapFile::open(&self.bp)?;
        let mut max_id = 0u16;
        for (rid, rec) in heap.scan()? {
            if rec.len() < 2 {
                continue;
            }
            let tid = u16::from_le_bytes([rec[0], rec[1]]);
            if tid == CATALOG_TID {
                let entry = parse_catalog(&rec[2..])?;
                max_id = max_id.max(entry.0);
                self.catalog
                    .borrow_mut()
                    .insert(entry.1, (entry.0, entry.2));
            } else if tid == INDEX_TID {
                let mut m = parse_index(&rec[2..])?;
                m.catalog_rid = rid;
                self.indexes.borrow_mut().push(m);
            }
        }
        self.next_tid.set((max_id + 1).max(FIRST_USER_TID));
        Ok(())
    }

    pub fn checkpoint(&self) -> R<()> {
        self.bp.checkpoint()?;
        Ok(())
    }

    pub fn table_names(&self) -> Vec<String> {
        self.catalog.borrow().keys().cloned().collect()
    }

    pub fn index_lookups(&self) -> u64 {
        self.index_lookups.get()
    }

    pub fn join_streams(&self) -> u64 {
        self.join_streams.get()
    }

    pub fn agg_streams(&self) -> u64 {
        self.agg_streams.get()
    }

    pub fn analyze(&self) -> R<()> {
        let tables: Vec<(u16, Schema)> = self.catalog.borrow().values().cloned().collect();
        for (tid, schema) in tables {
            let rows = self.scan_table(tid, &schema)?;
            let ts = keel_stats::analyze(&schema, &rows);
            self.stats.borrow_mut().insert(tid, ts);
        }
        Ok(())
    }

    pub fn q_error(&self, sql: &str) -> R<Option<(f64, f64, f64)>> {
        let stmt = parse_statement(sql)?;
        let keel_sql::Stmt::Select(q) = stmt else {
            return Ok(None);
        };
        let Some(from) = &q.from else { return Ok(None) };
        if !from.joins.is_empty() {
            return Ok(None);
        }
        let Some((tid, schema)) = self.catalog.borrow().get(&from.first.table).cloned() else {
            return Ok(None);
        };
        let stats = self.stats.borrow();
        let Some(ts) = stats.get(&tid) else {
            return Ok(None);
        };
        let est_sel = match &q.filter {
            Some(f) => keel_stats::estimate_selectivity(f, &schema, ts),
            None => 1.0,
        };
        let est = est_sel * ts.row_count as f64;
        let actual = self.select(&q)?.rows.len() as f64;
        Ok(Some((est, actual, keel_stats::q_error(est, actual))))
    }

    pub fn execute(&self, sql: &str) -> R<Option<ResultSet>> {
        let stmt = parse_statement(sql)?;

        if self.log.is_some() {
            match &stmt {
                Stmt::Begin => {
                    let mut txn = self.txn.borrow_mut();
                    if txn.is_some() {
                        return exec_err("a transaction is already open");
                    }
                    *txn = Some(Vec::new());
                    return Ok(None);
                }
                Stmt::Commit => {
                    let batch = self.txn.borrow_mut().take();
                    if let Some(stmts) = batch {
                        self.commit_batch(&stmts)?;
                    }
                    return Ok(None);
                }
                Stmt::Rollback => {
                    self.txn.borrow_mut().take();
                    return Ok(None);
                }
                _ => {}
            }
            let in_txn = self.txn.borrow().is_some();
            if in_txn {
                if is_mutating(&stmt) {
                    self.txn
                        .borrow_mut()
                        .as_mut()
                        .unwrap()
                        .push(sql.to_string());
                    return Ok(None);
                }
                if let Stmt::Select(q) = &stmt {
                    let buffered = self.txn.borrow().clone().unwrap_or_default();
                    return Ok(Some(self.select_with_overlay(q, &buffered)?));
                }
                return self.dispatch(stmt);
            }
            if is_mutating(&stmt) {
                self.commit_batch(std::slice::from_ref(&sql.to_string()))?;
                return Ok(None);
            }
        }
        self.dispatch(stmt)
    }

    fn commit_batch(&self, stmts: &[String]) -> R<()> {
        let log = self
            .log
            .as_ref()
            .expect("commit_batch requires logged mode");
        for s in stmts {
            log.append(&stmt_record(s))
                .map_err(|e| DbError::Exec(format!("log append: {e}")))?;
        }
        log.append(COMMIT_RECORD)
            .map_err(|e| DbError::Exec(format!("log append: {e}")))?;
        for s in stmts {
            self.dispatch(parse_statement(s)?)?;
        }
        Ok(())
    }

    fn persist(&self) -> R<()> {
        if self.log.is_some() {
            Ok(())
        } else {
            self.checkpoint()
        }
    }

    fn dispatch(&self, stmt: Stmt) -> R<Option<ResultSet>> {
        match stmt {
            Stmt::CreateTable(ct) => {
                self.create_table(&ct)?;
                Ok(None)
            }
            Stmt::Insert(ins) => {
                self.insert(&ins)?;
                Ok(None)
            }
            Stmt::Delete(d) => {
                self.delete(&d)?;
                Ok(None)
            }
            Stmt::Update(u) => {
                self.update(&u)?;
                Ok(None)
            }
            Stmt::Select(q) => Ok(Some(self.select(&q)?)),
            Stmt::CreateIndex(ci) => {
                self.create_index(&ci)?;
                Ok(None)
            }
            Stmt::DropTable(name) => {
                self.drop_table(&name)?;
                Ok(None)
            }
            Stmt::DropIndex(name) => {
                self.drop_index(&name)?;
                Ok(None)
            }
            Stmt::Begin | Stmt::Commit | Stmt::Rollback => Ok(None),
        }
    }

    fn drop_table(&self, name: &str) -> R<()> {
        let tid = match self.catalog.borrow().get(name) {
            Some((t, _)) => *t,
            None => return exec_err(format!("no such table '{name}'")),
        };
        let heap = HeapFile::open(&self.bp)?;
        let mut to_delete = Vec::new();
        for (rid, rec) in heap.scan()? {
            if rec.len() < 2 {
                continue;
            }
            let rtid = u16::from_le_bytes([rec[0], rec[1]]);
            if rtid == tid {
                to_delete.push(rid);
            } else if rtid == CATALOG_TID {
                if let Ok((ctid, _, _)) = parse_catalog(&rec[2..]) {
                    if ctid == tid {
                        to_delete.push(rid);
                    }
                }
            } else if rtid == INDEX_TID {
                if let Ok(m) = parse_index(&rec[2..]) {
                    if m.table_id == tid {
                        to_delete.push(rid);
                    }
                }
            }
        }
        for rid in to_delete {
            heap.delete(rid)?;
        }
        self.catalog.borrow_mut().remove(name);
        self.indexes.borrow_mut().retain(|m| m.table_id != tid);
        self.stats.borrow_mut().remove(&tid);
        self.persist()
    }

    fn drop_index(&self, name: &str) -> R<()> {
        let catalog_rid = match self.indexes.borrow().iter().find(|m| m.name == name) {
            Some(m) => m.catalog_rid,
            None => return exec_err(format!("no such index '{name}'")),
        };
        HeapFile::open(&self.bp)?.delete(catalog_rid)?;
        self.indexes.borrow_mut().retain(|m| m.name != name);
        self.persist()
    }

    fn delete(&self, del: &keel_sql::Delete) -> R<usize> {
        let (tid, schema) = self
            .catalog
            .borrow()
            .get(&del.table)
            .cloned()
            .ok_or_else(|| DbError::Exec(format!("no such table '{}'", del.table)))?;
        if let Some(f) = &del.filter {
            require_no_subquery(f)?;
        }
        let cols = scan_cols(&del.table, &schema);
        let heap = HeapFile::open(&self.bp)?;
        let mut victims: Vec<(Rid, Row)> = Vec::new();
        for (rid, rec) in heap.scan()? {
            if rec.len() < 2 || u16::from_le_bytes([rec[0], rec[1]]) != tid {
                continue;
            }
            let row =
                decode_record(&schema, &rec[2..]).map_err(|e| DbError::Exec(e.to_string()))?;
            let matched = match &del.filter {
                None => true,
                Some(f) => matches_pred(f, &cols, &row)?,
            };
            if matched {
                victims.push((rid, row));
            }
        }
        let has_indexes = self.indexes.borrow().iter().any(|m| m.table_id == tid);
        for (rid, row) in &victims {
            heap.delete(*rid)?;
            if has_indexes {
                self.remove_index_entries(tid, row, *rid, &heap)?;
            }
        }
        self.persist()?;
        Ok(victims.len())
    }

    fn update(&self, upd: &keel_sql::Update) -> R<usize> {
        let (tid, schema) = self
            .catalog
            .borrow()
            .get(&upd.table)
            .cloned()
            .ok_or_else(|| DbError::Exec(format!("no such table '{}'", upd.table)))?;
        if let Some(f) = &upd.filter {
            require_no_subquery(f)?;
        }
        let targets: Vec<(usize, &keel_sql::Expr)> = upd
            .assignments
            .iter()
            .map(|(col, e)| {
                require_no_subquery(e)?;
                schema
                    .column_index(col)
                    .map(|i| (i, e))
                    .ok_or_else(|| DbError::Exec(format!("no column '{col}'")))
            })
            .collect::<R<Vec<_>>>()?;
        let cols = scan_cols(&upd.table, &schema);
        let heap = HeapFile::open(&self.bp)?;
        let mut work: Vec<(Rid, Row, Row)> = Vec::new();
        for (rid, rec) in heap.scan()? {
            if rec.len() < 2 || u16::from_le_bytes([rec[0], rec[1]]) != tid {
                continue;
            }
            let row =
                decode_record(&schema, &rec[2..]).map_err(|e| DbError::Exec(e.to_string()))?;
            let matched = match &upd.filter {
                None => true,
                Some(f) => matches_pred(f, &cols, &row)?,
            };
            if !matched {
                continue;
            }
            let mut newrow = row.clone();
            for (idx, e) in &targets {
                let v = keel_sql::refengine::eval_public(e, &cols, &row).map_err(DbError::from)?;
                let coerced = coerce_into(v, schema.columns[*idx].ty)?;
                if schema.columns[*idx].not_null && coerced.is_null() {
                    return exec_err(format!(
                        "NULL in NOT NULL column '{}'",
                        schema.columns[*idx].name
                    ));
                }
                newrow[*idx] = coerced;
            }
            work.push((rid, row, newrow));
        }
        let has_indexes = self.indexes.borrow().iter().any(|m| m.table_id == tid);
        for (rid, old, new) in &work {
            let payload = encode_record(&schema, new).map_err(|e| DbError::Exec(e.to_string()))?;
            let mut rec = tid.to_le_bytes().to_vec();
            rec.extend(payload);
            heap.update(*rid, &rec)?;
            if has_indexes {
                self.update_index_entries(tid, old, new, *rid, &heap)?;
            }
        }
        self.persist()?;
        Ok(work.len())
    }

    fn remove_index_entries(&self, tid: u16, row: &Row, rid: Rid, heap: &HeapFile<'_, P>) -> R<()> {
        let mut idxs = self.indexes.borrow_mut();
        for m in idxs.iter_mut().filter(|m| m.table_id == tid) {
            let key = index_key(m.col_type, &row[m.col_index], rid);
            let bt = BTree::open_rooted(&self.bp, m.root);
            bt.delete(&key).map_err(|e| DbError::Exec(e.to_string()))?;
            let new_root = bt.root();
            if new_root != m.root {
                m.root = new_root;
                write_index_catalog(heap, m)?;
            }
        }
        Ok(())
    }

    fn update_index_entries(
        &self,
        tid: u16,
        old: &Row,
        new: &Row,
        rid: Rid,
        heap: &HeapFile<'_, P>,
    ) -> R<()> {
        let mut idxs = self.indexes.borrow_mut();
        for m in idxs.iter_mut().filter(|m| m.table_id == tid) {
            if old[m.col_index] == new[m.col_index] {
                continue;
            }
            let bt = BTree::open_rooted(&self.bp, m.root);
            bt.delete(&index_key(m.col_type, &old[m.col_index], rid))
                .map_err(|e| DbError::Exec(e.to_string()))?;
            bt.insert(&index_key(m.col_type, &new[m.col_index], rid), rid)
                .map_err(|e| DbError::Exec(e.to_string()))?;
            let new_root = bt.root();
            if new_root != m.root {
                m.root = new_root;
                write_index_catalog(heap, m)?;
            }
        }
        Ok(())
    }

    fn create_index(&self, ci: &keel_sql::CreateIndex) -> R<()> {
        let (tid, schema) = self
            .catalog
            .borrow()
            .get(&ci.table)
            .cloned()
            .ok_or_else(|| DbError::Exec(format!("no such table '{}'", ci.table)))?;
        if ci.columns.len() != 1 {
            return exec_err("only single-column indexes are supported");
        }
        let col = &ci.columns[0];
        let col_index = schema
            .column_index(col)
            .ok_or_else(|| DbError::Exec(format!("no column '{col}'")))?;
        let col_type = schema.columns[col_index].ty;

        let bt = BTree::create_rooted(&self.bp).map_err(|e| DbError::Exec(e.to_string()))?;
        for (rid, rec) in HeapFile::open(&self.bp)?.scan()? {
            if rec.len() < 2 || u16::from_le_bytes([rec[0], rec[1]]) != tid {
                continue;
            }
            let row =
                decode_record(&schema, &rec[2..]).map_err(|e| DbError::Exec(e.to_string()))?;
            let key = index_key(col_type, &row[col_index], rid);
            bt.insert(&key, rid)
                .map_err(|e| DbError::Exec(e.to_string()))?;
        }
        let root = bt.root();

        let heap = HeapFile::open(&self.bp)?;
        let mut crec = INDEX_TID.to_le_bytes().to_vec();
        crec.extend(serialize_index(&ci.name, tid, col_index, col_type, root));
        let catalog_rid = heap.insert(&crec)?;
        self.indexes.borrow_mut().push(IndexMeta {
            name: ci.name.clone(),
            table_id: tid,
            col_index,
            col_type,
            root,
            catalog_rid,
        });
        self.persist()
    }

    fn create_table(&self, ct: &keel_sql::CreateTable) -> R<()> {
        if self.catalog.borrow().contains_key(&ct.name) {
            return exec_err(format!("table '{}' already exists", ct.name));
        }
        let tid = self.next_tid.get();
        self.next_tid.set(tid + 1);
        let schema = Schema::new(
            ct.columns
                .iter()
                .map(|c| ColumnDef::new(c.name.clone(), c.ty, c.not_null))
                .collect(),
        );
        let heap = HeapFile::open(&self.bp)?;
        let mut rec = CATALOG_TID.to_le_bytes().to_vec();
        rec.extend(serialize_catalog(tid, &ct.name, &schema));
        heap.insert(&rec)?;
        self.catalog
            .borrow_mut()
            .insert(ct.name.clone(), (tid, schema));
        self.persist()
    }

    fn insert(&self, ins: &keel_sql::Insert) -> R<()> {
        let (tid, schema) = self
            .catalog
            .borrow()
            .get(&ins.table)
            .cloned()
            .ok_or_else(|| DbError::Exec(format!("no such table '{}'", ins.table)))?;
        let order: Vec<usize> = match &ins.columns {
            None => (0..schema.len()).collect(),
            Some(cols) => cols
                .iter()
                .map(|c| {
                    schema
                        .column_index(c)
                        .ok_or_else(|| DbError::Exec(format!("no column '{c}'")))
                })
                .collect::<R<Vec<_>>>()?,
        };
        let has_indexes = self.indexes.borrow().iter().any(|m| m.table_id == tid);
        let heap = HeapFile::open(&self.bp)?;
        for exprs in &ins.rows {
            if exprs.len() != order.len() {
                return exec_err("INSERT value count does not match column count");
            }
            let mut row = vec![Value::Null; schema.len()];
            for (slot, e) in order.iter().zip(exprs) {
                let v = eval_literal(e)?;
                row[*slot] = coerce_into(v, schema.columns[*slot].ty)?;
            }
            let payload = encode_record(&schema, &row).map_err(|e| DbError::Exec(e.to_string()))?;
            let mut rec = tid.to_le_bytes().to_vec();
            rec.extend(payload);
            let data_rid = heap.insert(&rec)?;

            if has_indexes {
                let mut idxs = self.indexes.borrow_mut();
                for m in idxs.iter_mut().filter(|m| m.table_id == tid) {
                    let key = index_key(m.col_type, &row[m.col_index], data_rid);
                    let bt = BTree::open_rooted(&self.bp, m.root);
                    bt.insert(&key, data_rid)
                        .map_err(|e| DbError::Exec(e.to_string()))?;
                    let new_root = bt.root();
                    if new_root != m.root {
                        m.root = new_root;
                        let mut crec = INDEX_TID.to_le_bytes().to_vec();
                        crec.extend(serialize_index(
                            &m.name,
                            m.table_id,
                            m.col_index,
                            m.col_type,
                            new_root,
                        ));
                        heap.update(m.catalog_rid, &crec)?;
                    }
                }
            }
        }
        self.persist()
    }

    pub fn query(&self, q: &keel_sql::Select) -> R<ResultSet> {
        self.select(q)
    }

    fn select(&self, q: &keel_sql::Select) -> R<ResultSet> {
        if let Some(from) = &q.from {
            if from.joins.is_empty() {
                let entry = self.catalog.borrow().get(&from.first.table).cloned();
                if let Some((tid, schema)) = entry {
                    let alias = from
                        .first
                        .alias
                        .clone()
                        .unwrap_or_else(|| from.first.table.clone());
                    let rows = match self.index_rows(tid, &schema, q)? {
                        Some(r) => r,
                        None => self.scan_table(tid, &schema)?,
                    };
                    if let Some(res) = exec::try_stream(q, &alias, &schema, rows) {
                        let rs = res.map_err(|e| DbError::Exec(e.0))?;
                        if exec::is_aggregate(q) {
                            self.agg_streams.set(self.agg_streams.get() + 1);
                        }
                        return Ok(rs);
                    }
                }
            } else if let Some(rs) = self.try_join_stream(q, from)? {
                return Ok(rs);
            }
        }

        let mut mem = self.materialized_memdb()?;
        let rs = mem
            .execute(&Stmt::Select(q.clone()))?
            .ok_or_else(|| DbError::Exec("select produced no result".into()))?;
        Ok(rs)
    }

    fn materialized_memdb(&self) -> R<MemDb> {
        let tid_schema: HashMap<u16, (String, Schema)> = self
            .catalog
            .borrow()
            .iter()
            .map(|(name, (tid, schema))| (*tid, (name.clone(), schema.clone())))
            .collect();
        let mut by_tid: HashMap<u16, Vec<Row>> = HashMap::new();
        let heap = HeapFile::open(&self.bp)?;
        for (_rid, rec) in heap.scan()? {
            if rec.len() < 2 {
                continue;
            }
            let tid = u16::from_le_bytes([rec[0], rec[1]]);
            if tid == CATALOG_TID {
                continue;
            }
            if let Some((_name, schema)) = tid_schema.get(&tid) {
                let row =
                    decode_record(schema, &rec[2..]).map_err(|e| DbError::Exec(e.to_string()))?;
                by_tid.entry(tid).or_default().push(row);
            }
        }
        let mut mem = MemDb::new();
        for (tid, (name, schema)) in &tid_schema {
            let rows = by_tid.remove(tid).unwrap_or_default();
            mem.install_table(name, schema.clone(), rows);
        }
        Ok(mem)
    }

    fn select_with_overlay(&self, q: &keel_sql::Select, buffered: &[String]) -> R<ResultSet> {
        let mut mem = self.materialized_memdb()?;
        for sql in buffered {
            mem.execute(&parse_statement(sql)?)?;
        }
        mem.execute(&Stmt::Select(q.clone()))?
            .ok_or_else(|| DbError::Exec("select produced no result".into()))
    }

    pub fn join_order(&self, sql: &str) -> R<Option<Vec<String>>> {
        let stmt = parse_statement(sql)?;
        let Stmt::Select(q) = stmt else {
            return Ok(None);
        };
        let Some(from) = &q.from else {
            return Ok(None);
        };
        if from.joins.is_empty() {
            return Ok(None);
        }
        let mut tables: Vec<(String, Schema, Vec<Row>)> = Vec::new();
        {
            let cat = self.catalog.borrow();
            for tref in std::iter::once(&from.first).chain(from.joins.iter().map(|(_, t, _)| t)) {
                let Some((tid, schema)) = cat.get(&tref.table).cloned() else {
                    return Ok(None);
                };
                let alias = tref.alias.clone().unwrap_or_else(|| tref.table.clone());
                let rows = self.scan_table(tid, &schema)?;
                tables.push((alias, schema, rows));
            }
        }
        Ok(exec::planned_join_order(from, &tables)
            .map(|idxs| idxs.into_iter().map(|i| tables[i].0.clone()).collect()))
    }

    fn try_join_stream(
        &self,
        q: &keel_sql::Select,
        from: &keel_sql::FromClause,
    ) -> R<Option<ResultSet>> {
        let mut tables: Vec<(String, Schema, Vec<Row>)> = Vec::new();
        {
            let cat = self.catalog.borrow();
            for tref in std::iter::once(&from.first).chain(from.joins.iter().map(|(_, t, _)| t)) {
                let Some((tid, schema)) = cat.get(&tref.table).cloned() else {
                    return Ok(None);
                };
                let alias = tref.alias.clone().unwrap_or_else(|| tref.table.clone());
                let rows = self.scan_table(tid, &schema)?;
                tables.push((alias, schema, rows));
            }
        }
        match exec::try_stream_join(q, &tables) {
            Some(Ok(rs)) => {
                self.join_streams.set(self.join_streams.get() + 1);
                if exec::is_aggregate(q) {
                    self.agg_streams.set(self.agg_streams.get() + 1);
                }
                Ok(Some(rs))
            }
            Some(Err(e)) => Err(DbError::Exec(e.0)),
            None => Ok(None),
        }
    }

    fn index_rows(&self, tid: u16, schema: &Schema, q: &keel_sql::Select) -> R<Option<Vec<Row>>> {
        let Some(filter) = &q.filter else {
            return Ok(None);
        };
        let Some((m, op, val)) = self.find_indexed_pred(tid, schema, filter) else {
            return Ok(None);
        };

        if let Some(ts) = self.stats.borrow().get(&tid) {
            let col = &schema.columns[m.col_index].name;
            let pred = keel_sql::Expr::bin(
                op,
                keel_sql::Expr::col(col),
                keel_sql::Expr::Literal(val.clone()),
            );
            let sel = keel_stats::estimate_selectivity(&pred, schema, ts);
            if !keel_stats::prefer_index_scan(sel, INDEX_CROSSOVER) {
                return Ok(None);
            }
        }
        self.index_lookups.set(self.index_lookups.get() + 1);

        let enc = keel_keys::encode_value(m.col_type, &val);
        let (lo, hi) = index_bounds(op, &enc);
        let bt = BTree::open_rooted(&self.bp, m.root);
        let entries = bt
            .range(&lo, hi.as_deref())
            .map_err(|e| DbError::Exec(e.to_string()))?;
        let heap = HeapFile::open(&self.bp)?;
        let mut rows = Vec::new();
        for (_k, rid) in entries {
            if let Some(rec) = heap.get(rid)? {
                if rec.len() >= 2 && u16::from_le_bytes([rec[0], rec[1]]) == tid {
                    rows.push(
                        decode_record(schema, &rec[2..])
                            .map_err(|e| DbError::Exec(e.to_string()))?,
                    );
                }
            }
        }
        Ok(Some(rows))
    }

    fn find_indexed_pred(
        &self,
        tid: u16,
        schema: &Schema,
        filter: &keel_sql::Expr,
    ) -> Option<(IndexMeta, keel_sql::BinOp, Value)> {
        use keel_sql::BinOp;
        let mut conj = Vec::new();
        collect_conjuncts(filter, &mut conj);
        let idxs = self.indexes.borrow();
        let mut best: Option<(IndexMeta, BinOp, Value)> = None;
        for c in conj {
            let Some((name, op, lit)) = cmp_col_lit(c) else {
                continue;
            };
            if lit.is_null() {
                continue;
            }
            let Some(ci) = schema.column_index(&name) else {
                continue;
            };
            let Some(m) = idxs.iter().find(|m| m.table_id == tid && m.col_index == ci) else {
                continue;
            };
            let Ok(coerced) = coerce_into(lit, m.col_type) else {
                continue;
            };
            if op == BinOp::Eq {
                return Some((m.clone(), op, coerced));
            }
            best.get_or_insert((m.clone(), op, coerced));
        }
        best
    }

    fn scan_table(&self, tid: u16, schema: &Schema) -> R<Vec<Row>> {
        let heap = HeapFile::open(&self.bp)?;
        let mut rows = Vec::new();
        for (_rid, rec) in heap.scan()? {
            if rec.len() < 2 {
                continue;
            }
            if u16::from_le_bytes([rec[0], rec[1]]) == tid {
                rows.push(
                    decode_record(schema, &rec[2..]).map_err(|e| DbError::Exec(e.to_string()))?,
                );
            }
        }
        Ok(rows)
    }
}

fn is_mutating(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::CreateIndex(_)
            | Stmt::Insert(_)
            | Stmt::Delete(_)
            | Stmt::Update(_)
            | Stmt::DropTable(_)
            | Stmt::DropIndex(_)
    )
}

fn scan_cols(table: &str, schema: &Schema) -> Vec<(String, String)> {
    schema
        .columns
        .iter()
        .map(|c| (table.to_string(), c.name.clone()))
        .collect()
}

fn matches_pred(pred: &keel_sql::Expr, cols: &[(String, String)], row: &Row) -> R<bool> {
    let v = keel_sql::refengine::eval_public(pred, cols, row).map_err(DbError::from)?;
    Ok(matches!(v, Value::Bool(true)))
}

fn require_no_subquery(e: &keel_sql::Expr) -> R<()> {
    if keel_sql::refengine::is_subquery_free(e) {
        Ok(())
    } else {
        exec_err("subqueries are not supported in DELETE/UPDATE (yet)")
    }
}

fn write_index_catalog<P: keel_pager::Pager>(heap: &HeapFile<'_, P>, m: &IndexMeta) -> R<()> {
    let mut crec = INDEX_TID.to_le_bytes().to_vec();
    crec.extend(serialize_index(
        &m.name,
        m.table_id,
        m.col_index,
        m.col_type,
        m.root,
    ));
    heap.update(m.catalog_rid, &crec)?;
    Ok(())
}

fn collect_conjuncts<'a>(e: &'a keel_sql::Expr, out: &mut Vec<&'a keel_sql::Expr>) {
    use keel_sql::{BinOp, Expr};
    if let Expr::Binary {
        op: BinOp::And,
        left,
        right,
    } = e
    {
        collect_conjuncts(left, out);
        collect_conjuncts(right, out);
    } else {
        out.push(e);
    }
}

fn cmp_col_lit(e: &keel_sql::Expr) -> Option<(String, keel_sql::BinOp, Value)> {
    use keel_sql::{BinOp, Expr};
    let Expr::Binary { op, left, right } = e else {
        return None;
    };
    let op = *op;
    if !matches!(
        op,
        BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    ) {
        return None;
    }
    match (&**left, &**right) {
        (Expr::Column { name, .. }, Expr::Literal(v)) => Some((name.clone(), op, v.clone())),
        (Expr::Literal(v), Expr::Column { name, .. }) => {
            Some((name.clone(), flip_op(op), v.clone()))
        }
        _ => None,
    }
}

fn flip_op(op: keel_sql::BinOp) -> keel_sql::BinOp {
    use keel_sql::BinOp::*;
    match op {
        Lt => Gt,
        Le => Ge,
        Gt => Lt,
        Ge => Le,
        other => other,
    }
}

fn index_bounds(op: keel_sql::BinOp, enc: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    use keel_sql::BinOp::*;
    let mut past = enc.to_vec();
    past.extend_from_slice(&[0xFF; 6]);
    match op {
        Eq => (enc.to_vec(), Some(past)),
        Ge => (enc.to_vec(), None),
        Gt => (past, None),
        Le => (Vec::new(), Some(past)),
        Lt => (Vec::new(), Some(enc.to_vec())),
        _ => (Vec::new(), None),
    }
}

fn serialize_index(
    name: &str,
    tid: u16,
    col_index: usize,
    col_type: ColumnType,
    root: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, name);
    out.extend_from_slice(&tid.to_le_bytes());
    out.extend_from_slice(&(col_index as u32).to_le_bytes());
    let (tag, param) = type_tag(col_type);
    out.push(tag);
    out.extend_from_slice(&param.to_le_bytes());
    out.extend_from_slice(&root.to_le_bytes());
    out
}

fn parse_index(b: &[u8]) -> Result<IndexMeta, DbError> {
    let mut pos = 0;
    let name = get_str(b, &mut pos)?;
    let tid = u16::from_le_bytes([b[pos], b[pos + 1]]);
    pos += 2;
    let col_index = u32::from_le_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]) as usize;
    pos += 4;
    let tag = b[pos];
    pos += 1;
    let param = u16::from_le_bytes([b[pos], b[pos + 1]]);
    pos += 2;
    let root = u32::from_le_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]);
    Ok(IndexMeta {
        name,
        table_id: tid,
        col_index,
        col_type: type_from_tag(tag, param)?,
        root,
        catalog_rid: Rid::new(0, 0),
    })
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}
fn get_str(b: &[u8], pos: &mut usize) -> Result<String, DbError> {
    let len = u16::from_le_bytes([b[*pos], b[*pos + 1]]) as usize;
    *pos += 2;
    let s = String::from_utf8(b[*pos..*pos + len].to_vec())
        .map_err(|_| DbError::Exec("bad catalog utf8".into()))?;
    *pos += len;
    Ok(s)
}

fn type_tag(ty: ColumnType) -> (u8, u16) {
    match ty {
        ColumnType::Bool => (0, 0),
        ColumnType::Int => (1, 0),
        ColumnType::BigInt => (2, 0),
        ColumnType::Double => (3, 0),
        ColumnType::Varchar(n) => (4, n),
    }
}
fn type_from_tag(tag: u8, param: u16) -> Result<ColumnType, DbError> {
    Ok(match tag {
        0 => ColumnType::Bool,
        1 => ColumnType::Int,
        2 => ColumnType::BigInt,
        3 => ColumnType::Double,
        4 => ColumnType::Varchar(param),
        _ => return exec_err("bad catalog type tag"),
    })
}

fn serialize_catalog(tid: u16, name: &str, schema: &Schema) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&tid.to_le_bytes());
    put_str(&mut out, name);
    out.extend_from_slice(&(schema.columns.len() as u16).to_le_bytes());
    for c in &schema.columns {
        put_str(&mut out, &c.name);
        let (tag, param) = type_tag(c.ty);
        out.push(tag);
        out.extend_from_slice(&param.to_le_bytes());
        out.push(c.not_null as u8);
    }
    out
}

fn parse_catalog(b: &[u8]) -> Result<(u16, String, Schema), DbError> {
    let mut pos = 0;
    let tid = u16::from_le_bytes([b[pos], b[pos + 1]]);
    pos += 2;
    let name = get_str(b, &mut pos)?;
    let ncols = u16::from_le_bytes([b[pos], b[pos + 1]]) as usize;
    pos += 2;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let cname = get_str(b, &mut pos)?;
        let tag = b[pos];
        pos += 1;
        let param = u16::from_le_bytes([b[pos], b[pos + 1]]);
        pos += 2;
        let not_null = b[pos] != 0;
        pos += 1;
        cols.push(ColumnDef::new(cname, type_from_tag(tag, param)?, not_null));
    }
    Ok((tid, name, Schema::new(cols)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::MemDisk;

    fn fresh() -> Database {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        Database::open(disk, 32).unwrap()
    }

    #[test]
    fn create_insert_select_end_to_end() {
        let db = fresh();
        db.execute("CREATE TABLE t (id BIGINT, name VARCHAR(16), x INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'a',30)")
            .unwrap();
        let rs = db
            .execute("SELECT name, COUNT(*) FROM t GROUP BY name ORDER BY name")
            .unwrap()
            .unwrap();
        assert_eq!(rs.rows.len(), 2);
        assert_eq!(rs.rows[0][1], Value::BigInt(2));
    }

    #[test]
    fn persists_and_reopens() {
        let disk = Arc::new(MemDisk::new());
        {
            let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 32).unwrap();
            db.execute("CREATE TABLE acct (id INT, bal BIGINT)")
                .unwrap();
            db.execute("INSERT INTO acct VALUES (1,100),(2,200),(3,300)")
                .unwrap();
        }
        let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 32).unwrap();
        assert_eq!(db.table_names(), vec!["acct".to_string()]);
        let rs = db.execute("SELECT SUM(bal) FROM acct").unwrap().unwrap();
        assert_eq!(rs.rows[0][0], Value::BigInt(600));
    }

    #[test]
    fn index_point_lookup_correct_maintained_and_durable() {
        let disk = Arc::new(MemDisk::new());
        {
            let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
            db.execute("CREATE TABLE t (id INT, k INT)").unwrap();
            for i in 0..100 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 7))
                    .unwrap();
            }
            db.execute("CREATE INDEX ix ON t (k)").unwrap();
            for i in 100..170 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 7))
                    .unwrap();
            }
            for target in 0..7i64 {
                let rs = db
                    .execute(&format!("SELECT id FROM t WHERE k = {target} ORDER BY id"))
                    .unwrap()
                    .unwrap();
                let got: Vec<i64> = rs.rows.iter().map(|r| int_of(&r[0])).collect();
                let want: Vec<i64> = (0..170i64).filter(|i| i % 7 == target).collect();
                assert_eq!(got, want, "index scan for k={target}");
            }
            assert!(
                db.index_lookups() >= 7,
                "queries should have used the index"
            );
        }
        let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
        let rs = db
            .execute("SELECT id FROM t WHERE k = 5 ORDER BY id")
            .unwrap()
            .unwrap();
        let got: Vec<i64> = rs.rows.iter().map(|r| int_of(&r[0])).collect();
        let want: Vec<i64> = (0..170i64).filter(|i| i % 7 == 5).collect();
        assert_eq!(got, want, "index survived reopen");
        assert!(db.index_lookups() >= 1);
    }

    #[test]
    fn index_range_scans_correct() {
        let db = fresh();
        db.execute("CREATE TABLE t (id INT, k INT)").unwrap();
        for i in 0..100i64 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i})"))
                .unwrap();
        }
        db.execute("CREATE INDEX ix ON t (k)").unwrap();
        let cases: [(&str, Vec<i64>); 5] = [
            (
                "SELECT id FROM t WHERE k > 90 ORDER BY id",
                (91..100).collect(),
            ),
            (
                "SELECT id FROM t WHERE k >= 90 ORDER BY id",
                (90..100).collect(),
            ),
            ("SELECT id FROM t WHERE k < 5 ORDER BY id", (0..5).collect()),
            (
                "SELECT id FROM t WHERE k <= 5 ORDER BY id",
                (0..6).collect(),
            ),
            (
                "SELECT id FROM t WHERE k > 40 AND k < 45 ORDER BY id",
                (41..45).collect(),
            ),
        ];
        for (sql, want) in cases {
            let rs = db.execute(sql).unwrap().unwrap();
            let got: Vec<i64> = rs.rows.iter().map(|r| int_of(&r[0])).collect();
            assert_eq!(got, want, "for `{sql}`");
        }
        assert!(
            db.index_lookups() >= 5,
            "range queries should have used the index"
        );
    }

    #[test]
    fn cost_based_access_path_and_q_error() {
        let db = fresh();
        db.execute("CREATE TABLE t (id INT, k INT)").unwrap();
        for i in 0..1000i64 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 50))
                .unwrap();
        }
        db.execute("CREATE INDEX ix ON t (k)").unwrap();
        db.analyze().unwrap();

        let before = db.index_lookups();
        let rs = db.execute("SELECT id FROM t WHERE k = 7").unwrap().unwrap();
        assert_eq!(rs.rows.len(), 20);
        assert_eq!(
            db.index_lookups(),
            before + 1,
            "selective query should use the index"
        );

        let before = db.index_lookups();
        let rs = db
            .execute("SELECT id FROM t WHERE k >= 5")
            .unwrap()
            .unwrap();
        assert_eq!(rs.rows.len(), 900);
        assert_eq!(
            db.index_lookups(),
            before,
            "unselective query should NOT use the index"
        );

        let (est, act, qerr) = db.q_error("SELECT id FROM t WHERE k = 7").unwrap().unwrap();
        assert_eq!(act, 20.0);
        assert!(
            qerr < 2.0,
            "q-error {qerr:.2} (est {est}, act {act}) should be small for a uniform column"
        );
    }

    fn int_of(v: &Value) -> i64 {
        match v {
            Value::Int(n) => *n as i64,
            Value::BigInt(n) => *n,
            _ => panic!("expected int, got {v:?}"),
        }
    }

    #[test]
    fn drop_table_and_index() {
        let disk = Arc::new(MemDisk::new());
        {
            let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
            db.execute("CREATE TABLE t (id INT, k INT)").unwrap();
            db.execute("CREATE TABLE keep (id INT)").unwrap();
            for i in 0..30i64 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 5))
                    .unwrap();
            }
            db.execute("INSERT INTO keep VALUES (1),(2)").unwrap();
            db.execute("CREATE INDEX ix ON t (k)").unwrap();

            db.execute("DROP INDEX ix").unwrap();
            let before = db.index_lookups();
            let rs = db.execute("SELECT id FROM t WHERE k = 2").unwrap().unwrap();
            assert_eq!(
                rs.rows.len(),
                6,
                "k=2 rows still queryable after DROP INDEX"
            );
            assert_eq!(db.index_lookups(), before, "no index should be used");

            db.execute("DROP TABLE t").unwrap();
            assert_eq!(db.table_names(), vec!["keep".to_string()]);
            assert!(db.execute("SELECT id FROM t").is_err(), "t is gone");
            assert_eq!(
                db.execute("SELECT COUNT(*) FROM keep")
                    .unwrap()
                    .unwrap()
                    .rows[0][0],
                Value::BigInt(2)
            );

            assert!(db.execute("DROP TABLE nope").is_err());
            assert!(db.execute("DROP INDEX nope").is_err());
        }
        let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
        assert_eq!(db.table_names(), vec!["keep".to_string()]);
    }

    #[test]
    fn drop_then_recreate_same_name() {
        let db = fresh();
        db.execute("CREATE TABLE t (id INT, s VARCHAR(8))").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'old')").unwrap();
        db.execute("DROP TABLE t").unwrap();
        db.execute("CREATE TABLE t (id INT, n BIGINT)").unwrap();
        db.execute("INSERT INTO t VALUES (9, 900)").unwrap();
        let rs = db.execute("SELECT id, n FROM t").unwrap().unwrap();
        assert_eq!(rs.rows.len(), 1, "only the new row is visible");
        assert_eq!(rs.rows[0][0], Value::Int(9));
        assert_eq!(rs.rows[0][1], Value::BigInt(900));
    }

    #[test]
    fn delete_and_update_basic() {
        let db = fresh();
        db.execute("CREATE TABLE t (id INT, v INT)").unwrap();
        for i in 0..10i64 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 10))
                .unwrap();
        }
        db.execute("UPDATE t SET v = v + 1 WHERE id < 3").unwrap();
        let rs = db
            .execute("SELECT id, v FROM t WHERE id < 3 ORDER BY id")
            .unwrap()
            .unwrap();
        assert_eq!(rs.rows[0][1], Value::Int(1));
        assert_eq!(rs.rows[1][1], Value::Int(11));
        assert_eq!(rs.rows[2][1], Value::Int(21));

        db.execute("DELETE FROM t WHERE id >= 5").unwrap();
        let rs = db.execute("SELECT COUNT(*) FROM t").unwrap().unwrap();
        assert_eq!(rs.rows[0][0], Value::BigInt(5));

        db.execute("DELETE FROM t").unwrap();
        assert_eq!(
            db.execute("SELECT COUNT(*) FROM t").unwrap().unwrap().rows[0][0],
            Value::BigInt(0)
        );
    }

    #[test]
    fn dml_maintains_index_and_persists() {
        let disk = Arc::new(MemDisk::new());
        {
            let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
            db.execute("CREATE TABLE t (id INT, k INT)").unwrap();
            for i in 0..60i64 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 6))
                    .unwrap();
            }
            db.execute("CREATE INDEX ix ON t (k)").unwrap();
            db.execute("UPDATE t SET k = 99 WHERE k = 0").unwrap();
            db.execute("DELETE FROM t WHERE k = 1").unwrap();

            let moved = db
                .execute("SELECT id FROM t WHERE k = 99 ORDER BY id")
                .unwrap()
                .unwrap();
            let got: Vec<i64> = moved.rows.iter().map(|r| int_of(&r[0])).collect();
            let want: Vec<i64> = (0..60).filter(|i| i % 6 == 0).collect();
            assert_eq!(got, want, "moved rows found under new key via index");
            assert_eq!(
                db.execute("SELECT id FROM t WHERE k = 0")
                    .unwrap()
                    .unwrap()
                    .rows
                    .len(),
                0,
                "no rows remain under the vacated key"
            );
            assert_eq!(
                db.execute("SELECT id FROM t WHERE k = 1")
                    .unwrap()
                    .unwrap()
                    .rows
                    .len(),
                0
            );
        }
        let db = Database::open(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
        let got: Vec<i64> = db
            .execute("SELECT id FROM t WHERE k = 99 ORDER BY id")
            .unwrap()
            .unwrap()
            .rows
            .iter()
            .map(|r| int_of(&r[0]))
            .collect();
        let want: Vec<i64> = (0..60).filter(|i| i % 6 == 0).collect();
        assert_eq!(got, want, "DML survived reopen (index consistent)");
    }

    #[test]
    fn dml_differential_vs_reference() {
        use keel_rng::Rng;
        for seed in 0..20u64 {
            let db = fresh();
            db.execute("CREATE TABLE t (id INT, a INT, b INT)").unwrap();
            let mut mem = MemDb::new();
            mem.execute(&parse_statement("CREATE TABLE t (id INT, a INT, b INT)").unwrap())
                .unwrap();

            let mut rng = Rng::seed(seed);
            for id in 0..40i64 {
                let a = (rng.next_u64() % 10) as i64;
                let b = (rng.next_u64() % 10) as i64;
                let sql = format!("INSERT INTO t VALUES ({id}, {a}, {b})");
                db.execute(&sql).unwrap();
                mem.execute(&parse_statement(&sql).unwrap()).unwrap();
            }
            for _ in 0..30 {
                let sql = match rng.next_u64() % 3 {
                    0 => {
                        let thr = (rng.next_u64() % 10) as i64;
                        format!("UPDATE t SET a = a + 1 WHERE b >= {thr}")
                    }
                    1 => {
                        let v = (rng.next_u64() % 10) as i64;
                        format!(
                            "UPDATE t SET b = {v} WHERE a = {}",
                            (rng.next_u64() % 10) as i64
                        )
                    }
                    _ => {
                        let thr = (rng.next_u64() % 12) as i64;
                        format!("DELETE FROM t WHERE a < {thr}")
                    }
                };
                db.execute(&sql).unwrap();
                mem.execute(&parse_statement(&sql).unwrap()).unwrap();
            }
            let q = "SELECT id, a, b FROM t ORDER BY id";
            let got = db.execute(q).unwrap().unwrap();
            let want = mem.execute(&parse_statement(q).unwrap()).unwrap().unwrap();
            assert_eq!(
                got.rows, want.rows,
                "seed {seed}: storage vs reference diverged"
            );
        }
    }

    #[test]
    fn cost_based_join_reordering() {
        let db = fresh();
        db.execute("CREATE TABLE big (id INT, k INT)").unwrap();
        db.execute("CREATE TABLE small (k INT, j INT)").unwrap();
        db.execute("CREATE TABLE mid (j INT, z INT)").unwrap();
        for i in 0..200i64 {
            db.execute(&format!("INSERT INTO big VALUES ({i}, {})", i % 4))
                .unwrap();
        }
        for i in 0..4i64 {
            db.execute(&format!("INSERT INTO small VALUES ({i}, {i})"))
                .unwrap();
        }
        for i in 0..30i64 {
            db.execute(&format!("INSERT INTO mid VALUES ({i}, {})", i % 10))
                .unwrap();
        }
        let order = db
            .join_order(
                "SELECT big.id FROM big JOIN small ON big.k = small.k JOIN mid ON small.j = mid.j",
            )
            .unwrap()
            .unwrap();
        assert_eq!(order.len(), 3);
        assert_ne!(
            order,
            vec!["big".to_string(), "small".to_string(), "mid".to_string()],
            "reordering should differ from FROM order, got {order:?}"
        );
        assert_eq!(
            order[2], "big",
            "cost model joins the big table last, got {order:?}"
        );

        let sql = "SELECT big.id, mid.z FROM big JOIN small ON big.k = small.k \
                   JOIN mid ON small.j = mid.j ORDER BY big.id, mid.z";
        let got = db.execute(sql).unwrap().unwrap();
        let mut mem = MemDb::new();
        for stmt in [
            "CREATE TABLE big (id INT, k INT)",
            "CREATE TABLE small (k INT, j INT)",
            "CREATE TABLE mid (j INT, z INT)",
        ] {
            mem.execute(&parse_statement(stmt).unwrap()).unwrap();
        }
        for i in 0..200i64 {
            mem.execute(
                &parse_statement(&format!("INSERT INTO big VALUES ({i}, {})", i % 4)).unwrap(),
            )
            .unwrap();
        }
        for i in 0..4i64 {
            mem.execute(&parse_statement(&format!("INSERT INTO small VALUES ({i}, {i})")).unwrap())
                .unwrap();
        }
        for i in 0..30i64 {
            mem.execute(
                &parse_statement(&format!("INSERT INTO mid VALUES ({i}, {})", i % 10)).unwrap(),
            )
            .unwrap();
        }
        let want = mem
            .execute(&parse_statement(sql).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            got.rows, want.rows,
            "reordered join result must match the oracle"
        );
    }

    #[test]
    fn join_over_storage() {
        let db = fresh();
        db.execute("CREATE TABLE a (id INT, v INT)").unwrap();
        db.execute("CREATE TABLE b (id INT, w INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1,10),(2,20),(3,30)")
            .unwrap();
        db.execute("INSERT INTO b VALUES (1,100),(2,200)").unwrap();
        let rs = db
            .execute("SELECT a.id, b.w FROM a LEFT JOIN b ON a.id = b.id ORDER BY id")
            .unwrap()
            .unwrap();
        assert_eq!(rs.rows.len(), 3);
        assert_eq!(rs.rows[2][1], Value::Null);
    }

    #[test]
    fn join_differential_vs_reference() {
        use keel_rng::Rng;
        for seed in 0..25u64 {
            let db = fresh();
            let mut mem = MemDb::new();
            for ddl in [
                "CREATE TABLE a (id INT, k INT, v INT)",
                "CREATE TABLE b (k INT, w INT)",
            ] {
                db.execute(ddl).unwrap();
                mem.execute(&parse_statement(ddl).unwrap()).unwrap();
            }
            let mut rng = Rng::seed(seed);
            for id in 0..25i64 {
                let kexpr = if rng.next_u64().is_multiple_of(7) {
                    "NULL".to_string()
                } else {
                    (rng.next_u64() % 6).to_string()
                };
                let v = (rng.next_u64() % 100) as i64;
                let sql = format!("INSERT INTO a VALUES ({id}, {kexpr}, {v})");
                db.execute(&sql).unwrap();
                mem.execute(&parse_statement(&sql).unwrap()).unwrap();
            }
            for _ in 0..15 {
                let k = (rng.next_u64() % 8) as i64;
                let w = (rng.next_u64() % 100) as i64;
                let sql = format!("INSERT INTO b VALUES ({k}, {w})");
                db.execute(&sql).unwrap();
                mem.execute(&parse_statement(&sql).unwrap()).unwrap();
            }
            let queries = [
                "SELECT a.id, b.w FROM a JOIN b ON a.k = b.k ORDER BY a.id, b.w",
                "SELECT a.id, a.v, b.w FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.id, b.w",
                "SELECT a.id, b.w FROM a JOIN b ON a.k = b.k WHERE b.w > 50 ORDER BY a.id, b.w",
                "SELECT DISTINCT a.k FROM a JOIN b ON a.k = b.k ORDER BY a.k",
                "SELECT a.id, b.w FROM a LEFT JOIN b ON b.k = a.k WHERE a.v > 20 ORDER BY a.id, b.w",
            ];
            for q in queries {
                let got = db.execute(q).unwrap().unwrap();
                let want = mem.execute(&parse_statement(q).unwrap()).unwrap().unwrap();
                assert_eq!(got.rows, want.rows, "seed {seed}: `{q}`");
                assert_eq!(got.columns, want.columns, "seed {seed} cols: `{q}`");
            }
            assert_eq!(
                db.join_streams(),
                queries.len() as u64,
                "seed {seed}: streaming hash-join path should serve every query"
            );
        }
    }
}

#[cfg(test)]
mod concurrency {
    use super::*;
    use keel_vfs::MemDisk;
    use std::sync::Mutex;
    use std::thread;

    #[test]
    fn database_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Database>();
    }

    #[test]
    fn concurrent_writers_via_shared_handle() {
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open(disk, 64).unwrap();
        db.execute("CREATE TABLE t (id INT, who INT)").unwrap();
        let db = Arc::new(Mutex::new(db));

        const THREADS: i64 = 8;
        const PER: i64 = 100;
        let mut handles = Vec::new();
        for who in 0..THREADS {
            let db = db.clone();
            handles.push(thread::spawn(move || {
                for i in 0..PER {
                    let id = who * PER + i;
                    db.lock()
                        .unwrap()
                        .execute(&format!("INSERT INTO t VALUES ({id}, {who})"))
                        .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let db = db.lock().unwrap();
        let n = db.execute("SELECT COUNT(*) FROM t").unwrap().unwrap();
        assert_eq!(n.rows[0][0], Value::BigInt(THREADS * PER));
        let ids = db.execute("SELECT id FROM t ORDER BY id").unwrap().unwrap();
        for (expected, row) in ids.rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(expected as i32));
        }
    }

    #[test]
    fn concurrent_sql_transfers_conserve_money() {
        const ACCOUNTS: i64 = 8;
        const START: i64 = 1000;
        let disk = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
        let db = Database::open(disk, 64).unwrap();
        db.execute("CREATE TABLE acct (id INT, bal BIGINT)")
            .unwrap();
        for i in 0..ACCOUNTS {
            db.execute(&format!("INSERT INTO acct VALUES ({i}, {START})"))
                .unwrap();
        }
        db.execute("CREATE INDEX ix ON acct (id)").unwrap();
        let db = Arc::new(Mutex::new(db));

        let mut handles = Vec::new();
        for tid in 0..6 {
            let db = db.clone();
            handles.push(thread::spawn(move || {
                let mut s = 0x2545_F491_4F6C_DD1Du64 ^ ((tid as u64 + 1) << 24);
                let mut next = || {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    s
                };
                for _ in 0..150 {
                    let from = (next() as i64).rem_euclid(ACCOUNTS);
                    let mut to = (next() as i64).rem_euclid(ACCOUNTS);
                    if to == from {
                        to = (to + 1) % ACCOUNTS;
                    }
                    let amt = 1 + (next() % 10) as i64;
                    let db = db.lock().unwrap();
                    let get = |id: i64| -> i64 {
                        let rs = db
                            .execute(&format!("SELECT bal FROM acct WHERE id = {id}"))
                            .unwrap()
                            .unwrap();
                        match rs.rows[0][0] {
                            Value::BigInt(n) => n,
                            _ => unreachable!(),
                        }
                    };
                    let (fb, tb) = (get(from), get(to));
                    db.execute(&format!(
                        "UPDATE acct SET bal = {} WHERE id = {from}",
                        fb - amt
                    ))
                    .unwrap();
                    db.execute(&format!(
                        "UPDATE acct SET bal = {} WHERE id = {to}",
                        tb + amt
                    ))
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let db = db.lock().unwrap();
        let total = db.execute("SELECT SUM(bal) FROM acct").unwrap().unwrap();
        assert_eq!(
            total.rows[0][0],
            Value::BigInt(ACCOUNTS * START),
            "money must be conserved across concurrent SQL transfers"
        );
    }
}
