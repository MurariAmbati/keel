use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};

use keel_buffer::{BufferError, BufferPool, PageId, WalSync};
use keel_page::{crc32, raw, PageType};
use keel_pager::RecoveryPager;
use keel_vfs::BlockFile;

pub type Lsn = u64;
pub const NULL_LSN: Lsn = 0;

const MAGIC: &[u8; 8] = b"KEELWAL1";

const TAG_BEGIN: u8 = 0;
const TAG_COMMIT: u8 = 1;
const TAG_ABORT: u8 = 2;
const TAG_PAGE_INIT: u8 = 3;
const TAG_UPDATE: u8 = 4;
const TAG_FULL_PAGE: u8 = 5;
const TAG_CLR: u8 = 6;
const TAG_CHECKPOINT: u8 = 7;

#[derive(Debug)]
pub enum WalError {
    Io(io::Error),
    Buffer(BufferError),
    BadLog,
}
impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "io: {e}"),
            WalError::Buffer(e) => write!(f, "{e}"),
            WalError::BadLog => write!(f, "bad log stream"),
        }
    }
}
impl std::error::Error for WalError {}
impl From<io::Error> for WalError {
    fn from(e: io::Error) -> Self {
        WalError::Io(e)
    }
}
impl From<keel_pager::PagerError> for WalError {
    fn from(e: keel_pager::PagerError) -> Self {
        WalError::Buffer(match e {
            keel_pager::PagerError::Io(e) => BufferError::Io(e),
            keel_pager::PagerError::Corrupt(p) => BufferError::Corrupt(p),
            keel_pager::PagerError::Exhausted => BufferError::Exhausted,
        })
    }
}
impl From<BufferError> for WalError {
    fn from(e: BufferError) -> Self {
        WalError::Buffer(e)
    }
}
pub type Result<T> = std::result::Result<T, WalError>;

#[derive(Clone, Debug)]
pub enum Kind {
    Begin,
    Commit,
    Abort,
    PageInit {
        page: PageId,
        ptype: u8,
    },
    Update {
        page: PageId,
        offset: u32,
        before: Vec<u8>,
        after: Vec<u8>,
    },
    FullPage {
        page: PageId,
        after: Vec<u8>,
        offset: u32,
        before: Vec<u8>,
    },
    Clr {
        page: PageId,
        offset: u32,
        bytes: Vec<u8>,
        undo_next: Lsn,
    },
    Checkpoint {
        att: Vec<(u64, Lsn)>,
        dpt: Vec<(PageId, Lsn)>,
    },
}

#[derive(Clone, Debug)]
pub struct LogRecord {
    pub lsn: Lsn,
    pub prev_lsn: Lsn,
    pub txn: u64,
    pub kind: Kind,
}

