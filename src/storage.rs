//! On-disk snapshot storage.
//!
//! The storage file is a small page container.  A snapshot is encoded into a
//! sequence of fixed-size pages, each page carrying its own length and CRC.
//! This keeps the file format inspectable and lets recovery distinguish a
//! complete snapshot from a torn write without relying on external crates.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::crc::crc32;
use crate::db::{DbError, DbErrorKind, State};

pub const PAGE_SIZE: usize = 4096;
/// Maximum encoded snapshot size accepted by the file and byte APIs.
pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
const FILE_MAGIC: &[u8; 8] = b"BASALTDB";
const FILE_VERSION: u32 = 1;
const FILE_HEADER: usize = 64;
const PAGE_HEADER: usize = 24;

fn io_error(context: &str, e: io::Error) -> DbError {
    DbError::new(
        DbErrorKind::Io(format!("{context}: {e}")),
        format!("{context}: {e}"),
    )
}

/// Write a complete database snapshot atomically.
pub fn write_snapshot(path: &Path, state: &State, generation: u64) -> Result<(), DbError> {
    let payload = state.encode();
    let page_payload = PAGE_SIZE - PAGE_HEADER;
    let page_count = payload.len().div_ceil(page_payload).max(1);
    let file_len = FILE_HEADER
        .checked_add(
            page_count
                .checked_mul(PAGE_SIZE)
                .ok_or_else(|| corrupt("database snapshot is too large"))?,
        )
        .ok_or_else(|| corrupt("database snapshot is too large"))?;
    if file_len > MAX_SNAPSHOT_BYTES {
        return Err(corrupt("database snapshot is too large"));
    }

    let mut bytes = vec![0u8; file_len];
    bytes[..8].copy_from_slice(FILE_MAGIC);
    bytes[8..12].copy_from_slice(&FILE_VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    bytes[16..24].copy_from_slice(&generation.to_le_bytes());
    bytes[24..32].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes[32..40].copy_from_slice(&(page_count as u64).to_le_bytes());
    let header_crc = crc32(&bytes[..40]);
    bytes[40..44].copy_from_slice(&header_crc.to_le_bytes());

    for page in 0..page_count {
        let source_start = page * page_payload;
        let source_end = (source_start + page_payload).min(payload.len());
        let chunk = &payload[source_start..source_end];
        let offset = FILE_HEADER + page * PAGE_SIZE;
        bytes[offset..offset + 8].copy_from_slice(&(page as u64).to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&(chunk.len() as u64).to_le_bytes());
        bytes[offset + 16..offset + 20].copy_from_slice(&crc32(chunk).to_le_bytes());
        bytes[offset + 20..offset + 24].copy_from_slice(&0u32.to_le_bytes());
        bytes[offset + PAGE_HEADER..offset + PAGE_HEADER + chunk.len()].copy_from_slice(chunk);
    }

    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = Path::new(&tmp_os);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|e| io_error("create database directory", e))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(tmp)
        .map_err(|e| io_error("open snapshot temporary file", e))?;
    file.write_all(&bytes)
        .map_err(|e| io_error("write snapshot", e))?;
    file.sync_all().map_err(|e| io_error("sync snapshot", e))?;
    drop(file);
    let install_result = install_snapshot(tmp, path);
    if install_result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    install_result?;
    sync_parent(path)
}

#[cfg(not(windows))]
fn install_snapshot(tmp: &Path, path: &Path) -> Result<(), DbError> {
    fs::rename(tmp, path).map_err(|e| io_error("install snapshot", e))
}

#[cfg(windows)]
fn install_snapshot(tmp: &Path, path: &Path) -> Result<(), DbError> {
    match fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Windows does not replace an existing file with rename. The
            // synced WAL remains the recovery source if the process stops
            // between removing the old snapshot and installing the new one.
            fs::remove_file(path).map_err(|e| io_error("replace snapshot", e))?;
            fs::rename(tmp, path).map_err(|e| io_error("install snapshot", e))
        }
        Err(error) => Err(io_error("install snapshot", error)),
    }
}

/// Read a snapshot.  A missing file is treated as an empty database.
pub fn read_snapshot(path: &Path) -> Result<(State, u64), DbError> {
    if !path.exists() {
        return Ok((State::empty(), 0));
    }
    let file_len = fs::metadata(path)
        .map_err(|e| io_error("inspect database", e))?
        .len();
    if file_len > MAX_SNAPSHOT_BYTES as u64 {
        return Err(corrupt("database snapshot is too large"));
    }
    let file = File::open(path).map_err(|e| io_error("open database", e))?;
    let mut bytes = Vec::with_capacity(file_len as usize);
    file.take((MAX_SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("read database", e))?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(corrupt("database snapshot is too large"));
    }
    read_snapshot_bytes(&bytes)
}

