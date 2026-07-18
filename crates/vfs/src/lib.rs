//! The virtual file layer — KEEL's single door to durable storage (D11).
//!
//! Every byte the engine reads or writes goes through [`BlockFile`]. That is not
//! tidiness for its own sake: the crash campaign (§7.3) is *only* possible
//! because the fault injector is just another `BlockFile` implementation
//! (`keel-faultfs`). If any subsystem reaches around this trait to `std::fs`,
//! the injector can't see that I/O and the campaign silently under-tests. So the
//! house law is absolute: **all I/O through `vfs`, no exceptions.**
//!
//! The trait is deliberately narrow — positioned read/write, sync, size, set_len
//! — which is the whole vocabulary a pager needs and nothing a network or a
//! POSIX namespace would drag in (D2).

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

/// A page-addressable file: positioned I/O, no implicit cursor.
///
/// `read_at`/`write_at` have read-exact / write-all semantics — they either
/// move the whole buffer or return an error — so callers never have to reason
/// about short transfers. `&self` throughout: positioned I/O needs no `&mut`,
/// which is what lets the buffer pool share one handle across future latches.
pub trait BlockFile: Send + Sync {
    /// Fill `buf` from `offset`. Errors with `UnexpectedEof` if the file is too
    /// short to satisfy the whole request.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Write all of `buf` at `offset`, extending the file (zero-filling any gap)
    /// if it starts at or past EOF.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Flush data + metadata durably (the `fsync` of [`FsyncPolicy::Full`]).
    fn sync(&self) -> io::Result<()>;

    /// Flush data durably, metadata best-effort (`fdatasync`). Defaults to a
    /// full sync for backends that don't distinguish.
    fn sync_data(&self) -> io::Result<()> {
        self.sync()
    }

    /// Current length in bytes.
    fn size(&self) -> io::Result<u64>;

    /// Truncate or extend to `len` (extension zero-fills).
    fn set_len(&self, len: u64) -> io::Result<()>;
}

/// The durability knob (§2.4). Database benchmarks lie by leaving this implicit;
/// KEEL threads it as a named value so every number can state which one it ran
/// under. `OffForBenchmarksOnly` is spelled that way on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncPolicy {
    Full,
    DataOnly,
    OffForBenchmarksOnly,
}

impl FsyncPolicy {
    /// Apply this policy to a file at a durability point.
    pub fn apply(self, f: &dyn BlockFile) -> io::Result<()> {
        match self {
            FsyncPolicy::Full => f.sync(),
            FsyncPolicy::DataOnly => f.sync_data(),
            FsyncPolicy::OffForBenchmarksOnly => Ok(()),
        }
    }
}

/// A real file on the host filesystem, using positioned I/O so no seek cursor is
/// shared. This is the production backing; the in-process crash campaign runs
/// over `MemDisk`/`FaultDisk` instead (see decisions.md D-VFS-1).
pub struct OsFile {
    file: File,
}

impl OsFile {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn open_readonly<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file })
    }
}

#[cfg(windows)]
fn pread(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "short read at EOF",
                ))
            }
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn pwrite(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "wrote zero bytes")),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn pread(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        match file.read_at(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "short read at EOF",
                ))
            }
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn pwrite(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "wrote zero bytes")),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl BlockFile for OsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        pread(&self.file, buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        pwrite(&self.file, buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

/// An in-memory byte medium shared by clonable handles. Used by tests and
/// microbenchmarks that want zero disk latency but no crash model; the crash
/// campaign layers `keel-faultfs` on top of the same idea.
///
/// A `MemDisk` clone shares the underlying bytes, so it models "the disk
/// survives, the process does not": drop every handle, clone a fresh one, and
/// the bytes are still there.
#[derive(Clone)]
pub struct MemDisk {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemDisk {
    pub fn new() -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::with_capacity(cap))),
        }
    }

    /// A snapshot copy of the whole medium — handy for `dbcheck` and assertions.
    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// Replace the whole medium (used by the fault injector to install a
    /// post-crash image).
    pub fn install(&self, image: Vec<u8>) {
        *self.bytes.lock().unwrap() = image;
    }

    pub fn len(&self) -> usize {
        self.bytes.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemDisk {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockFile for MemDisk {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "offset overflow"))?;
        if end > bytes.len() {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "read past EOF"));
        }
        buf.copy_from_slice(&bytes[start..end]);
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "offset overflow"))?;
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes.lock().unwrap().len() as u64)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.bytes.lock().unwrap().resize(len as usize, 0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: &dyn BlockFile) {
        f.write_at(b"hello", 0).unwrap();
        f.write_at(b"world", 100).unwrap();
        let mut b = [0u8; 5];
        f.read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"hello");
        f.read_at(&mut b, 100).unwrap();
        assert_eq!(&b, b"world");
        let mut gap = [0xFFu8; 5];
        f.read_at(&mut gap, 50).unwrap();
        assert_eq!(gap, [0u8; 5]);
        assert_eq!(f.size().unwrap(), 105);
    }

    #[test]
    fn memdisk_roundtrip() {
        roundtrip(&MemDisk::new());
    }

    #[test]
    fn memdisk_read_past_eof_errors() {
        let d = MemDisk::new();
        d.write_at(b"abc", 0).unwrap();
        let mut b = [0u8; 8];
        assert_eq!(
            d.read_at(&mut b, 0).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn memdisk_survives_reopen() {
        let disk = MemDisk::new();
        disk.write_at(b"durable", 0).unwrap();
        let reopened = disk.clone();
        drop(disk);
        let mut b = [0u8; 7];
        reopened.read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"durable");
    }

    #[test]
    fn osfile_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("keel-vfs-test-{}.dat", std::process::id()));
        let f = OsFile::open(&path).unwrap();
        roundtrip(&f);
        f.sync().unwrap();
        drop(f);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_len_truncates_and_extends() {
        let d = MemDisk::new();
        d.write_at(&[1, 2, 3, 4], 0).unwrap();
        d.set_len(2).unwrap();
        assert_eq!(d.size().unwrap(), 2);
        d.set_len(6).unwrap();
        let mut b = [9u8; 6];
        d.read_at(&mut b, 0).unwrap();
        assert_eq!(b, [1, 2, 0, 0, 0, 0]);
    }
}
