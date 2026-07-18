//! `dbcheck` — the offline validator of every invariant the engine claims (D12).
//!
//! It is the crash campaign's referee: after recovery, `dbcheck` decides whether
//! the database is actually well-formed. The rule (§7.5) is that every invariant
//! the system relies on gets a `dbcheck` rule the same week it's introduced, so
//! the tool grows monotonically with the engine. The P1 rule set:
//!
//!   * **Checksums** — every page's stored CRC matches its body (a mismatch is a
//!     torn or rotted page).
//!   * **Page structure** — header fields are self-consistent and no slot points
//!     outside the tuple heap.
//!   * **Heap record tags** — every record is a valid Tuple / Forward /
//!     ForwardTarget.
//!   * **Forward integrity** — every stub points at a real ForwardTarget, and
//!     every ForwardTarget is pointed at by exactly one stub (no dangling
//!     forwards, no orphaned or doubly-referenced targets).
//!
//! Later phases add B-tree order/balance/sibling rules and MVCC version-chain
//! rules here, same-week as those subsystems land.

use std::collections::HashMap;
use std::io;

use keel_heap::{classify_record, RecordKind, Rid};
use keel_page::{PageType, SlottedPage, PAGE_SIZE};
use keel_vfs::BlockFile;

/// A single invariant violation found by `dbcheck`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    BadChecksum {
        page: u32,
    },
    BadStructure {
        page: u32,
        why: String,
    },
    BadRecordTag {
        rid: Rid,
        tag: u8,
    },
    /// A forward stub points at a slot that isn't a live ForwardTarget.
    DanglingForward {
        stub: Rid,
        target: Rid,
    },
    /// A ForwardTarget that no stub points to (leaked space / lost tuple).
    OrphanTarget {
        target: Rid,
    },
    /// A ForwardTarget referenced by more than one stub.
    DoublyReferencedTarget {
        target: Rid,
        stubs: Vec<Rid>,
    },
    /// The file length is not a whole number of pages.
    RaggedFile {
        bytes: u64,
    },
}

/// The result of a check run: counts plus any violations.
#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    pub pages: u32,
    pub heap_pages: u32,
    pub tuples: u64,
    pub stubs: u64,
    pub targets: u64,
    pub violations: Vec<Violation>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Validate an entire data file. Never trusts a page whose checksum fails — such
/// a page is reported and skipped for structural checks.
pub fn check_file(file: &dyn BlockFile) -> io::Result<CheckReport> {
    let size = file.size()?;
    let mut report = CheckReport::default();
    if size % PAGE_SIZE as u64 != 0 {
        report
            .violations
            .push(Violation::RaggedFile { bytes: size });
    }
    let npages = (size / PAGE_SIZE as u64) as u32;
    report.pages = npages;

    let mut stub_targets: Vec<(Rid, Rid)> = Vec::new();
    let mut targets: HashMap<Rid, u32> = HashMap::new();

    let mut buf = vec![0u8; PAGE_SIZE];
    for pid in 0..npages {
        file.read_at(&mut buf, pid as u64 * PAGE_SIZE as u64)?;
        let sp = SlottedPage::from_bytes(&buf[..]);
        if !sp.verify_checksum() {
            report.violations.push(Violation::BadChecksum { page: pid });
            continue;
        }
        if sp.page_type() != Some(PageType::Heap) {
            continue;
        }
        if let Err(e) = sp.validate_structure() {
            report.violations.push(Violation::BadStructure {
                page: pid,
                why: e.to_string(),
            });
            continue;
        }
        report.heap_pages += 1;
        for (slot, bytes) in sp.iter() {
            let rid = Rid::new(pid, slot);
            match classify_record(bytes) {
                Ok(RecordKind::Tuple) => report.tuples += 1,
                Ok(RecordKind::Forward(target)) => {
                    report.stubs += 1;
                    stub_targets.push((rid, target));
                }
                Ok(RecordKind::ForwardTarget) => {
                    report.targets += 1;
                    targets.entry(rid).or_insert(0);
                }
                Err(tag) => report.violations.push(Violation::BadRecordTag { rid, tag }),
            }
        }
    }

    let mut refs: HashMap<Rid, Vec<Rid>> = HashMap::new();
    for (stub, target) in &stub_targets {
        if !targets.contains_key(target) {
            report.violations.push(Violation::DanglingForward {
                stub: *stub,
                target: *target,
            });
        } else {
            refs.entry(*target).or_default().push(*stub);
        }
    }
    for target in targets.keys() {
        match refs.get(target) {
            None => report
                .violations
                .push(Violation::OrphanTarget { target: *target }),
            Some(stubs) if stubs.len() > 1 => {
                report.violations.push(Violation::DoublyReferencedTarget {
                    target: *target,
                    stubs: stubs.clone(),
                });
            }
            Some(_) => {}
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_buffer::BufferPool;
    use keel_heap::HeapFile;
    use keel_vfs::MemDisk;
    use std::sync::Arc;

    #[test]
    fn clean_heap_passes() {
        let disk = Arc::new(MemDisk::new());
        {
            let bp = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, 16).unwrap();
            let h = HeapFile::open(&bp).unwrap();
            for i in 0..500 {
                h.insert(format!("row-{i}").as_bytes()).unwrap();
            }
            let mut rids = Vec::new();
            for i in 0..300 {
                rids.push(
                    h.insert(format!("f-{i}-xxxxxxxxxxxxxxxxxxxxxxxx").as_bytes())
                        .unwrap(),
                );
            }
            for r in rids.iter().step_by(5) {
                h.update(*r, &vec![b'Z'; 4000]).unwrap();
            }
            bp.checkpoint().unwrap();
        }
        let report = check_file(&*(disk.clone() as Arc<dyn BlockFile>)).unwrap();
        assert!(
            report.ok(),
            "clean heap should pass: {:?}",
            report.violations
        );
        assert!(report.tuples > 0);
        assert!(report.stubs > 0);
        assert_eq!(report.stubs, report.targets, "one target per stub");
    }

    #[test]
    fn detects_bad_checksum() {
        let disk = Arc::new(MemDisk::new());
        {
            let bp = BufferPool::open_default(disk.clone() as Arc<dyn BlockFile>, 4).unwrap();
            let h = HeapFile::open(&bp).unwrap();
            h.insert(b"corrupt-me").unwrap();
            bp.checkpoint().unwrap();
        }
        let mut image = disk.snapshot();
        image[PAGE_SIZE / 2] ^= 0xFF;
        disk.install(image);
        let report = check_file(&*(disk.clone() as Arc<dyn BlockFile>)).unwrap();
        assert!(report
            .violations
            .iter()
            .any(|v| matches!(v, Violation::BadChecksum { page: 0 })));
    }
}
