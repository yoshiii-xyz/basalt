//! Public database, connection, transaction, and recovery API.
//!
//! `State` remains available for small embedded/in-memory executor tests, but
//! applications should use [`Database`].  A transaction works on a private
//! snapshot and publishes it with an optimistic generation check.  Readers
//! therefore never observe half of a write, and concurrent writers fail with a
//! transaction conflict instead of silently losing updates.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::db::{DbError, DbErrorKind, State, StatementResult, dberr};
use crate::sql::ast::Statement;
use crate::sql::parser::parse;
use crate::{storage, wal};

struct Inner {
    path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
    _lock_file: Option<File>,
    _workspace_lock_file: Option<File>,
    state: RwLock<State>,
    generation: AtomicU64,
    commit_lock: Mutex<()>,
}

/// A cloneable handle to an embedded Basalt database.
#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

impl Database {
    /// Create an empty, non-durable database.
    pub fn in_memory() -> Database {
        Database {
            inner: Arc::new(Inner {
                path: None,
                wal_path: None,
                _lock_file: None,
                _workspace_lock_file: None,
                state: RwLock::new(State::empty()),
                generation: AtomicU64::new(0),
                commit_lock: Mutex::new(()),
            }),
        }
    }

    /// Open or create a durable database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Database, DbError> {
        Self::open_internal(path, false)
    }

    /// Open a workspace database when the caller already owns its workspace
    /// lock. The returned handle still owns the database lock as usual.
    pub(crate) fn open_in_workspace(path: impl AsRef<Path>) -> Result<Database, DbError> {
        Self::open_internal(path, true)
    }

    fn open_internal(
        path: impl AsRef<Path>,
        workspace_lock_already_held: bool,
    ) -> Result<Database, DbError> {
        let path = path.as_ref().to_path_buf();
        let workspace_lock_file = if workspace_lock_already_held {
            None
        } else {
            acquire_workspace_lock(&path)?
        };
        let lock_file = acquire_lock(&path)?;
        let wal_path = wal_path(&path);
        let frame = wal::latest(&wal_path)?;
        let snapshot = storage::read_snapshot(&path);
        let (mut state, mut generation, mut repair_snapshot) = match snapshot {
            Ok((state, generation)) => (state, generation, false),
            Err(error) => {
                let Some(frame) = &frame else {
                    return Err(error);
                };
                (State::decode(&frame.payload)?, frame.generation, true)
            }
        };
        if let Some(frame) = frame {
            if frame.generation > generation {
                state = State::decode(&frame.payload)?;
                generation = frame.generation;
                // Complete recovery before exposing the handle.  If this
                // process is killed again, the valid WAL frame remains.
                storage::write_snapshot(&path, &state, generation)?;
                wal::truncate(&wal_path)?;
                repair_snapshot = false;
            } else {
                // The snapshot is at least as new as every WAL frame; a
                // previous checkpoint may have been interrupted after the
                // snapshot install and left stale frames behind.
                if repair_snapshot {
                    storage::write_snapshot(&path, &state, generation)?;
                    repair_snapshot = false;
                }
                wal::truncate(&wal_path)?;
            }
        }
        if repair_snapshot || !path.exists() {
            storage::write_snapshot(&path, &state, generation)?;
        }
        Ok(Database {
            inner: Arc::new(Inner {
                path: Some(path),
                wal_path: Some(wal_path),
                _lock_file: Some(lock_file),
                _workspace_lock_file: workspace_lock_file,
                state: RwLock::new(state),
                generation: AtomicU64::new(generation),
                commit_lock: Mutex::new(()),
            }),
        })
    }

    /// Begin an optimistic snapshot transaction.
    pub fn begin(&self) -> Result<Transaction, DbError> {
        let state_guard = self
            .inner
            .state
            .read()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database state lock poisoned"))?;
        // The generation is published while the write lock is held. Reading
        // it under the same read guard keeps the cloned state and its
        // snapshot number from crossing a concurrent commit.
        let state = state_guard.clone();
        let generation = self.inner.generation.load(Ordering::Acquire);
        Ok(Transaction {
            db: self.clone(),
            state,
            base_generation: generation,
            active: true,
            dirty: false,
        })
    }

    /// Alias for [`Database::begin`] using the conventional transaction name.
    pub fn transaction(&self) -> Result<Transaction, DbError> {
        self.begin()
    }

    /// Create a stateful connection.  Connections are useful when SQL
    /// `BEGIN`, `COMMIT`, and `ROLLBACK` statements span multiple calls.
    pub fn connect(&self) -> Connection {
        Connection {
            db: self.clone(),
            transaction: None,
        }
    }

    /// Execute one statement as an autocommit operation.
    pub fn execute(&self, stmt: &Statement) -> Result<StatementResult, DbError> {
        match stmt {
            Statement::Checkpoint => {
                self.checkpoint()?;
                Ok(StatementResult::Checkpoint)
            }
            Statement::Begin => Ok(StatementResult::Begin),
            Statement::Commit => Ok(StatementResult::Commit),
            Statement::Rollback => Ok(StatementResult::Rollback),
            _ => {
                let mut transaction = self.begin()?;
                let result = transaction.execute(stmt)?;
                if is_mutation(&result) {
                    transaction.commit()?;
                } else {
                    transaction.rollback();
                }
                Ok(result)
            }
        }
    }

    /// Parse and execute all statements through a fresh autocommit connection.
    pub fn execute_sql(&self, sql: &str) -> Result<Vec<StatementResult>, DbError> {
        self.connect().execute_sql(sql)
    }

    /// Flush the current state into the page file and clear committed WAL
    /// frames.  Checkpointing is safe while readers are active.
    pub fn checkpoint(&self) -> Result<(), DbError> {
        let _commit = self
            .inner
            .commit_lock
            .lock()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database commit lock poisoned"))?;
        let Some(path) = &self.inner.path else {
            return Ok(());
        };
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database state lock poisoned"))?
            .clone();
        let generation = self.inner.generation.load(Ordering::Acquire);
        storage::write_snapshot(path, &state, generation)?;
        if let Some(wal_path) = &self.inner.wal_path {
            wal::truncate(wal_path)?;
        }
        Ok(())
    }

    /// Current committed generation, useful for diagnostics and tests.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Return table names in deterministic order for schema discovery tools.
    pub fn table_names(&self) -> Result<Vec<String>, DbError> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database state lock poisoned"))?;
        let mut names: Vec<String> = state.tables.keys().cloned().collect();
        names.sort_by_key(|name| name.to_ascii_lowercase());
        Ok(names)
    }

    /// Return a table's column metadata for migrations and introspection.
    pub fn columns(&self, table: &str) -> Result<Vec<crate::db::Column>, DbError> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database state lock poisoned"))?;
        state
            .table(table)
            .map(|value| value.columns.clone())
            .ok_or_else(|| dberr(DbErrorKind::UnknownTable, format!("no such table: {table}")))
    }

    fn commit_state(&self, state: State, expected: u64) -> Result<u64, DbError> {
        let _commit = self
            .inner
            .commit_lock
            .lock()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database commit lock poisoned"))?;
        let mut current = self
            .inner
            .state
            .write()
            .map_err(|_| dberr(DbErrorKind::Transaction, "database state lock poisoned"))?;
        let actual = self.inner.generation.load(Ordering::Acquire);
        if actual != expected {
            return Err(dberr(
                DbErrorKind::Transaction,
                format!("transaction conflict: snapshot {expected}, database is at {actual}"),
            ));
        }
        let generation = actual
            .checked_add(1)
            .ok_or_else(|| dberr(DbErrorKind::Transaction, "transaction generation exhausted"))?;
        if let Some(wal_path) = &self.inner.wal_path {
            let payload = state.encode();
            wal::append(wal_path, generation, &payload)?;
        }
        *current = state;
        self.inner.generation.store(generation, Ordering::Release);
        Ok(generation)
    }
}

