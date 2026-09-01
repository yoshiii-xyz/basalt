//! Append-only write-ahead log for committed state snapshots.
//!
//! A frame is considered committed only after its complete payload and CRC
//! have reached the WAL file.  Recovery accepts valid complete frames and
//! repairs and ignores an incomplete final frame, which is the normal result
//! of a killed process during append.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::crc::crc32;
use crate::db::{DbError, DbErrorKind};

const MAGIC: &[u8; 4] = b"BSWL";
const VERSION: u32 = 1;
const HEADER: usize = 32;

#[derive(Debug, Clone)]
pub struct Frame {
    pub generation: u64,
    pub payload: Vec<u8>,
}

fn io_error(context: &str, e: io::Error) -> DbError {
    DbError::new(
        DbErrorKind::Io(format!("{context}: {e}")),
        format!("{context}: {e}"),
    )
}

pub fn append(path: &Path, generation: u64, payload: &[u8]) -> Result<(), DbError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|e| io_error("create WAL directory", e))?;
    }
    let mut header = [0u8; HEADER];
    header[..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..16].copy_from_slice(&generation.to_le_bytes());
    header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[24..28].copy_from_slice(&crc32(payload).to_le_bytes());
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| io_error("open WAL", e))?;
    file.write_all(&header)
        .map_err(|e| io_error("write WAL header", e))?;
    file.write_all(payload)
        .map_err(|e| io_error("write WAL payload", e))?;
    file.sync_all().map_err(|e| io_error("sync WAL", e))
}

/// Return the highest valid frame.  A partial/corrupt tail is ignored; an
/// invalid frame before the tail is an error because it would hide later data.
pub fn latest(path: &Path) -> Result<Option<Frame>, DbError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| io_error("open WAL", e))?
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("read WAL", e))?;
    let mut offset = 0usize;
    let mut latest = None;
    while offset < bytes.len() {
        if bytes.len() - offset < HEADER {
            truncate_to(path, offset)?;
            break;
        }
        if &bytes[offset..offset + 4] != MAGIC {
            return Err(corrupt("invalid WAL magic"));
        }
        let version = u32_at(&bytes, offset + 4)?;
        if version != VERSION {
            return Err(corrupt("unsupported WAL version"));
        }
        let generation = u64_at(&bytes, offset + 8)?;
        let declared_len = u64_at(&bytes, offset + 16)?;
        let len = match usize::try_from(declared_len) {
            Ok(len) => len,
            Err(_) => {
                truncate_to(path, offset)?;
                break;
            }
        };
        let checksum = u32_at(&bytes, offset + 24)?;
        let end = match offset.checked_add(HEADER).and_then(|n| n.checked_add(len)) {
            Some(end) if end <= bytes.len() => end,
            _ => {
                truncate_to(path, offset)?;
                break;
            }
        };
        let payload = &bytes[offset + HEADER..end];
        if crc32(payload) != checksum {
            return Err(corrupt("WAL frame checksum mismatch"));
        }
        if latest
            .as_ref()
            .map(|f: &Frame| generation > f.generation)
            .unwrap_or(true)
        {
            latest = Some(Frame {
                generation,
                payload: payload.to_vec(),
            });
        }
        offset = end;
    }
    Ok(latest)
}

pub fn truncate(path: &Path) -> Result<(), DbError> {
    if !path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| io_error("truncate WAL", e))?;
    file.sync_all()
        .map_err(|e| io_error("sync truncated WAL", e))
}

fn truncate_to(path: &Path, length: usize) -> Result<(), DbError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| io_error("open WAL for tail repair", e))?;
    file.set_len(length as u64)
        .map_err(|e| io_error("truncate incomplete WAL frame", e))?;
    file.sync_all()
        .map_err(|e| io_error("sync repaired WAL", e))
}

fn corrupt(message: &str) -> DbError {
    DbError::new(
        DbErrorKind::Io(message.to_string()),
        format!("corrupt WAL: {message}"),
    )
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, DbError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| corrupt("WAL header is truncated"))?;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, DbError> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| corrupt("WAL header is truncated"))?;
    Ok(u64::from_le_bytes(raw.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_torn_tail() {
        let dir = std::env::temp_dir().join(format!("basalt-wal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        append(&path, 1, b"one").unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"BSWL").unwrap();
        file.sync_all().unwrap();
        assert_eq!(latest(&path).unwrap().unwrap().payload, b"one");
        truncate(&path).unwrap();
        assert!(latest(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_complete_corrupt_frame() {
        let dir = std::env::temp_dir().join(format!("basalt-wal-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        append(&path, 1, b"one").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(latest(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn repairs_a_torn_tail_before_a_later_commit() {
        let dir = std::env::temp_dir().join(format!("basalt-wal-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        append(&path, 1, b"one").unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"BSWL")
            .unwrap();

        assert_eq!(latest(&path).unwrap().unwrap().generation, 1);
        append(&path, 2, b"two").unwrap();
        let frame = latest(&path).unwrap().unwrap();
        assert_eq!(frame.generation, 2);
        assert_eq!(frame.payload, b"two");
        let _ = fs::remove_dir_all(dir);
    }
}