fn serialize(lsn: Lsn, prev: Lsn, txn: u64, kind: &Kind) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&lsn.to_le_bytes());
    b.extend_from_slice(&prev.to_le_bytes());
    b.extend_from_slice(&txn.to_le_bytes());
    match kind {
        Kind::Begin => b.push(TAG_BEGIN),
        Kind::Commit => b.push(TAG_COMMIT),
        Kind::Abort => b.push(TAG_ABORT),
        Kind::PageInit { page, ptype } => {
            b.push(TAG_PAGE_INIT);
            b.extend_from_slice(&page.to_le_bytes());
            b.push(*ptype);
        }
        Kind::Update {
            page,
            offset,
            before,
            after,
        } => {
            b.push(TAG_UPDATE);
            b.extend_from_slice(&page.to_le_bytes());
            b.extend_from_slice(&offset.to_le_bytes());
            b.extend_from_slice(&(before.len() as u32).to_le_bytes());
            b.extend_from_slice(before);
            b.extend_from_slice(&(after.len() as u32).to_le_bytes());
            b.extend_from_slice(after);
        }
        Kind::FullPage {
            page,
            after,
            offset,
            before,
        } => {
            b.push(TAG_FULL_PAGE);
            b.extend_from_slice(&page.to_le_bytes());
            b.extend_from_slice(&(after.len() as u32).to_le_bytes());
            b.extend_from_slice(after);
            b.extend_from_slice(&offset.to_le_bytes());
            b.extend_from_slice(&(before.len() as u32).to_le_bytes());
            b.extend_from_slice(before);
        }
        Kind::Clr {
            page,
            offset,
            bytes,
            undo_next,
        } => {
            b.push(TAG_CLR);
            b.extend_from_slice(&page.to_le_bytes());
            b.extend_from_slice(&offset.to_le_bytes());
            b.extend_from_slice(&undo_next.to_le_bytes());
            b.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            b.extend_from_slice(bytes);
        }
        Kind::Checkpoint { att, dpt } => {
            b.push(TAG_CHECKPOINT);
            b.extend_from_slice(&(att.len() as u32).to_le_bytes());
            for (t, l) in att {
                b.extend_from_slice(&t.to_le_bytes());
                b.extend_from_slice(&l.to_le_bytes());
            }
            b.extend_from_slice(&(dpt.len() as u32).to_le_bytes());
            for (p, r) in dpt {
                b.extend_from_slice(&p.to_le_bytes());
                b.extend_from_slice(&r.to_le_bytes());
            }
        }
    }
    let total = (b.len() + 4) as u32;
    b[0..4].copy_from_slice(&total.to_le_bytes());
    let crc = crc32(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn rd_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

fn parse_at(bytes: &[u8], pos: usize) -> Option<(LogRecord, usize)> {
    if pos + 4 > bytes.len() {
        return None;
    }
    let total = rd_u32(bytes, pos) as usize;
    if total < 29 + 4 || pos + total > bytes.len() {
        return None;
    }
    let rec = &bytes[pos..pos + total];
    let crc_stored = rd_u32(rec, total - 4);
    if crc_stored != crc32(&rec[..total - 4]) {
        return None;
    }
    let lsn = rd_u64(rec, 4);
    let prev = rd_u64(rec, 12);
    let txn = rd_u64(rec, 20);
    let tag = rec[28];
    let payload = &rec[29..total - 4];
    let kind = match tag {
        TAG_BEGIN => Kind::Begin,
        TAG_COMMIT => Kind::Commit,
        TAG_ABORT => Kind::Abort,
        TAG_PAGE_INIT => Kind::PageInit {
            page: rd_u32(payload, 0),
            ptype: payload[4],
        },
        TAG_UPDATE => {
            let page = rd_u32(payload, 0);
            let offset = rd_u32(payload, 4);
            let blen = rd_u32(payload, 8) as usize;
            let before = payload[12..12 + blen].to_vec();
            let mut p = 12 + blen;
            let alen = rd_u32(payload, p) as usize;
            p += 4;
            let after = payload[p..p + alen].to_vec();
            Kind::Update {
                page,
                offset,
                before,
                after,
            }
        }
        TAG_FULL_PAGE => {
            let page = rd_u32(payload, 0);
            let alen = rd_u32(payload, 4) as usize;
            let after = payload[8..8 + alen].to_vec();
            let mut p = 8 + alen;
            let offset = rd_u32(payload, p);
            p += 4;
            let blen = rd_u32(payload, p) as usize;
            p += 4;
            let before = payload[p..p + blen].to_vec();
            Kind::FullPage {
                page,
                after,
                offset,
                before,
            }
        }
        TAG_CLR => {
            let page = rd_u32(payload, 0);
            let offset = rd_u32(payload, 4);
            let undo_next = rd_u64(payload, 8);
            let len = rd_u32(payload, 16) as usize;
            Kind::Clr {
                page,
                offset,
                bytes: payload[20..20 + len].to_vec(),
                undo_next,
            }
        }
        TAG_CHECKPOINT => {
            let natt = rd_u32(payload, 0) as usize;
            let mut p = 4;
            let mut att = Vec::with_capacity(natt);
            for _ in 0..natt {
                att.push((rd_u64(payload, p), rd_u64(payload, p + 8)));
                p += 16;
            }
            let ndpt = rd_u32(payload, p) as usize;
            p += 4;
            let mut dpt = Vec::with_capacity(ndpt);
            for _ in 0..ndpt {
                dpt.push((rd_u32(payload, p), rd_u64(payload, p + 4)));
                p += 12;
            }
            Kind::Checkpoint { att, dpt }
        }
        _ => return None,
    };
    Some((
        LogRecord {
            lsn,
            prev_lsn: prev,
            txn,
            kind,
        },
        total,
    ))
}

pub struct Log {
    file: Arc<dyn BlockFile>,
    end: u64,
    durable: u64,
    buf: Vec<u8>,
}

impl Log {
    pub fn create(file: Arc<dyn BlockFile>) -> Log {
        Log {
            file,
            end: MAGIC.len() as u64,
            durable: 0,
            buf: MAGIC.to_vec(),
        }
    }

    pub fn open_for_append(file: Arc<dyn BlockFile>, at: u64) -> Log {
        Log {
            file,
            end: at,
            durable: at,
            buf: Vec::new(),
        }
    }

    pub fn durable_lsn(&self) -> Lsn {
        self.durable
    }
    pub fn end_lsn(&self) -> Lsn {
        self.end
    }

    pub fn append(&mut self, prev: Lsn, txn: u64, kind: Kind) -> Lsn {
        let lsn = self.end;
        let bytes = serialize(lsn, prev, txn, &kind);
        self.buf.extend_from_slice(&bytes);
        self.end += bytes.len() as u64;
        lsn
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.file.write_at(&self.buf, self.durable)?;
        self.file.sync()?;
        self.durable = self.end;
        self.buf.clear();
        Ok(())
    }

    pub fn flush_until(&mut self, lsn: Lsn) -> io::Result<()> {
        if self.durable < lsn {
            self.flush()?;
        }
        Ok(())
    }
}

pub fn read_records(file: &dyn BlockFile) -> Result<Vec<LogRecord>> {
    Ok(read_records_end(file)?.0)
}

pub fn read_records_end(file: &dyn BlockFile) -> Result<(Vec<LogRecord>, u64)> {
    let size = file.size()? as usize;
    let mut bytes = vec![0u8; size];
    if size > 0 {
        file.read_at(&mut bytes, 0)?;
    }
    if size < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        if size == 0 {
            return Ok((Vec::new(), MAGIC.len() as u64));
        }
        return Err(WalError::BadLog);
    }
    let mut out = Vec::new();
    let mut pos = MAGIC.len();
    while let Some((rec, consumed)) = parse_at(&bytes, pos) {
        pos += consumed;
        out.push(rec);
    }
    Ok((out, pos as u64))
}