/// A connection that can hold one transaction across multiple statements.
pub struct Connection {
    db: Database,
    transaction: Option<Transaction>,
}

impl Connection {
    pub fn execute(&mut self, stmt: &Statement) -> Result<StatementResult, DbError> {
        match stmt {
            Statement::Checkpoint => {
                if self.transaction.is_some() {
                    return Err(dberr(
                        DbErrorKind::Transaction,
                        "cannot checkpoint while a transaction is active",
                    ));
                }
                self.db.checkpoint()?;
                Ok(StatementResult::Checkpoint)
            }
            Statement::Begin => {
                if self.transaction.is_some() {
                    return Err(dberr(
                        DbErrorKind::Transaction,
                        "transaction already active",
                    ));
                }
                self.transaction = Some(self.db.begin()?);
                Ok(StatementResult::Begin)
            }
            Statement::Commit => {
                let Some(transaction) = self.transaction.take() else {
                    return Err(dberr(DbErrorKind::Transaction, "no transaction is active"));
                };
                transaction.commit()?;
                Ok(StatementResult::Commit)
            }
            Statement::Rollback => {
                if let Some(transaction) = self.transaction.take() {
                    transaction.rollback();
                }
                Ok(StatementResult::Rollback)
            }
            _ => match self.transaction.as_mut() {
                Some(transaction) => transaction.execute(stmt),
                None => self.db.execute(stmt),
            },
        }
    }

    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<StatementResult>, DbError> {
        let statements = parse(sql).map_err(|e| {
            dberr(
                DbErrorKind::Syntax(e.message.clone()),
                format!("{} at byte {}", e.message, e.offset),
            )
        })?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute(&statement)?);
        }
        Ok(results)
    }

    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// Return the committed generation visible to this connection's database.
    pub fn generation(&self) -> u64 {
        self.db.generation()
    }
}

