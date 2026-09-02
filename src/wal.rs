//! Append-only write-ahead log for committed state snapshots.
//!
//! A frame is considered committed only after its complete header, payload,
//! and checksums have reached the WAL file. Recovery accepts valid complete
//! frames and repairs and ignores an incomplete final frame, which is the
//! normal result of a killed process during append.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::crc::crc32;
use crate::db::{DbError, DbErrorKind};
use crate::storage;

const MAGIC: &[u8; 4] = b"BSWL";
const LEGACY_VERSION: u32 = 1;
const VERSION: u32 = 2;
const HEADER: usize = 32;
/// Maximum total WAL size before callers must checkpoint.
pub const MAX_WAL_BYTES: u64 = (storage::MAX_SNAPSHOT_BYTES as u64) * 4;
const MAX_PAYLOAD_BYTES: usize = storage::MAX_SNAPSHOT_PAYLOAD_BYTES;

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

/// Append a committed frame.
///
/// Callers must provide a generation greater than every complete frame already
/// in the file. [`latest`] validates that invariant during recovery; the
/// database commit path supplies generations from its serialized commit lock
/// without rescanning the full log for every append.
pub fn append(path: &Path, generation: u64, payload: &[u8]) -> Result<(), DbError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(limit("database state is too large for the WAL"));
    }
    let frame_len = HEADER
        .checked_add(payload.len())
        .ok_or_else(|| limit("WAL frame is too large"))?;
    let existing_len = existing_file_len(path)?.unwrap_or(0);
    ensure_wal_size(existing_len, frame_len as u64)?;
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
    let header_checksum = crc32(&header[..28]);
    header[28..32].copy_from_slice(&header_checksum.to_le_bytes());
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| io_error("open WAL", e))?;
    let actual_len = file
        .metadata()
        .map_err(|e| io_error("inspect WAL", e))?
        .len();
    ensure_wal_size(actual_len, frame_len as u64)?;
    file.write_all(&header)
        .map_err(|e| io_error("write WAL header", e))?;
    file.write_all(payload)
        .map_err(|e| io_error("write WAL payload", e))?;
    file.sync_all().map_err(|e| io_error("sync WAL", e))?;
    sync_parent(path)
}

/// Return the highest valid frame.  A partial/corrupt tail is ignored; an
/// invalid frame before the tail is an error because it would hide later data.
pub fn latest(path: &Path) -> Result<Option<Frame>, DbError> {
    let Some(file_len) = existing_file_len(path)? else {
        return Ok(None);
    };
    if file_len > MAX_WAL_BYTES {
        return Err(limit(
            "WAL is too large; checkpoint the database before retrying",
        ));
    }
    let mut file = File::open(path).map_err(|e| io_error("open WAL", e))?;
    let mut offset = 0u64;
    let mut latest = None;
    let mut previous_generation = None;
    loop {
        let mut header = [0u8; HEADER];
        let header_len =
            read_prefix(&mut file, &mut header).map_err(|e| io_error("read WAL header", e))?;
        if header_len == 0 {
            break;
        }
        if header_len < HEADER {
            truncate_to(path, offset)?;
            break;
        }
        if &header[..4] != MAGIC {
            return Err(corrupt("invalid WAL magic"));
        }
        let version = u32_at(&header, 4)?;
        if version == VERSION {
            let header_checksum = u32_at(&header, 28)?;
            if crc32(&header[..28]) != header_checksum {
                return Err(corrupt("WAL header checksum mismatch"));
            }
        } else if version != LEGACY_VERSION {
            return Err(corrupt("unsupported WAL version"));
        }
        let generation = u64_at(&header, 8)?;
        if previous_generation.is_some_and(|previous| generation <= previous) {
            return Err(corrupt("WAL generations are not strictly increasing"));
        }
        previous_generation = Some(generation);
        let declared_len = u64_at(&header, 16)?;
        if declared_len > MAX_PAYLOAD_BYTES as u64 {
            return Err(limit("WAL frame payload is too large"));
        }
        let len = match usize::try_from(declared_len) {
            Ok(len) => len,
            Err(_) => return Err(limit("WAL frame payload is too large")),
        };
        let frame_len = (HEADER as u64)
            .checked_add(declared_len)
            .ok_or_else(|| limit("WAL frame is too large"))?;
        let end = offset
            .checked_add(frame_len)
            .ok_or_else(|| limit("WAL offset is too large"))?;
        if end > file_len {
            truncate_to(path, offset)?;
            break;
        }
        let mut payload = vec![0u8; len];
        if let Err(error) = file.read_exact(&mut payload) {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                truncate_to(path, offset)?;
                break;
            }
            return Err(io_error("read WAL payload", error));
        }
        let checksum = u32_at(&header, 24)?;
        if crc32(&payload) != checksum {
            return Err(corrupt("WAL frame checksum mismatch"));
        }
        if latest
            .as_ref()
            .map(|f: &Frame| generation > f.generation)
            .unwrap_or(true)
        {
            latest = Some(Frame {
                generation,
                payload,
            });
        }
        offset = end;
    }
    Ok(latest)
}