struct LogWal {
    log: Arc<Mutex<Log>>,
}
impl WalSync for LogWal {
    fn flushed_lsn(&self) -> u64 {
        self.log.lock().unwrap().durable_lsn()
    }
    fn flush_until(&self, lsn: u64) -> io::Result<()> {
        self.log.lock().unwrap().flush_until(lsn)
    }
}

struct Txn {
    id: u64,
    prev: Lsn,
    dirtied: Vec<PageId>,
    undo: Vec<(PageId, usize, Vec<u8>, Lsn)>,
}

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub steal: bool,
    pub force: bool,
    pub fpw: bool,
}

impl Policy {
    pub fn rung1() -> Self {
        Policy {
            steal: false,
            force: true,
            fpw: false,
        }
    }
    pub fn rung2() -> Self {
        Policy {
            steal: true,
            force: false,
            fpw: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WalStats {
    pub commits: u64,
    pub aborts: u64,
    pub update_records: u64,
    pub full_page_records: u64,
    pub full_page_bytes: u64,
    pub clrs: u64,
    pub checkpoints: u64,
}

pub struct TxnStore<P: RecoveryPager = BufferPool> {
    bp: P,
    log: Arc<Mutex<Log>>,
    cur: Mutex<Option<Txn>>,
    next_txn: Cell<u64>,
    policy: Policy,
    fpi_done: Mutex<HashSet<PageId>>,
    stats: Cell<WalStats>,
}

impl TxnStore<BufferPool> {
    pub fn open(
        data: Arc<dyn BlockFile>,
        log_file: Arc<dyn BlockFile>,
        frames: usize,
    ) -> Result<Self> {
        Self::open_with(data, log_file, frames, Policy::rung1())
    }

    pub fn open_with(
        data: Arc<dyn BlockFile>,
        log_file: Arc<dyn BlockFile>,
        frames: usize,
        policy: Policy,
    ) -> Result<Self> {
        let log = Arc::new(Mutex::new(Log::create(log_file)));
        let wal: Box<dyn WalSync + Send> = Box::new(LogWal { log: log.clone() });
        let bp = BufferPool::open(data, frames, wal)?;
        if !policy.steal {
            bp.set_no_steal();
        }
        Ok(TxnStore {
            bp,
            log,
            cur: Mutex::new(None),
            next_txn: Cell::new(1),
            policy,
            fpi_done: Mutex::new(HashSet::new()),
            stats: Cell::new(WalStats::default()),
        })
    }
}

impl<P: RecoveryPager> TxnStore<P> {
    pub fn with_pager(bp: P, log: Arc<Mutex<Log>>, policy: Policy) -> Self {
        if !policy.steal {
            bp.set_no_steal();
        }
        TxnStore {
            bp,
            log,
            cur: Mutex::new(None),
            next_txn: Cell::new(1),
            policy,
            fpi_done: Mutex::new(HashSet::new()),
            stats: Cell::new(WalStats::default()),
        }
    }

    pub fn stats(&self) -> WalStats {
        self.stats.get()
    }

    fn bump<F: FnOnce(&mut WalStats)>(&self, f: F) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }

    pub fn page_count(&self) -> PageId {
        keel_pager::Pager::page_count(&self.bp)
    }

    pub fn begin(&self) -> u64 {
        assert!(
            self.cur.lock().unwrap().is_none(),
            "a transaction is already active (serial only)"
        );
        let id = self.next_txn.get();
        self.next_txn.set(id + 1);
        let lsn = self.log.lock().unwrap().append(NULL_LSN, id, Kind::Begin);
        *self.cur.lock().unwrap() = Some(Txn {
            id,
            prev: lsn,
            dirtied: Vec::new(),
            undo: Vec::new(),
        });
        id
    }

    pub fn create_page(&self, ptype: PageType) -> Result<PageId> {
        let pid = self.bp.alloc_raw(ptype)?;
        let mut cur = self.cur.lock().unwrap();
        let txn = cur.as_mut().expect("no active transaction");
        let lsn = self.log.lock().unwrap().append(
            txn.prev,
            txn.id,
            Kind::PageInit {
                page: pid,
                ptype: ptype as u8,
            },
        );
        self.bp
            .with_page_mut(pid, |b| keel_page::raw::set_page_lsn(b, lsn))?;
        self.bp.note_dirty(pid, lsn);
        txn.prev = lsn;
        txn.dirtied.push(pid);
        Ok(pid)
    }

    pub fn write(&self, page: PageId, offset: usize, bytes: &[u8]) -> Result<()> {
        let mut cur = self.cur.lock().unwrap();
        let txn = cur.as_mut().expect("no active transaction");
        let (lsn, before, undo_next) = self.bp.with_page_mut(page, |b| {
            let before = keel_page::raw::body(b)[offset..offset + bytes.len()].to_vec();
            let undo_next = txn.prev;

            let fpi = self.policy.fpw && !self.fpi_done.lock().unwrap().contains(&page);
            let lsn = if fpi {
                let mut snap = b.to_vec();
                keel_page::raw::body_mut(&mut snap)[offset..offset + bytes.len()]
                    .copy_from_slice(bytes);
                let lsn = self.log.lock().unwrap().append(
                    txn.prev,
                    txn.id,
                    Kind::FullPage {
                        page,
                        after: snap.clone(),
                        offset: offset as u32,
                        before: before.clone(),
                    },
                );
                b.copy_from_slice(&snap);
                keel_page::raw::set_page_lsn(b, lsn);
                self.fpi_done.lock().unwrap().insert(page);
                self.bump(|s| {
                    s.full_page_records += 1;
                    s.full_page_bytes += snap.len() as u64;
                });
                lsn
            } else {
                let lsn = self.log.lock().unwrap().append(
                    txn.prev,
                    txn.id,
                    Kind::Update {
                        page,
                        offset: offset as u32,
                        before: before.clone(),
                        after: bytes.to_vec(),
                    },
                );
                keel_page::raw::set_page_lsn(b, lsn);
                keel_page::raw::body_mut(b)[offset..offset + bytes.len()].copy_from_slice(bytes);
                self.bump(|s| s.update_records += 1);
                lsn
            };
            (lsn, before, undo_next)
        })?;

        self.bp.note_dirty(page, lsn);
        txn.undo.push((page, offset, before, undo_next));
        txn.prev = lsn;
        if !txn.dirtied.contains(&page) {
            txn.dirtied.push(page);
        }
        Ok(())
    }

    pub fn read(&self, page: PageId, offset: usize, len: usize) -> Result<Vec<u8>> {
        Ok(self
            .bp
            .with_page(page, |b| raw::body(b)[offset..offset + len].to_vec())?)
    }

    pub fn commit(&self) -> Result<()> {
        let txn = self
            .cur
            .lock()
            .unwrap()
            .take()
            .expect("no active transaction");
        {
            let mut log = self.log.lock().unwrap();
            log.append(txn.prev, txn.id, Kind::Commit);
            log.flush()?;
        }
        if self.policy.force {
            for &pid in &txn.dirtied {
                self.bp.flush_page(pid)?;
            }
            self.bp.sync()?;
        }
        self.bump(|s| s.commits += 1);
        Ok(())
    }

    pub fn abort(&self) {
        let mut txn = self
            .cur
            .lock()
            .unwrap()
            .take()
            .expect("no active transaction");
        if self.policy.steal {
            while let Some((page, offset, before, undo_next)) = txn.undo.pop() {
                let clr_lsn = self.log.lock().unwrap().append(
                    txn.prev,
                    txn.id,
                    Kind::Clr {
                        page,
                        offset: offset as u32,
                        bytes: before.clone(),
                        undo_next,
                    },
                );
                self.bp
                    .with_page_mut(page, |b| {
                        keel_page::raw::set_page_lsn(b, clr_lsn);
                        keel_page::raw::body_mut(b)[offset..offset + before.len()]
                            .copy_from_slice(&before);
                    })
                    .expect("abort fetch");
                self.bp.note_dirty(page, clr_lsn);
                txn.prev = clr_lsn;
                self.bump(|s| s.clrs += 1);
            }
            self.log
                .lock()
                .unwrap()
                .append(txn.prev, txn.id, Kind::Abort);
        } else {
            for &pid in &txn.dirtied {
                self.bp.invalidate(pid);
            }
        }
        self.bump(|s| s.aborts += 1);
    }

    pub fn checkpoint(&self) -> Result<()> {
        keel_pager::Pager::checkpoint(&self.bp)?;
        let att: Vec<(u64, Lsn)> = self
            .cur
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| vec![(t.id, t.prev)])
            .unwrap_or_default();
        let dpt = self.bp.dpt_snapshot();
        {
            let mut log = self.log.lock().unwrap();
            log.append(NULL_LSN, 0, Kind::Checkpoint { att, dpt });
            log.flush()?;
        }
        self.fpi_done.lock().unwrap().clear();
        self.bump(|s| s.checkpoints += 1);
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryReport {
    pub total_records: usize,
    pub committed_txns: usize,
    pub redone: u64,
    pub losers: usize,
    pub undone: u64,
}

pub fn recover(
    data: Arc<dyn BlockFile>,
    log_file: Arc<dyn BlockFile>,
    frames: usize,
) -> Result<RecoveryReport> {
    let records = read_records(&*log_file)?;
    let committed: HashSet<u64> = records
        .iter()
        .filter(|r| matches!(r.kind, Kind::Commit))
        .map(|r| r.txn)
        .collect();

    let bp = BufferPool::open_default(data, frames)?;
    let mut report = RecoveryReport {
        total_records: records.len(),
        committed_txns: committed.len(),
        redone: 0,
        ..Default::default()
    };

    for r in &records {
        if !committed.contains(&r.txn) {
            continue;
        }
        match &r.kind {
            Kind::PageInit { page, ptype } => {
                let mut g = bp.fetch_write_for_redo(*page)?;
                if raw::page_lsn(g.bytes()) < r.lsn {
                    let pt = PageType::from_u8(*ptype).unwrap_or(PageType::Overflow);
                    raw::init_header(g.bytes_mut(), pt);
                    raw::set_page_lsn(g.bytes_mut(), r.lsn);
                    report.redone += 1;
                }
            }
            Kind::Update {
                page,
                offset,
                after,
                ..
            } => {
                let mut g = bp.fetch_write_for_redo(*page)?;
                if raw::page_lsn(g.bytes()) < r.lsn {
                    let body = raw::body_mut(g.bytes_mut());
                    let off = *offset as usize;
                    body[off..off + after.len()].copy_from_slice(after);
                    raw::set_page_lsn(g.bytes_mut(), r.lsn);
                    report.redone += 1;
                }
            }
            Kind::FullPage { page, after, .. } => {
                let mut g = bp.fetch_write_for_redo(*page)?;
                if raw::page_lsn(g.bytes()) < r.lsn {
                    g.bytes_mut().copy_from_slice(after);
                    raw::set_page_lsn(g.bytes_mut(), r.lsn);
                    report.redone += 1;
                }
            }
            _ => {}
        }
    }
    bp.checkpoint()?;
    Ok(report)
}

type Applier = Box<dyn Fn(&mut [u8])>;

fn redo_record(bp: &BufferPool, r: &LogRecord) -> Result<bool> {
    let (page, apply): (PageId, Applier) = match &r.kind {
        Kind::PageInit { page, ptype } => {
            let pt = PageType::from_u8(*ptype).unwrap_or(PageType::Overflow);
            (*page, Box::new(move |b: &mut [u8]| raw::init_header(b, pt)))
        }
        Kind::Update {
            page,
            offset,
            after,
            ..
        } => {
            let off = *offset as usize;
            let after = after.clone();
            (
                *page,
                Box::new(move |b: &mut [u8]| {
                    raw::body_mut(b)[off..off + after.len()].copy_from_slice(&after)
                }),
            )
        }
        Kind::FullPage { page, after, .. } => {
            let after = after.clone();
            (
                *page,
                Box::new(move |b: &mut [u8]| b.copy_from_slice(&after)),
            )
        }
        Kind::Clr {
            page,
            offset,
            bytes,
            ..
        } => {
            let off = *offset as usize;
            let bytes = bytes.clone();
            (
                *page,
                Box::new(move |b: &mut [u8]| {
                    raw::body_mut(b)[off..off + bytes.len()].copy_from_slice(&bytes)
                }),
            )
        }
        _ => return Ok(false),
    };
    let mut g = bp.fetch_write_for_redo(page)?;
    if raw::page_lsn(g.bytes()) < r.lsn {
        apply(g.bytes_mut());
        raw::set_page_lsn(g.bytes_mut(), r.lsn);
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn recover_aries(
    data: Arc<dyn BlockFile>,
    log_file: Arc<dyn BlockFile>,
    frames: usize,
) -> Result<RecoveryReport> {
    let (records, valid_end) = read_records_end(&*log_file)?;
    let by_lsn: HashMap<Lsn, usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.lsn, i))
        .collect();

    let mut att: HashMap<u64, Lsn> = HashMap::new();
    let mut dpt: HashMap<PageId, Lsn> = HashMap::new();
    let last_ckpt = records
        .iter()
        .rposition(|r| matches!(r.kind, Kind::Checkpoint { .. }));
    let start = match last_ckpt {
        Some(i) => {
            if let Kind::Checkpoint { att: a, dpt: d } = &records[i].kind {
                for (t, l) in a {
                    att.insert(*t, *l);
                }
                for (p, r) in d {
                    dpt.insert(*p, *r);
                }
            }
            i + 1
        }
        None => 0,
    };
    let mut committed = HashSet::new();
    for r in &records[start..] {
        match &r.kind {
            Kind::Begin => {
                att.insert(r.txn, r.lsn);
            }
            Kind::Commit => {
                att.remove(&r.txn);
                committed.insert(r.txn);
            }
            Kind::Abort => {
                att.remove(&r.txn);
            }
            Kind::PageInit { page, .. }
            | Kind::Update { page, .. }
            | Kind::FullPage { page, .. }
            | Kind::Clr { page, .. } => {
                att.insert(r.txn, r.lsn);
                dpt.entry(*page).or_insert(r.lsn);
            }
            Kind::Checkpoint { .. } => {}
        }
    }
    for r in &records {
        if matches!(r.kind, Kind::Commit) {
            committed.insert(r.txn);
        }
    }
    let losers: Vec<u64> = att.keys().copied().collect();

    let bp = BufferPool::open_default(data, frames)?;
    let redo_start = dpt.values().copied().min().unwrap_or(u64::MAX);
    let mut report = RecoveryReport {
        total_records: records.len(),
        committed_txns: committed.len(),
        losers: losers.len(),
        ..Default::default()
    };
    for r in &records {
        if r.lsn < redo_start {
            continue;
        }
        if redo_record(&bp, r)? {
            report.redone += 1;
        }
    }

    struct U {
        txn: u64,
        next: Lsn,
        prev: Lsn,
    }
    let mut work: Vec<U> = losers
        .iter()
        .map(|&t| U {
            txn: t,
            next: att[&t],
            prev: att[&t],
        })
        .collect();
    let mut log = Log::open_for_append(log_file.clone(), valid_end);
    loop {
        let mut best: Option<usize> = None;
        for (i, u) in work.iter().enumerate() {
            if u.next != NULL_LSN && best.is_none_or(|b| u.next > work[b].next) {
                best = Some(i);
            }
        }
        let Some(i) = best else { break };
        let rec = &records[by_lsn[&work[i].next]];
        match &rec.kind {
            Kind::Clr { undo_next, .. } => {
                work[i].next = *undo_next;
            }
            Kind::Update {
                page,
                offset,
                before,
                ..
            }
            | Kind::FullPage {
                page,
                offset,
                before,
                ..
            } => {
                let clr_lsn = log.append(
                    work[i].prev,
                    work[i].txn,
                    Kind::Clr {
                        page: *page,
                        offset: *offset,
                        bytes: before.clone(),
                        undo_next: rec.prev_lsn,
                    },
                );
                let mut g = bp.fetch_write_for_redo(*page)?;
                let off = *offset as usize;
                raw::body_mut(g.bytes_mut())[off..off + before.len()].copy_from_slice(before);
                raw::set_page_lsn(g.bytes_mut(), clr_lsn);
                drop(g);
                work[i].prev = clr_lsn;
                work[i].next = rec.prev_lsn;
                report.undone += 1;
            }
            Kind::PageInit { .. } => {
                work[i].next = rec.prev_lsn;
            }
            Kind::Begin => {
                log.append(work[i].prev, work[i].txn, Kind::Abort);
                work[i].next = NULL_LSN;
            }
            Kind::Commit | Kind::Abort | Kind::Checkpoint { .. } => {
                work[i].next = rec.prev_lsn;
            }
        }
    }
    let end = log.end_lsn();
    log.flush()?;
    log_file.set_len(end)?;

    bp.checkpoint()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_faultfs::{FaultConfig, FaultDisk};
    use keel_rng::Rng;

    const ACCOUNTS: usize = 400;
    const PER_PAGE: usize = 16;
    const INIT: i64 = 1000;

    fn page_of(acct: usize) -> PageId {
        (acct / PER_PAGE) as PageId
    }
    fn offset_of(acct: usize) -> usize {
        (acct % PER_PAGE) * 8
    }

    fn read_balance(store: &TxnStore, acct: usize) -> i64 {
        let b = store.read(page_of(acct), offset_of(acct), 8).unwrap();
        i64::from_le_bytes(b.try_into().unwrap())
    }

    fn read_all_balances(data: Arc<dyn BlockFile>) -> Vec<i64> {
        let bp = BufferPool::open_default(data, 32).unwrap();
        let mut out = vec![0i64; ACCOUNTS];
        for (a, bal) in out.iter_mut().enumerate() {
            let g = bp.fetch_read(page_of(a)).unwrap();
            let body = raw::body(g.bytes());
            let off = offset_of(a);
            *bal = i64::from_le_bytes(body[off..off + 8].try_into().unwrap());
        }
        out
    }

    fn setup_accounts(store: &TxnStore) {
        let npages = ACCOUNTS.div_ceil(PER_PAGE);
        for p in 0..npages {
            store.begin();
            let pid = store.create_page(PageType::Overflow).unwrap();
            assert_eq!(pid, p as PageId);
            for slot in 0..PER_PAGE {
                let a = p * PER_PAGE + slot;
                if a < ACCOUNTS {
                    store.write(pid, slot * 8, &INIT.to_le_bytes()).unwrap();
                }
            }
            store.commit().unwrap();
        }
    }

    #[test]
    fn recover_reproduces_committed_and_drops_uncommitted() {
        for seed in 0..24u64 {
            let data_disk = FaultDisk::new(FaultConfig::default(), seed);
            let log_disk = FaultDisk::new(FaultConfig::default(), seed ^ 0x5EED);
            let mut rng = Rng::seed(seed);
            let mut model = vec![INIT; ACCOUNTS];

            {
                let store = TxnStore::open(
                    Arc::new(data_disk.handle()),
                    Arc::new(log_disk.handle()),
                    16,
                )
                .unwrap();
                setup_accounts(&store);

                for _ in 0..300 {
                    let a = rng.below(ACCOUNTS as u64) as usize;
                    let b = rng.below(ACCOUNTS as u64) as usize;
                    if a == b {
                        continue;
                    }
                    let amt = rng.range(1, 50) as i64;
                    store.begin();
                    let ba = read_balance(&store, a);
                    let bb = read_balance(&store, b);
                    if ba >= amt {
                        store
                            .write(page_of(a), offset_of(a), &(ba - amt).to_le_bytes())
                            .unwrap();
                        store
                            .write(page_of(b), offset_of(b), &(bb + amt).to_le_bytes())
                            .unwrap();
                        store.commit().unwrap();
                        model[a] -= amt;
                        model[b] += amt;
                    } else {
                        store.abort();
                    }
                }

                let a = rng.below(ACCOUNTS as u64) as usize;
                let b = (a + 1) % ACCOUNTS;
                store.begin();
                store
                    .write(page_of(a), offset_of(a), &(0i64).to_le_bytes())
                    .unwrap();
                store
                    .write(page_of(b), offset_of(b), &(999999i64).to_le_bytes())
                    .unwrap();
            }

            data_disk.crash();
            log_disk.crash();
            let report = recover(
                Arc::new(data_disk.handle()),
                Arc::new(log_disk.handle()),
                16,
            )
            .unwrap();
            assert!(
                report.committed_txns >= 1,
                "seed {seed}: no committed txns recovered?"
            );

            let balances = read_all_balances(Arc::new(data_disk.handle()));
            assert_eq!(
                balances, model,
                "seed {seed}: recovered state != committed model"
            );
            let total: i64 = balances.iter().sum();
            assert_eq!(
                total,
                INIT * ACCOUNTS as i64,
                "seed {seed}: money not conserved"
            );
        }
    }

    #[test]
    fn recovery_rebuilds_from_log_alone() {
        let log_disk = FaultDisk::new(FaultConfig::benign(), 1);
        let mut model = vec![INIT; ACCOUNTS];
        let data_disk = FaultDisk::new(FaultConfig::benign(), 2);
        {
            let store = TxnStore::open(
                Arc::new(data_disk.handle()),
                Arc::new(log_disk.handle()),
                16,
            )
            .unwrap();
            setup_accounts(&store);
            let mut rng = Rng::seed(7);
            for _ in 0..200 {
                let a = rng.below(ACCOUNTS as u64) as usize;
                let b = rng.below(ACCOUNTS as u64) as usize;
                if a == b {
                    continue;
                }
                let amt = rng.range(1, 30) as i64;
                store.begin();
                let ba = read_balance(&store, a);
                let bb = read_balance(&store, b);
                if ba >= amt {
                    store
                        .write(page_of(a), offset_of(a), &(ba - amt).to_le_bytes())
                        .unwrap();
                    store
                        .write(page_of(b), offset_of(b), &(bb + amt).to_le_bytes())
                        .unwrap();
                    store.commit().unwrap();
                    model[a] -= amt;
                    model[b] += amt;
                }
            }
        }
        let fresh_data = FaultDisk::new(FaultConfig::benign(), 99);
        recover(
            Arc::new(fresh_data.handle()),
            Arc::new(log_disk.handle()),
            16,
        )
        .unwrap();
        let balances = read_all_balances(Arc::new(fresh_data.handle()));
        assert_eq!(
            balances, model,
            "log-only recovery must reproduce committed state"
        );
    }

    #[test]
    fn rung2_survives_torn_writes() {
        let vicious = FaultConfig::default();
        let mut total_fpi = 0u64;
        for seed in 0..24u64 {
            let data_disk = FaultDisk::new(vicious, seed);
            let log_disk = FaultDisk::new(vicious, seed ^ 0x5EED);
            let mut rng = Rng::seed(seed);
            let mut model = vec![INIT; ACCOUNTS];

            {
                let store = TxnStore::open_with(
                    Arc::new(data_disk.handle()),
                    Arc::new(log_disk.handle()),
                    16,
                    Policy::rung2(),
                )
                .unwrap();
                setup_accounts(&store);
                for _ in 0..300 {
                    let a = rng.below(ACCOUNTS as u64) as usize;
                    let b = rng.below(ACCOUNTS as u64) as usize;
                    if a == b {
                        continue;
                    }
                    let amt = rng.range(1, 50) as i64;
                    store.begin();
                    let ba = read_balance(&store, a);
                    let bb = read_balance(&store, b);
                    if ba >= amt {
                        store
                            .write(page_of(a), offset_of(a), &(ba - amt).to_le_bytes())
                            .unwrap();
                        store
                            .write(page_of(b), offset_of(b), &(bb + amt).to_le_bytes())
                            .unwrap();
                        store.commit().unwrap();
                        model[a] -= amt;
                        model[b] += amt;
                    } else {
                        store.abort();
                    }
                }
                let a = rng.below(ACCOUNTS as u64) as usize;
                let b = (a + 1) % ACCOUNTS;
                store.begin();
                store
                    .write(page_of(a), offset_of(a), &(0i64).to_le_bytes())
                    .unwrap();
                store
                    .write(page_of(b), offset_of(b), &(123456i64).to_le_bytes())
                    .unwrap();
                total_fpi += store.stats().full_page_records;
                assert!(
                    store.stats().full_page_records > 0,
                    "FPW should have logged full-page images"
                );
            }

            data_disk.crash();
            log_disk.crash();
            recover(
                Arc::new(data_disk.handle()),
                Arc::new(log_disk.handle()),
                16,
            )
            .unwrap();

            let balances = read_all_balances(Arc::new(data_disk.handle()));
            assert_eq!(
                balances, model,
                "seed {seed}: rung-2 recovery != committed model"
            );
            let total: i64 = balances.iter().sum();
            assert_eq!(
                total,
                INIT * ACCOUNTS as i64,
                "seed {seed}: money not conserved"
            );
        }
        assert!(
            total_fpi > 0,
            "the campaign should have exercised full-page images"
        );
    }

    #[test]
    fn abort_undoes_via_clrs() {
        let data = FaultDisk::new(FaultConfig::benign(), 1);
        let log = FaultDisk::new(FaultConfig::benign(), 2);
        let store = TxnStore::open_with(
            Arc::new(data.handle()),
            Arc::new(log.handle()),
            16,
            Policy::rung2(),
        )
        .unwrap();
        setup_accounts(&store);
        let a0 = read_balance(&store, 5);
        let b0 = read_balance(&store, 250);

        store.begin();
        store
            .write(page_of(5), offset_of(5), &(a0 - 100).to_le_bytes())
            .unwrap();
        store
            .write(page_of(250), offset_of(250), &(b0 + 100).to_le_bytes())
            .unwrap();
        assert_eq!(
            read_balance(&store, 5),
            a0 - 100,
            "own writes visible mid-txn"
        );
        store.abort();

        assert!(store.stats().clrs >= 2, "abort should have written CLRs");
        assert_eq!(
            read_balance(&store, 5),
            a0,
            "abort must reinstate the before-image"
        );
        assert_eq!(
            read_balance(&store, 250),
            b0,
            "abort must reinstate the before-image"
        );
    }

    #[test]
    fn rung3_aries_full_recovery() {
        let mut saw_redo = false;
        let mut saw_undo = false;
        for seed in 0..20u64 {
            let data = FaultDisk::new(FaultConfig::default(), seed);
            let log = FaultDisk::new(FaultConfig::default(), seed ^ 0xA11CE);
            let mut rng = Rng::seed(seed);
            let mut model = vec![INIT; ACCOUNTS];

            {
                let store = TxnStore::open_with(
                    Arc::new(data.handle()),
                    Arc::new(log.handle()),
                    12,
                    Policy::rung2(),
                )
                .unwrap();
                setup_accounts(&store);
                store.checkpoint().unwrap();

                let transfer = |store: &TxnStore, model: &mut Vec<i64>, rng: &mut Rng| {
                    let a = rng.below(ACCOUNTS as u64) as usize;
                    let b = rng.below(ACCOUNTS as u64) as usize;
                    if a == b {
                        return;
                    }
                    let amt = rng.range(1, 50) as i64;
                    store.begin();
                    let ba = read_balance(store, a);
                    let bb = read_balance(store, b);
                    if ba >= amt {
                        store
                            .write(page_of(a), offset_of(a), &(ba - amt).to_le_bytes())
                            .unwrap();
                        store
                            .write(page_of(b), offset_of(b), &(bb + amt).to_le_bytes())
                            .unwrap();
                        store.commit().unwrap();
                        model[a] -= amt;
                        model[b] += amt;
                    } else {
                        store.abort();
                    }
                };

                for i in 0..250 {
                    transfer(&store, &mut model, &mut rng);
                    if i % 70 == 0 {
                        store.checkpoint().unwrap();
                    }
                }
                for _ in 0..50 {
                    transfer(&store, &mut model, &mut rng);
                }
                store.begin();
                for k in 0..20u64 {
                    let acct = ((k * 17) % ACCOUNTS as u64) as usize;
                    store
                        .write(page_of(acct), offset_of(acct), &(-99i64).to_le_bytes())
                        .unwrap();
                }
            }

            data.crash();
            log.crash();

            for pass in 0..3 {
                let r = recover_aries(Arc::new(data.handle()), Arc::new(log.handle()), 12).unwrap();
                if pass == 0 {
                    if r.redone > 0 {
                        saw_redo = true;
                    }
                    if r.undone > 0 {
                        saw_undo = true;
                    }
                }
                data.crash();
                log.crash();
            }

            let balances = read_all_balances(Arc::new(data.handle()));
            assert_eq!(
                balances, model,
                "seed {seed}: ARIES recovery != committed model"
            );
            let total: i64 = balances.iter().sum();
            assert_eq!(
                total,
                INIT * ACCOUNTS as i64,
                "seed {seed}: money not conserved"
            );
        }
        assert!(saw_redo, "the campaign should have exercised redo");
        assert!(
            saw_undo,
            "the campaign should have exercised undo of a durable loser"
        );
    }
}