/// A private MVCC-style snapshot.  Reads use the snapshot without holding a
/// database lock; commit publishes it only if no newer generation exists.
pub struct Transaction {
    db: Database,
    state: State,
    base_generation: u64,
    active: bool,
    dirty: bool,
}

impl Transaction {
    pub fn execute(&mut self, stmt: &Statement) -> Result<StatementResult, DbError> {
        if !self.active {
            return Err(dberr(DbErrorKind::Transaction, "transaction is closed"));
        }
        match stmt {
            Statement::Begin | Statement::Commit | Statement::Rollback | Statement::Checkpoint => {
                Err(dberr(
                    DbErrorKind::Transaction,
                    "transaction control is owned by the connection",
                ))
            }
            _ => {
                let result = crate::engine::execute(&mut self.state, stmt)?;
                if is_mutation(&result) {
                    self.dirty = true;
                }
                Ok(result)
            }
        }
    }

    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<StatementResult>, DbError> {
        let statements = parse(sql).map_err(|e| {
            dberr(
                DbErrorKind::Syntax(e.message.clone()),
                format!("{} at byte {}", e.message, e.offset),
            )
        })?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute(&statement)?);
        }
        Ok(results)
    }

    pub fn commit(mut self) -> Result<u64, DbError> {
        if !self.active {
            return Err(dberr(DbErrorKind::Transaction, "transaction is closed"));
        }
        self.active = false;
        if !self.dirty {
            return Ok(self.db.generation());
        }
        self.db.commit_state(self.state, self.base_generation)
    }

    pub fn rollback(mut self) {
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

fn is_mutation(result: &StatementResult) -> bool {
    matches!(
        result,
        StatementResult::Insert { .. }
            | StatementResult::Update { .. }
            | StatementResult::Delete { .. }
            | StatementResult::CreateTable { .. }
            | StatementResult::DropTable { .. }
            | StatementResult::CreateIndex { .. }
            | StatementResult::DropIndex { .. }
    )
}

fn wal_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".wal");
    PathBuf::from(value)
}

fn acquire_lock(path: &Path) -> Result<File, DbError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            dberr(
                DbErrorKind::Io(format!("create database directory: {error}")),
                format!("create database directory: {error}"),
            )
        })?;
    }
    let mut lock_os = path.as_os_str().to_os_string();
    lock_os.push(".lock");
    let lock_path = PathBuf::from(lock_os);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            dberr(
                DbErrorKind::Io(format!("open database lock: {error}")),
                format!("open database lock: {error}"),
            )
        })?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(file),
        Err(fs4::TryLockError::WouldBlock) => Err(dberr(
            DbErrorKind::Busy,
            format!("database is already open: {}", path.display()),
        )),
        Err(fs4::TryLockError::Error(error)) => Err(dberr(
            DbErrorKind::Io(format!("lock database: {error}")),
            format!("lock database: {error}"),
        )),
    }
}