pub fn truncate(path: &Path) -> Result<(), DbError> {
    if existing_file_len(path)?.is_none() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| io_error("truncate WAL", e))?;
    file.sync_all()
        .map_err(|e| io_error("sync truncated WAL", e))?;
    sync_parent(path)
}

fn truncate_to(path: &Path, length: u64) -> Result<(), DbError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| io_error("open WAL for tail repair", e))?;
    file.set_len(length)
        .map_err(|e| io_error("truncate incomplete WAL frame", e))?;
    file.sync_all()
        .map_err(|e| io_error("sync repaired WAL", e))?;
    sync_parent(path)
}

fn corrupt(message: &str) -> DbError {
    DbError::new(
        DbErrorKind::Io(message.to_string()),
        format!("corrupt WAL: {message}"),
    )
}

fn limit(message: &str) -> DbError {
    DbError::new(DbErrorKind::Limit, message)
}

fn existing_file_len(path: &Path) -> Result<Option<u64>, DbError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect WAL", error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(path_error("WAL cannot be a symbolic link"));
    }
    if !metadata.is_file() {
        return Err(path_error("WAL is not a regular file"));
    }
    Ok(Some(metadata.len()))
}

fn ensure_wal_size(existing_len: u64, additional_len: u64) -> Result<(), DbError> {
    let total = existing_len
        .checked_add(additional_len)
        .ok_or_else(|| limit("WAL is too large; checkpoint the database before retrying"))?;
    if total > MAX_WAL_BYTES {
        return Err(limit(
            "WAL is full; checkpoint the database before retrying the write",
        ));
    }
    Ok(())
}

fn read_prefix(file: &mut File, bytes: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < bytes.len() {
        let count = file.read(&mut bytes[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    Ok(read)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), DbError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let dir = File::open(parent).map_err(|e| io_error("open WAL directory", e))?;
        dir.sync_all()
            .map_err(|e| io_error("sync WAL directory", e))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), DbError> {
    Ok(())
}

fn path_error(message: &str) -> DbError {
    DbError::new(DbErrorKind::Io(message.to_string()), message)
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

    #[test]
    fn rejects_an_oversized_frame_before_allocating_its_payload() {
        let dir = std::env::temp_dir().join(format!("basalt-wal-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        let mut header = [0u8; HEADER];
        header[..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&1u64.to_le_bytes());
        header[16..24].copy_from_slice(&(MAX_PAYLOAD_BYTES as u64 + 1).to_le_bytes());
        header[24..28].copy_from_slice(&0u32.to_le_bytes());
        let header_checksum = crc32(&header[..28]);
        header[28..32].copy_from_slice(&header_checksum.to_le_bytes());
        fs::write(&path, header).unwrap();

        let error = latest(&path).unwrap_err();

        assert_eq!(error.kind, DbErrorKind::Limit);
        assert!(error.message.contains("payload is too large"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_wal_file_above_the_total_limit() {
        let dir =
            std::env::temp_dir().join(format!("basalt-wal-total-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(MAX_WAL_BYTES + 1).unwrap();
        drop(file);

        let error = latest(&path).unwrap_err();

        assert_eq!(error.kind, DbErrorKind::Limit);
        assert!(error.message.contains("WAL is too large"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_changed_v2_header_even_when_the_payload_is_intact() {
        let dir =
            std::env::temp_dir().join(format!("basalt-wal-header-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        append(&path, 1, b"one").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[8] ^= 1;
        fs::write(&path, bytes).unwrap();

        let error = latest(&path).unwrap_err();

        assert!(error.message.contains("header checksum mismatch"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_non_monotonic_wal_generations_during_recovery() {
        let dir = std::env::temp_dir().join(format!(
            "basalt-wal-generation-order-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        append(&path, 2, b"two").unwrap();
        let payload = b"one";
        let mut header = [0u8; HEADER];
        header[..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&1u64.to_le_bytes());
        header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        header[24..28].copy_from_slice(&crc32(payload).to_le_bytes());
        let header_checksum = crc32(&header[..28]);
        header[28..32].copy_from_slice(&header_checksum.to_le_bytes());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&header).unwrap();
        file.write_all(payload).unwrap();
        file.sync_all().unwrap();

        let error = latest(&path).unwrap_err();

        assert!(error.message.contains("not strictly increasing"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_legacy_v1_frames_during_upgrade() {
        let dir = std::env::temp_dir().join(format!("basalt-wal-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.wal");
        let payload = b"legacy";
        let mut header = [0u8; HEADER];
        header[..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&LEGACY_VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&1u64.to_le_bytes());
        header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        header[24..28].copy_from_slice(&crc32(payload).to_le_bytes());
        fs::write(&path, [header.as_slice(), payload].concat()).unwrap();

        let frame = latest(&path).unwrap().unwrap();

        assert_eq!(frame.generation, 1);
        assert_eq!(frame.payload, payload);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symbolic_link_wal() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("basalt-wal-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("outside.wal");
        let path = dir.join("db.wal");
        fs::write(&target, b"").unwrap();
        symlink(&target, &path).unwrap();

        let error = latest(&path).unwrap_err();

        assert_eq!(
            error.kind,
            DbErrorKind::Io("WAL cannot be a symbolic link".into())
        );
        let _ = fs::remove_dir_all(dir);
    }
}
