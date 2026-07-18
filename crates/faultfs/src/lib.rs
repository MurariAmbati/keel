//! The fault-injecting file layer — the crash campaign's adversary (§7.3).
//!
//! `FaultDisk` implements [`keel_vfs::BlockFile`], so from the engine's side it
//! is indistinguishable from a real disk. Underneath it models the *honest*
//! disk of the ALICE work (Pillai et al., OSDI'14):
//!
//!   * a write becomes durable only after a following `sync`;
//!   * between two syncs, un-synced writes may be reordered arbitrarily;
//!   * across a sync there is no reordering (durability is a barrier);
//!   * on a crash, an un-synced write may land fully, partially (torn at
//!     512-byte sector boundaries), or not at all.
//!
//! Every fault decision is drawn from a seeded [`keel_rng::Rng`], so a failure
//! is fully described by `(disk seed, crash schedule)` and replays byte-for-byte
//! — the deterministic-simulation ethos the whole campaign rests on.
//!
//! The *timing* of a crash (after which write, at which fsync boundary, and how
//! deep into recovery) is the harness's job: it drives the workload and calls
//! [`FaultDisk::crash`] at the scheduled point. This layer owns only the disk
//! semantics, never the control flow.

use std::io::{self, ErrorKind};
use std::sync::{Arc, Mutex};

use keel_rng::Rng;
use keel_vfs::BlockFile;

/// Disk sector size. A torn write persists a subset of the sectors it touched.
pub const SECTOR: usize = 512;

/// Tunables for the disk's failure behavior. Probabilities apply *per un-synced
/// write* at crash time.
#[derive(Clone, Copy, Debug)]
pub struct FaultConfig {
    /// P(an un-synced write is entirely lost on crash).
    pub p_drop: f64,
    /// P(an un-synced write is torn rather than applied whole).
    pub p_tear: f64,
    /// Within a torn write, P(each touched sector survives).
    pub p_sector_survives: f64,
    /// Whether un-synced writes may be reordered before the crash resolves them.
    pub reorder: bool,
    /// Sector size for tearing.
    pub sector: usize,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            p_drop: 0.30,
            p_tear: 0.40,
            p_sector_survives: 0.5,
            reorder: true,
            sector: SECTOR,
        }
    }
}

impl FaultConfig {
    /// A benign config: writes are never dropped or torn. Useful to isolate a
    /// bug to "recovery logic" vs "the injector".
    pub fn benign() -> Self {
        Self {
            p_drop: 0.0,
            p_tear: 0.0,
            p_sector_survives: 1.0,
            reorder: false,
            sector: SECTOR,
        }
    }
}

#[derive(Clone)]
struct WriteOp {
    offset: u64,
    data: Vec<u8>,
}

/// What a single crash did — folded into the campaign's stats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrashReport {
    pub pending_ops: usize,
    pub fully_applied: usize,
    pub torn: usize,
    pub dropped: usize,
    /// Sectors that survived within torn writes.
    pub torn_sectors_kept: usize,
    /// Sectors that vanished within torn writes.
    pub torn_sectors_lost: usize,
}

/// Cumulative counters — every stat before every explanation (house law).
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub reads: u64,
    pub writes: u64,
    pub bytes_written: u64,
    pub syncs: u64,
    pub crashes: u64,
    pub writes_since_sync: u64,
}

struct State {
    /// Bytes as of the last successful `sync` — guaranteed to survive a crash.
    durable: Vec<u8>,
    /// What the running process sees: durable plus every write since.
    live: Vec<u8>,
    /// Un-synced writes, in issue order — the reorder/tear candidates.
    pending: Vec<WriteOp>,
    cfg: FaultConfig,
    rng: Rng,
    counters: Counters,
}

/// A fault-injecting durable medium. Clone handles share the same disk (so it
/// models "the disk survives, the process does not").
#[derive(Clone)]
pub struct FaultDisk {
    st: Arc<Mutex<State>>,
}

impl FaultDisk {
    /// Fresh empty disk with the given fault config and seed.
    pub fn new(cfg: FaultConfig, seed: u64) -> Self {
        Self {
            st: Arc::new(Mutex::new(State {
                durable: Vec::new(),
                live: Vec::new(),
                pending: Vec::new(),
                cfg,
                rng: Rng::seed(seed),
                counters: Counters::default(),
            })),
        }
    }