fn acquire_workspace_lock(path: &Path) -> Result<Option<File>, DbError> {
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("data.basalt"))
    {
        return Ok(None);
    }
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(None);
    };
    let lock_path = parent.join(".workspace.lock");
    let metadata = match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(dberr(
                DbErrorKind::Io(format!("inspect workspace lock: {error}")),
                format!("inspect workspace lock: {error}"),
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(dberr(
            DbErrorKind::Io("workspace lock cannot be a symbolic link".into()),
            "workspace lock cannot be a symbolic link",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            dberr(
                DbErrorKind::Io(format!("open workspace lock: {error}")),
                format!("open workspace lock: {error}"),
            )
        })?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(file)),
        Err(fs4::TryLockError::WouldBlock) => Err(dberr(
            DbErrorKind::Busy,
            format!("workspace is already open: {}", parent.display()),
        )),
        Err(fs4::TryLockError::Error(error)) => Err(dberr(
            DbErrorKind::Io(format!("lock workspace: {error}")),
            format!("lock workspace: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn durable_commit_reopens() {
        let dir = std::env::temp_dir().join(format!("basalt-db-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.db");
        {
            let db = Database::open(&path).unwrap();
            db.execute_sql(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'one');",
            )
            .unwrap();
            db.checkpoint().unwrap();
        }
        let db = Database::open(&path).unwrap();
        let result = db.execute_sql("SELECT * FROM t").unwrap();
        assert!(matches!(
            &result[0],
            StatementResult::Select { rows, .. } if rows.len() == 1
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_snapshot_conflicts() {
        let db = Database::in_memory();
        db.execute_sql("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let mut a = db.begin().unwrap();
        let mut b = db.begin().unwrap();
        a.execute_sql("INSERT INTO t VALUES (1)").unwrap();
        b.execute_sql("INSERT INTO t VALUES (2)").unwrap();
        a.commit().unwrap();
        assert!(b.commit().is_err());
    }

    #[test]
    fn replays_wal_and_restores_user_indexes() {
        let dir = std::env::temp_dir().join(format!("basalt-wal-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.db");
        {
            let db = Database::open(&path).unwrap();
            db.execute_sql("CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER)")
                .unwrap();
            db.execute_sql("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
                .unwrap();
            db.execute_sql("CREATE INDEX value_idx ON t(value)")
                .unwrap();
            // Deliberately do not checkpoint: reopening must use the WAL.
        }
        let db = Database::open(&path).unwrap();
        let results = db.execute_sql("SELECT id FROM t WHERE value = 20").unwrap();
        let StatementResult::Select { rows, .. } = &results[0] else {
            panic!()
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(db.generation(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn readers_and_writer_can_share_a_handle() {
        let db = Database::in_memory();
        db.execute_sql("CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER)")
            .unwrap();
        db.execute_sql("INSERT INTO t VALUES (1, 0)").unwrap();
        let writer_db = db.clone();
        let writer = std::thread::spawn(move || {
            for value in 1..=20 {
                writer_db
                    .execute_sql(&format!("UPDATE t SET value = {value} WHERE id = 1"))
                    .unwrap();
            }
        });
        let mut readers = Vec::new();
        for _ in 0..4 {
            let reader_db = db.clone();
            readers.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    let result = reader_db.execute_sql("SELECT value FROM t").unwrap();
                    assert!(matches!(&result[0], StatementResult::Select { rows, .. } if rows.len() == 1));
                }
            }));
        }
        writer.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        let result = db.execute_sql("SELECT value FROM t").unwrap();
        assert!(
            matches!(&result[0], StatementResult::Select { rows, .. } if rows[0][0] == crate::types::Value::Integer(20))
        );
    }

    #[test]
    fn relative_paths_are_supported() {
        let filename = format!("basalt-relative-{}.tmp", std::process::id());
        let path = std::path::Path::new(&filename);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{filename}.wal"));
        let database = Database::open(path).unwrap();
        database
            .execute_sql("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        database.checkpoint().unwrap();
        assert!(path.exists());
        drop(database);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{filename}.wal"));
    }

    #[test]
    fn valid_wal_recovers_a_corrupt_snapshot() {
        let dir =
            std::env::temp_dir().join(format!("basalt-corrupt-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.db");
        {
            let database = Database::open(&path).unwrap();
            database
                .execute_sql("CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (7)")
                .unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        let database = Database::open(&path).unwrap();
        let result = database.execute_sql("SELECT * FROM t").unwrap();
        assert!(matches!(&result[0], StatementResult::Select { rows, .. } if rows.len() == 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn durable_database_rejects_a_second_open_handle() {
        let dir = std::env::temp_dir().join(format!("basalt-db-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.db");
        let first = Database::open(&path).unwrap();
        let error = match Database::open(&path) {
            Ok(_) => panic!("a second database open should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind, DbErrorKind::Busy);
        assert!(error.message.contains("already open"));
        drop(first);
        let second = Database::open(&path).unwrap();
        drop(second);
        let _ = fs::remove_dir_all(dir);
    }
}
