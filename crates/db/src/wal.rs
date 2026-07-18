use std::cell::Cell;
use std::sync::Arc;

use keel_vfs::BlockFile;

const LOG_MAGIC: u32 = 0x4B4C_4F47;
const HEADER: usize = 12;

pub(crate) struct StmtLog {
    file: Arc<dyn BlockFile>,
    end: Cell<u64>,
}

impl StmtLog {
    pub fn open(file: Arc<dyn BlockFile>) -> Self {
        StmtLog {
            file,
            end: Cell::new(0),
        }
    }

    pub fn append(&self, sql: &[u8]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(HEADER + sql.len());
        buf.extend_from_slice(&LOG_MAGIC.to_le_bytes());
        buf.extend_from_slice(&(sql.len() as u32).to_le_bytes());
        buf.extend_from_slice(&keel_page::crc32(sql).to_le_bytes());
        buf.extend_from_slice(sql);
        let off = self.end.get();
        self.file.write_at(&buf, off)?;
        self.file.sync()?;
        self.end.set(off + buf.len() as u64);
        Ok(())
    }

    pub fn replay(&self) -> std::io::Result<Vec<Vec<u8>>> {
        let size = self.file.size()? as usize;
        let mut all = vec![0u8; size];
        if size > 0 {
            self.file.read_at(&mut all, 0)?;
        }
        let mut out = Vec::new();
        let mut pos = 0usize;
        let mut valid_end = 0u64;
        while pos + HEADER <= all.len() {
            let magic = u32::from_le_bytes(all[pos..pos + 4].try_into().unwrap());
            if magic != LOG_MAGIC {
                break;
            }
            let len = u32::from_le_bytes(all[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(all[pos + 8..pos + 12].try_into().unwrap());
            let start = pos + HEADER;
            if start + len > all.len() {
                break;
            }
            let bytes = &all[start..start + len];
            if keel_page::crc32(bytes) != crc {
                break;
            }
            out.push(bytes.to_vec());
            pos = start + len;
            valid_end = pos as u64;
        }
        self.end.set(valid_end);
        Ok(out)
    }
}
