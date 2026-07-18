use keel_cbuffer::{PageCache, PageFormat, WalSync as CWalSync};
use keel_page::PageType;
use keel_pager::RecoveryPager;
use keel_vfs::{BlockFile, MemDisk};
use keel_wal::{Log, Policy, TxnStore};
use std::sync::{Arc, Mutex};

const FRAMES: usize = 8;
const PAGES: usize = 6;
const WRITES: usize = 40;

struct LogGate {
    log: Arc<Mutex<Log>>,
}
impl CWalSync for LogGate {
    fn flushed_lsn(&self) -> u64 {
        self.log.lock().unwrap().durable_lsn()
    }
    fn flush_until(&self, lsn: u64) -> std::io::Result<()> {
        self.log.lock().unwrap().flush_until(lsn)
    }
}

fn exercise<P: RecoveryPager>(store: &TxnStore<P>) -> (Vec<Vec<u8>>, u64) {
    let mut pids = Vec::new();
    store.begin();
    for _ in 0..PAGES {
        pids.push(store.create_page(PageType::Heap).expect("create_page"));
    }
    store.commit().expect("commit");

    store.begin();
    for i in 0..WRITES {
        let pid = pids[i % pids.len()];
        let payload = format!("w{i:03}").into_bytes();
        store.write(pid, (i % 20) * 8, &payload).expect("write");
    }
    store.commit().expect("commit 2");

    store.begin();
    for &pid in &pids {
        store.write(pid, 200, b"ROLLBACKME").expect("write abort");
    }
    store.abort();

    store.checkpoint().expect("checkpoint");

    let pages: Vec<Vec<u8>> = pids
        .iter()
        .map(|&p| store.read(p, 0, 256).expect("read"))
        .collect();
    (pages, store.stats().update_records)
}

#[test]
fn the_txn_store_agrees_on_both_pools() {
    let disk_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let log_a = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let via_buffer = {
        let store = TxnStore::open_with(disk_a, log_a, FRAMES, Policy::rung1()).expect("open");
        exercise(&store)
    };

    let disk_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let log_b = Arc::new(MemDisk::new()) as Arc<dyn BlockFile>;
    let via_cache = {
        let log = Arc::new(Mutex::new(Log::create(log_b)));
        let gate = Arc::new(LogGate { log: log.clone() });
        let cache = PageCache::open_formatted(disk_b, FRAMES, gate, PageFormat::keel_page());
        let store = TxnStore::with_pager(cache, log, Policy::rung1());
        exercise(&store)
    };

    assert_eq!(
        via_buffer.0, via_cache.0,
        "page contents differ between the two pools after commit + abort + checkpoint"
    );
    assert_eq!(
        via_buffer.1, via_cache.1,
        "update-record counts differ between the two pools"
    );
    assert!(
        via_buffer.1 >= WRITES as u64,
        "expected at least {WRITES} update records, got {}",
        via_buffer.1
    );
    for page in &via_cache.0 {
        assert!(
            !page.windows(10).any(|w| w == b"ROLLBACKME"),
            "an aborted write survived on the concurrent cache"
        );
    }
}