    /// Seed a disk from an existing durable image (e.g. after an external crash
    /// or to resume a campaign from a saved image).
    pub fn from_image(cfg: FaultConfig, seed: u64, image: Vec<u8>) -> Self {
        Self {
            st: Arc::new(Mutex::new(State {
                durable: image.clone(),
                live: image,
                pending: Vec::new(),
                cfg,
                rng: Rng::seed(seed),
                counters: Counters::default(),
            })),
        }
    }

    /// A `BlockFile` handle onto this disk. Multiple handles share one disk.
    pub fn handle(&self) -> FaultFile {
        FaultFile {
            st: self.st.clone(),
        }
    }

    pub fn counters(&self) -> Counters {
        self.st.lock().unwrap().counters
    }

    /// Number of un-synced writes currently in flight.
    pub fn pending_writes(&self) -> usize {
        self.st.lock().unwrap().pending.len()
    }

    /// A copy of the durable (last-synced) image.
    pub fn durable_image(&self) -> Vec<u8> {
        self.st.lock().unwrap().durable.clone()
    }

    /// A copy of the live image (what the process currently sees).
    pub fn live_image(&self) -> Vec<u8> {
        self.st.lock().unwrap().live.clone()
    }

    /// Simulate power loss **now**. Un-synced writes are resolved per the fault
    /// config (drop / tear / apply, possibly reordered); the result becomes the
    /// new durable image. Volatile state is cleared, as if the process died and
    /// a fresh one will re-open. Returns what happened, for the stats log.
    pub fn crash(&self) -> CrashReport {
        let mut st = self.st.lock().unwrap();
        st.counters.crashes += 1;

        let cfg = st.cfg;
        let mut image = st.durable.clone();
        let mut pending = std::mem::take(&mut st.pending);
        if cfg.reorder {
            st.rng.shuffle(&mut pending);
        }

        let mut report = CrashReport {
            pending_ops: pending.len(),
            ..Default::default()
        };

        for op in &pending {
            if st.rng.chance(cfg.p_drop) {
                report.dropped += 1;
                continue;
            }
            if st.rng.chance(cfg.p_tear) {
                report.torn += 1;
                apply_torn(
                    &mut image,
                    op,
                    cfg.sector,
                    cfg.p_sector_survives,
                    &mut st.rng,
                    &mut report,
                );
            } else {
                report.fully_applied += 1;
                apply_whole(&mut image, op);
            }
        }

        st.durable = image.clone();
        st.live = image;
        st.counters.writes_since_sync = 0;
        report
    }
}

fn ensure_len(image: &mut Vec<u8>, end: usize) {
    if image.len() < end {
        image.resize(end, 0);
    }
}

fn apply_whole(image: &mut Vec<u8>, op: &WriteOp) {
    let start = op.offset as usize;
    let end = start + op.data.len();
    ensure_len(image, end);
    image[start..end].copy_from_slice(&op.data);
}

fn apply_torn(
    image: &mut Vec<u8>,
    op: &WriteOp,
    sector: usize,
    p_keep: f64,
    rng: &mut Rng,
    report: &mut CrashReport,
) {
    let start = op.offset as usize;
    let end = start + op.data.len();
    let first_sector = start / sector;
    let last_sector = (end - 1) / sector;
    for s in first_sector..=last_sector {
        let sec_start = s * sector;
        let sec_end = sec_start + sector;
        let lo = start.max(sec_start);
        let hi = end.min(sec_end);
        if rng.chance(p_keep) {
            ensure_len(image, hi);
            image[lo..hi].copy_from_slice(&op.data[lo - start..hi - start]);
            report.torn_sectors_kept += 1;
        } else {
            report.torn_sectors_lost += 1;
        }
    }
}

/// A `BlockFile` view onto a [`FaultDisk`].
pub struct FaultFile {
    st: Arc<Mutex<State>>,
}

