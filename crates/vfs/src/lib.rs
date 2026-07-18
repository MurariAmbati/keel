use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

pub trait BlockFile: Send + Sync {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    fn sync(&self) -> io::Result<()>;

    fn sync_data(&self) -> io::Result<()> {
        self.sync()
    }

    fn size(&self) -> io::Result<u64>;

    fn set_len(&self, len: u64) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncPolicy {
    Full,
    DataOnly,
    OffForBenchmarksOnly,
}

impl FsyncPolicy {
    pub fn apply(self, f: &dyn BlockFile) -> io::Result<()> {
        match self {
            FsyncPolicy::Full => f.sync(),
            FsyncPolicy::DataOnly => f.sync_data(),
            FsyncPolicy::OffForBenchmarksOnly => Ok(()),
        }
    }
}

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

    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

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
