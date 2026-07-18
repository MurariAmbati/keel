use std::io::{self, ErrorKind};
use std::sync::{Arc, Mutex};

use keel_rng::Rng;
use keel_vfs::BlockFile;

pub const SECTOR: usize = 512;

#[derive(Clone, Copy, Debug)]
pub struct FaultConfig {
    pub p_drop: f64,
    pub p_tear: f64,
    pub p_sector_survives: f64,
    pub reorder: bool,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrashReport {
    pub pending_ops: usize,
    pub fully_applied: usize,
    pub torn: usize,
    pub dropped: usize,
    pub torn_sectors_kept: usize,
    pub torn_sectors_lost: usize,
}

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
    durable: Vec<u8>,
    live: Vec<u8>,
    pending: Vec<WriteOp>,
    cfg: FaultConfig,
    rng: Rng,
    counters: Counters,
}

#[derive(Clone)]
pub struct FaultDisk {
    st: Arc<Mutex<State>>,
}

impl FaultDisk {
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

    pub fn handle(&self) -> FaultFile {
        FaultFile {
            st: self.st.clone(),
        }
    }

    pub fn counters(&self) -> Counters {
        self.st.lock().unwrap().counters
    }

    pub fn pending_writes(&self) -> usize {
        self.st.lock().unwrap().pending.len()
    }

    pub fn durable_image(&self) -> Vec<u8> {
        self.st.lock().unwrap().durable.clone()
    }

    pub fn live_image(&self) -> Vec<u8> {
        self.st.lock().unwrap().live.clone()
    }

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