impl BlockFile for FaultFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.counters.reads += 1;
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "offset overflow"))?;
        if end > st.live.len() {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "read past EOF"));
        }
        buf.copy_from_slice(&st.live[start..end]);
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.counters.writes += 1;
        st.counters.writes_since_sync += 1;
        st.counters.bytes_written += buf.len() as u64;
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "offset overflow"))?;
        ensure_len(&mut st.live, end);
        st.live[start..end].copy_from_slice(buf);
        st.pending.push(WriteOp {
            offset,
            data: buf.to_vec(),
        });
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.counters.syncs += 1;
        st.counters.writes_since_sync = 0;
        st.durable = st.live.clone();
        st.pending.clear();
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.st.lock().unwrap().live.len() as u64)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.live.resize(len as usize, 0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_writes_survive_crash() {
        let disk = FaultDisk::new(FaultConfig::default(), 1);
        let f = disk.handle();
        f.write_at(b"committed-data", 0).unwrap();
        f.sync().unwrap();
        let report = disk.crash();
        assert_eq!(report.pending_ops, 0);
        let mut b = [0u8; 14];
        disk.handle().read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"committed-data");
    }

    #[test]
    fn unsynced_writes_can_vanish() {
        let cfg = FaultConfig {
            p_drop: 1.0,
            p_tear: 0.0,
            ..FaultConfig::default()
        };
        let disk = FaultDisk::new(cfg, 2);
        let f = disk.handle();
        f.write_at(b"durable", 0).unwrap();
        f.sync().unwrap();
        f.write_at(b"VOLATILE", 0).unwrap();
        let report = disk.crash();
        assert_eq!(report.dropped, 1);
        let mut b = [0u8; 7];
        disk.handle().read_at(&mut b, 0).unwrap();
        assert_eq!(
            &b, b"durable",
            "the un-synced overwrite must have been lost"
        );
    }

    #[test]
    fn benign_config_never_loses_unsynced() {
        let disk = FaultDisk::new(FaultConfig::benign(), 3);
        let f = disk.handle();
        f.write_at(b"AAAA", 0).unwrap();
        f.write_at(b"BBBB", 4).unwrap();
        let report = disk.crash();
        assert_eq!(report.fully_applied, 2);
        assert_eq!(report.dropped, 0);
        assert_eq!(report.torn, 0);
        let mut b = [0u8; 8];
        disk.handle().read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"AAAABBBB");
    }

    #[test]
    fn tearing_splits_at_sector_boundaries() {
        let mut kept_seen = false;
        let mut lost_seen = false;
        for seed in 0..64 {
            let cfg = FaultConfig {
                p_drop: 0.0,
                p_tear: 1.0,
                p_sector_survives: 0.5,
                reorder: false,
                sector: SECTOR,
            };
            let disk = FaultDisk::new(cfg, seed);
            let f = disk.handle();
            let payload = vec![0xABu8; 2 * SECTOR];
            f.write_at(&payload, 0).unwrap();
            let report = disk.crash();
            assert_eq!(report.torn, 1);
            if report.torn_sectors_kept > 0 {
                kept_seen = true;
            }
            if report.torn_sectors_lost > 0 {
                lost_seen = true;
            }
        }
        assert!(
            kept_seen && lost_seen,
            "tearing should both keep and lose sectors across seeds"
        );
    }

    #[test]
    fn reproducible_from_seed() {
        let run = |seed: u64| {
            let disk = FaultDisk::new(FaultConfig::default(), seed);
            let f = disk.handle();
            f.write_at(b"one", 0).unwrap();
            f.sync().unwrap();
            f.write_at(b"twotwotwo", 3).unwrap();
            f.write_at(b"three", 12).unwrap();
            disk.crash();
            disk.durable_image()
        };
        assert_eq!(run(0xBEEF), run(0xBEEF));
    }

    #[test]
    fn repeated_crashes_stay_consistent() {
        let disk = FaultDisk::new(FaultConfig::default(), 7);
        let f = disk.handle();
        f.write_at(b"base", 0).unwrap();
        f.sync().unwrap();
        let base = disk.durable_image();
        f.write_at(b"xxxx", 0).unwrap();
        disk.crash();
        assert!(disk.durable_image().len() >= base.len());
        let before = disk.durable_image();
        let report = disk.crash();
        assert_eq!(report.pending_ops, 0);
        assert_eq!(disk.durable_image(), before);
    }
}