/// Validate and decode snapshot bytes without touching the filesystem.
///
/// This is useful for embedded callers that already control the bytes and for
/// exercising the on-disk format boundary without creating a temporary file.
pub fn read_snapshot_bytes(bytes: &[u8]) -> Result<(State, u64), DbError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(corrupt("database snapshot is too large"));
    }
    if bytes.len() < FILE_HEADER {
        return Err(corrupt("database header is truncated"));
    }
    if &bytes[..8] != FILE_MAGIC {
        return Err(corrupt("invalid database magic"));
    }
    if u32_at(bytes, 8)? != FILE_VERSION {
        return Err(corrupt("unsupported database version"));
    }
    if u32_at(bytes, 12)? as usize != PAGE_SIZE {
        return Err(corrupt("unsupported database page size"));
    }
    let header_crc = u32_at(bytes, 40)?;
    if crc32(&bytes[..40]) != header_crc {
        return Err(corrupt("database header checksum mismatch"));
    }
    let generation = u64_at(bytes, 16)?;
    let payload_len = usize::try_from(u64_at(bytes, 24)?)
        .map_err(|_| corrupt("database payload is too large"))?;
    let page_count = usize::try_from(u64_at(bytes, 32)?)
        .map_err(|_| corrupt("database page count is too large"))?;
    if page_count == 0 || payload_len > page_count.saturating_mul(PAGE_SIZE - PAGE_HEADER) {
        return Err(corrupt("invalid database payload size"));
    }
    let expected = FILE_HEADER
        .checked_add(
            page_count
                .checked_mul(PAGE_SIZE)
                .ok_or_else(|| corrupt("database is too large"))?,
        )
        .ok_or_else(|| corrupt("database is too large"))?;
    if bytes.len() != expected {
        return Err(corrupt(
            "database page area is truncated or has trailing data",
        ));
    }
    let mut payload = Vec::with_capacity(payload_len);
    for page in 0..page_count {
        let offset = FILE_HEADER + page * PAGE_SIZE;
        if u64_at(bytes, offset)? != page as u64 {
            return Err(corrupt("database page sequence mismatch"));
        }
        let len = usize::try_from(u64_at(bytes, offset + 8)?)
            .map_err(|_| corrupt("database page is too large"))?;
        let payload_end = payload
            .len()
            .checked_add(len)
            .ok_or_else(|| corrupt("database payload is too large"))?;
        if len > PAGE_SIZE - PAGE_HEADER || payload_end > payload_len {
            return Err(corrupt("invalid database page length"));
        }
        let checksum = u32_at(bytes, offset + 16)?;
        let chunk = &bytes[offset + PAGE_HEADER..offset + PAGE_HEADER + len];
        if crc32(chunk) != checksum {
            return Err(corrupt("database page checksum mismatch"));
        }
        payload.extend_from_slice(chunk);
    }
    payload.truncate(payload_len);
    let state = State::decode(&payload)?;
    Ok((state, generation))
}

fn sync_parent(path: &Path) -> Result<(), DbError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn corrupt(message: &str) -> DbError {
    DbError::new(
        DbErrorKind::Io(message.to_string()),
        format!("corrupt database: {message}"),
    )
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, DbError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| corrupt("offset overflow"))?;
    let raw = bytes
        .get(offset..end)
        .ok_or_else(|| corrupt("database header is truncated"))?;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, DbError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| corrupt("offset overflow"))?;
    let raw = bytes
        .get(offset..end)
        .ok_or_else(|| corrupt("database header is truncated"))?;
    Ok(u64::from_le_bytes(raw.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::State;
    use crate::engine;
    use crate::sql::parser::parse;

    #[test]
    fn empty_snapshot_round_trips() {
        let dir = std::env::temp_dir().join(format!("basalt-storage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
        write_snapshot(&path, &State::empty(), 7).unwrap();
        let (loaded, generation) = read_snapshot(&path).unwrap();
        assert!(loaded.tables.is_empty());
        assert_eq!(generation, 7);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rewrites_an_existing_snapshot() {
        let dir =
            std::env::temp_dir().join(format!("basalt-storage-rewrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
        write_snapshot(&path, &State::empty(), 1).unwrap();
        write_snapshot(&path, &State::empty(), 2).unwrap();
        let (_, generation) = read_snapshot(&path).unwrap();
        assert_eq!(generation, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn table_snapshot_round_trips_tombstones_and_indexes() {
        let dir = std::env::temp_dir().join(format!("basalt-storage-rows-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
        let mut state = State::empty();
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER)",
            "INSERT INTO t VALUES (1, 10), (2, 20)",
            "CREATE INDEX value_idx ON t(value)",
            "DELETE FROM t WHERE id = 1",
        ] {
            let statement = &parse(sql).unwrap()[0];
            engine::execute(&mut state, statement).unwrap();
        }
        write_snapshot(&path, &state, 4).unwrap();
        let (loaded, generation) = read_snapshot(&path).unwrap();
        assert_eq!(generation, 4);
        let table = loaded.table("t").unwrap();
        assert_eq!(table.row_count(), 1);
        assert!(table.get_row(0).is_none());
        assert_eq!(
            table.get_row(1).unwrap()[0],
            crate::types::Value::Integer(2)
        );
        assert!(table.index(1).is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn page_checksum_rejects_mutation() {
        let dir = std::env::temp_dir().join(format!("basalt-storage-crc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
        write_snapshot(&path, &State::empty(), 0).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[FILE_HEADER + PAGE_HEADER] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(read_snapshot(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
