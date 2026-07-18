use std::sync::Arc;

use keel_buffer::BufferPool;
use keel_dbcheck::{check_file, Violation};
use keel_faultfs::{FaultConfig, FaultDisk};
use keel_heap::HeapFile;
use keel_rng::Rng;
use keel_vfs::BlockFile;

#[test]
fn benign_crash_preserves_checkpointed_data() {
    for seed in 0..16u64 {
        let disk = FaultDisk::new(FaultConfig::benign(), seed);
        let file: Arc<dyn BlockFile> = Arc::new(disk.handle());

        let mut committed: Vec<(keel_heap::Rid, Vec<u8>)> = Vec::new();
        {
            let bp = BufferPool::open_default(file.clone(), 8).unwrap();
            let heap = HeapFile::open(&bp).unwrap();
            for i in 0..400 {
                let rec = format!("committed-{seed}-{i:04}").into_bytes();
                let rid = heap.insert(&rec).unwrap();
                committed.push((rid, rec));
            }
            bp.checkpoint().unwrap();
        }
        {
            let bp = BufferPool::open_default(file.clone(), 8).unwrap();
            let heap = HeapFile::open(&bp).unwrap();
            for i in 0..400 {
                let _ = heap.insert(format!("volatile-{i}").as_bytes()).unwrap();
            }
        }
        disk.crash();

        let bp = BufferPool::open_default(file.clone(), 8).unwrap();
        let heap = HeapFile::open(&bp).unwrap();
        for (rid, rec) in &committed {
            assert_eq!(
                heap.get(*rid).unwrap().as_deref(),
                Some(rec.as_slice()),
                "seed {seed}: committed row lost after benign crash"
            );
        }
        assert!(
            check_file(&*file).unwrap().ok(),
            "seed {seed}: dbcheck failed after benign crash"
        );
    }
}

#[test]
fn vicious_crashes_are_always_detected() {
    let mut torn_pages_seen = 0u64;
    for seed in 0..200u64 {
        let disk = FaultDisk::new(FaultConfig::default(), seed);
        let file: Arc<dyn BlockFile> = Arc::new(disk.handle());
        let mut rng = Rng::seed(seed ^ 0xA5A5);

        {
            let bp = BufferPool::open_default(file.clone(), 6).unwrap();
            let heap = HeapFile::open(&bp).unwrap();
            let mut rids = Vec::new();
            let pad = "x".repeat(180);
            for i in 0..400 {
                rids.push(
                    heap.insert(format!("r-{seed}-{i:04}-{pad}").as_bytes())
                        .unwrap(),
                );
            }
            bp.checkpoint().unwrap();
            for i in 400..1000 {
                match rng.below(3) {
                    0 if !rids.is_empty() => {
                        let k = rng.below(rids.len() as u64) as usize;
                        let _ = heap.update(rids[k], &vec![b'U'; rng.range(8, 300) as usize]);
                    }
                    1 if !rids.is_empty() => {
                        let k = rng.below(rids.len() as u64) as usize;
                        let _ = heap.delete(rids[k]);
                    }
                    _ => {
                        if let Ok(rid) = heap.insert(format!("r-{seed}-{i:04}-{pad}").as_bytes()) {
                            rids.push(rid);
                        }
                    }
                }
            }
            bp.flush_all().unwrap();
        }
        disk.crash();

        let report = check_file(&*file).unwrap();

        for v in &report.violations {
            match v {
                Violation::BadStructure { page, why } => {
                    panic!("seed {seed}: torn page {page} slipped past the checksum: {why}")
                }
                Violation::BadRecordTag { rid, tag } => {
                    panic!("seed {seed}: garbage record accepted at {rid:?} tag {tag}")
                }
                _ => {}
            }
        }

        let has_checksum = report
            .violations
            .iter()
            .any(|v| matches!(v, Violation::BadChecksum { .. }));
        let has_downstream = report.violations.iter().any(|v| {
            matches!(
                v,
                Violation::DanglingForward { .. }
                    | Violation::OrphanTarget { .. }
                    | Violation::DoublyReferencedTarget { .. }
            )
        });
        assert!(
            !has_downstream || has_checksum,
            "seed {seed}: forward inconsistency with no torn page to explain it: {:?}",
            report.violations
        );
        if has_checksum {
            torn_pages_seen += 1;
        }
    }
    assert!(
        torn_pages_seen > 0,
        "expected some torn pages across the campaign"
    );
    eprintln!("vicious campaign: {torn_pages_seen}/200 crashes tore at least one page");
}
