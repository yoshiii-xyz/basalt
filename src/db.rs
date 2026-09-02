//! Storage core: catalog, tables, rows, and constraint enforcement.
//!
//! Rows live in a slot array (`Vec<Option<Row>>`) so row ids stay stable across
//! deletes; tombstones are skipped during scans. PRIMARY KEY / UNIQUE
//! constraints are enforced through hand-written B+trees keyed by (Value, rid).

use std::collections::{HashMap, HashSet};

use crate::btree::BTree;
use crate::types::{ColumnType, Value};

pub type Row = Vec<Value>;

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    pub unique: bool,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbErrorKind {
    UnknownTable,
    DuplicateColumn,
    Constraint,
    TypeMismatch,
    Syntax(String),
    NotNull,
    UnknownColumn,
    ColumnCount,
    Io(String),
    Busy,
    Transaction,
    Limit,
    Internal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    pub kind: DbErrorKind,
    pub message: String,
}

impl DbError {
    pub fn new(kind: DbErrorKind, message: impl Into<String>) -> DbError {
        DbError {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DbError {}

/// A snapshot of the whole database, shared by readers and swapped on commit.
#[derive(Debug, Clone)]
pub struct State {
    pub tables: HashMap<String, Table>,
}

impl State {
    pub fn empty() -> State {
        State {
            tables: HashMap::new(),
        }
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, table)| table)
    }

    pub fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        let key = self
            .tables
            .keys()
            .find(|key| key.eq_ignore_ascii_case(name))?
            .clone();
        self.tables.get_mut(&key)
    }

    pub fn contains_table(&self, name: &str) -> bool {
        self.table(name).is_some()
    }

    pub fn contains_index(&self, name: &str) -> bool {
        self.tables.values().any(|table| table.has_index(name))
    }

    pub fn remove_table(&mut self, name: &str) -> Option<Table> {
        let key = self
            .tables
            .keys()
            .find(|key| key.eq_ignore_ascii_case(name))?
            .clone();
        self.tables.remove(&key)
    }

    /// Encode the catalog and row stores into a versioned, deterministic
    /// binary representation used by the page store and WAL.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = BinWriter::default();
        w.bytes(b"BSS1");
        w.u32(1);
        let mut names: Vec<&String> = self.tables.keys().collect();
        names.sort();
        w.u32(names.len() as u32);
        for name in names {
            let table = &self.tables[name];
            w.string(&table.name);
            w.u32(table.columns.len() as u32);
            for column in &table.columns {
                w.string(&column.name);
                w.u8(column_type_tag(&column.ty));
                let mut flags = 0u8;
                if column.not_null {
                    flags |= 1;
                }
                if column.unique {
                    flags |= 2;
                }
                if column.primary_key {
                    flags |= 4;
                }
                w.u8(flags);
            }
            w.u32(table.indexes.len() as u32);
            for index in &table.indexes {
                w.string(&index.name);
                w.u32(index.column as u32);
                w.u8(index.unique as u8);
            }
            w.u64(table.row_seq);
            w.u32(table.rows.len() as u32);
            for row in &table.rows {
                match row {
                    None => w.u8(0),
                    Some(values) => {
                        w.u8(1);
                        w.u32(values.len() as u32);
                        for value in values {
                            encode_value(&mut w, value);
                        }
                    }
                }
            }
        }
        w.finish()
    }

    /// Decode a snapshot and rebuild all derived indexes.  Corrupt or
    /// inconsistent data is rejected before it becomes visible to callers.
    pub(crate) fn decode(bytes: &[u8]) -> Result<State, DbError> {
        let mut r = BinReader::new(bytes);
        if r.bytes(4)? != b"BSS1" {
            return Err(dberr(
                DbErrorKind::Io("invalid state magic".into()),
                "corrupt database: invalid state magic",
            ));
        }
        if r.u32()? != 1 {
            return Err(dberr(
                DbErrorKind::Io("unsupported state version".into()),
                "corrupt database: unsupported state version",
            ));
        }
        let table_count = r.count("table")?;
        let mut tables: HashMap<String, Table> = HashMap::new();
        let mut index_names = HashSet::new();
        for _ in 0..table_count {
            let name = r.string("table name")?;
            if tables
                .keys()
                .any(|existing| existing.eq_ignore_ascii_case(&name))
            {
                return Err(dberr(
                    DbErrorKind::Io("duplicate table name".into()),
                    format!("corrupt database: duplicate table '{name}'"),
                ));
            }
            let column_count = r.count("column")?;
            let mut columns = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                let column_name = r.string("column name")?;
                let ty = column_type_from_tag(r.u8()?)?;
                let flags = r.u8()?;
                if flags & !0b111 != 0 {
                    return Err(dberr(
                        DbErrorKind::Io("invalid column flags".into()),
                        "corrupt database: invalid column flags",
                    ));
                }
                columns.push(Column {
                    name: column_name,
                    ty,
                    not_null: flags & 1 != 0,
                    unique: flags & 2 != 0,
                    primary_key: flags & 4 != 0,
                });
            }
            let index_count = r.count("index")?;
            let mut index_defs = Vec::with_capacity(index_count);
            for _ in 0..index_count {
                let index_name = r.string("index name")?;
                let column = r.u32()? as usize;
                let unique = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(dberr(
                            DbErrorKind::Io("invalid index uniqueness flag".into()),
                            "corrupt database: invalid index uniqueness flag",
                        ));
                    }
                };
                index_defs.push((index_name, column, unique));
            }
            let row_seq = r.u64()?;
            let slot_count = r.count("row slot")?;
            if row_seq != slot_count as u64 {
                return Err(dberr(
                    DbErrorKind::Io("row sequence does not match slot array".into()),
                    format!("corrupt database: invalid row sequence for table '{name}'"),
                ));
            }
            let mut table = Table::new(&name, columns)?;
            let mut live = 0usize;
            table.rows.reserve(slot_count);
            for _ in 0..slot_count {
                match r.u8()? {
                    0 => table.rows.push(None),
                    1 => {
                        let value_count = r.count("row value")?;
                        if value_count != table.columns.len() {
                            return Err(dberr(
                                DbErrorKind::Io("row width does not match table".into()),
                                format!("corrupt database: invalid row width in table '{name}'"),
                            ));
                        }
                        let mut row = Vec::with_capacity(value_count);
                        for _ in 0..value_count {
                            row.push(decode_value(&mut r)?);
                        }
                        for (idx, column) in table.columns.iter().enumerate() {
                            if (column.not_null || column.primary_key)
                                && matches!(row[idx], Value::Null)
                            {
                                return Err(dberr(
                                    DbErrorKind::Io("stored NULL violates NOT NULL".into()),
                                    format!("corrupt database: NULL in '{}'", column.name),
                                ));
                            }
                            row[idx] = row[idx].coerce_to(&column.ty).map_err(|e| {
                                dberr(
                                    DbErrorKind::Io(e.clone()),
                                    format!(
                                        "corrupt database: invalid value in '{}': {e}",
                                        column.name
                                    ),
                                )
                            })?;
                        }
                        table.rows.push(Some(row));
                        live += 1;
                    }
                    _ => {
                        return Err(dberr(
                            DbErrorKind::Io("invalid row slot marker".into()),
                            format!("corrupt database: invalid row slot in table '{name}'"),
                        ));
                    }
                }
            }
            table.row_seq = row_seq;
            table.live = live;
            table.rebuild_indexes();
            for (index_name, column, unique) in index_defs {
                if !index_names.insert(index_name.to_ascii_lowercase()) {
                    return Err(dberr(
                        DbErrorKind::Io("duplicate index name".into()),
                        format!("corrupt database: duplicate index '{index_name}'"),
                    ));
                }
                table.create_index(&index_name, column, unique)?;
            }
            table.validate_indexes()?;
            tables.insert(name, table);
        }
        if !r.at_end() {
            return Err(dberr(
                DbErrorKind::Io("trailing state bytes".into()),
                "corrupt database: trailing state bytes",
            ));
        }
        Ok(State { tables })
    }
}

/// Result payload of an executed statement.
#[derive(Debug, Clone)]
pub enum StatementResult {
    Select {
        columns: Vec<String>,
        rows: Vec<Row>,
    },
    Insert {
        rows_affected: usize,
    },
    Update {
        rows_affected: usize,
    },
    Delete {
        rows_affected: usize,
    },
    CreateTable {
        name: String,
    },
    DropTable {
        name: String,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
    },
    DropIndex {
        name: String,
    },
    Explain(String),
    Begin,
    Commit,
    Rollback,
    Checkpoint,
    Echo(String),
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    rows: Vec<Option<Row>>,
    row_seq: u64,
    live: usize,
    /// B-tree for the PRIMARY KEY column, if any.
    pk: Option<BTree>,
    /// B-trees for UNIQUE columns (column index in `columns`).
    uniques: Vec<(usize, BTree)>,
    /// User-created indexes used by the planner.
    indexes: Vec<Index>,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub column: usize,
    pub unique: bool,
    pub(crate) tree: BTree,
}

impl Table {
    pub fn new(name: &str, columns: Vec<Column>) -> Result<Table, DbError> {
        let mut seen = HashMap::new();
        let mut pk: Option<BTree> = None;
        let mut uniques = Vec::new();
        let mut primary_count = 0usize;
        for (i, c) in columns.iter().enumerate() {
            if seen.insert(c.name.to_ascii_lowercase(), ()).is_some() {
                return Err(DbError::new(
                    DbErrorKind::DuplicateColumn,
                    format!("duplicate column name '{}'", c.name),
                ));
            }
            if c.primary_key && c.ty != ColumnType::Integer {
                return Err(DbError::new(
                    DbErrorKind::Constraint,
                    "PRIMARY KEY column must be INTEGER".to_string(),
                ));
            }
            if c.primary_key {
                primary_count += 1;
                pk = Some(BTree::default());
            }
            if c.unique && !c.primary_key {
                uniques.push((i, BTree::default()));
            }
        }
        if primary_count > 1 {
            return Err(DbError::new(
                DbErrorKind::Constraint,
                "only one PRIMARY KEY column is supported",
            ));
        }
        Ok(Table {
            name: name.to_string(),
            columns,
            rows: Vec::new(),
            row_seq: 0,
            live: 0,
            pk,
            uniques,
            indexes: Vec::new(),
        })
    }

    pub fn row_count(&self) -> usize {
        self.live
    }

    pub fn column_index(&self, name: &str) -> Result<usize, DbError> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                DbError::new(
                    DbErrorKind::UnknownColumn,
                    format!("no such column: {name}"),
                )
            })
    }

    pub fn coerce_val(&self, val: &Value, col_idx: usize) -> Result<Value, DbError> {
        let ty = &self.columns[col_idx].ty;
        val.coerce_to(ty).map_err(|e| {
            DbError::new(
                DbErrorKind::TypeMismatch,
                format!("column '{}': {e}", self.columns[col_idx].name),
            )
        })
    }

    /// Allocate a fresh row id, reusing a free tombstone slot when available.
    fn alloc_slot(&mut self) -> u64 {
        if let Some((i, slot)) = self.rows.iter_mut().enumerate().find(|(_, r)| r.is_none()) {
            *slot = Some(Vec::new());
            return i as u64;
        }
        let id = self.row_seq;
        self.row_seq += 1;
        self.rows.push(Some(Vec::new()));
        id
    }

    fn pk_col_idx(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }

    /// Insert a fully-validated row value vector; maintains indexes.
    pub fn insert_row(&mut self, values: Vec<Value>) -> Result<u64, DbError> {
        if values.len() != self.columns.len() {
            return Err(DbError::new(
                DbErrorKind::ColumnCount,
                format!(
                    "column count mismatch: expected {}, got {}",
                    self.columns.len(),
                    values.len()
                ),
            ));
        }
        self.validate_row(&values, None)?;
        let rid = self.alloc_slot();
        self.rows[rid as usize] = Some(values.clone());
        self.live += 1;
        let pk_idx = self.pk_col_idx();
        if let Some(pk) = &mut self.pk {
            let pk_idx = pk_idx.expect("has pk");
            pk.insert(values[pk_idx].clone(), rid);
        }
        for (idx, tree) in &mut self.uniques {
            let k = values[*idx].clone();
            if !matches!(k, Value::Null) {
                tree.insert(k, rid);
            }
        }
        for index in &mut self.indexes {
            let key = values[index.column].clone();
            if !index.unique || !matches!(key, Value::Null) {
                index.tree.insert(key, rid);
            }
        }
        Ok(rid)
    }

    /// Remove a row by id; also removes its index entries.
    pub fn delete_row(&mut self, rid: u64) -> Result<(), DbError> {
        let idx = rid as usize;
        if idx >= self.rows.len() || self.rows[idx].is_none() {
            return Err(dberr(
                DbErrorKind::Internal(format!("no such row id {rid}")),
                format!("no such row id {rid}"),
            ));
        }
        let row = self.rows[idx].take().unwrap();
        self.live -= 1;
        if self.pk.is_some() {
            let pk_idx = self.pk_col_idx().expect("has pk");
            if let Some(pk) = self.pk.as_mut() {
                pk.delete(&row[pk_idx], rid);
            }
        }
        for (cidx, tree) in &mut self.uniques {
            let k = row[*cidx].clone();
            if !matches!(k, Value::Null) {
                tree.delete(&k, rid);
            }
        }
        for index in &mut self.indexes {
            index.tree.delete(&row[index.column], rid);
        }
        Ok(())
    }

    /// Get a row by id.
    pub fn get_row(&self, rid: u64) -> Option<&Row> {
        let idx = rid as usize;
        self.rows.get(idx).and_then(|r| r.as_ref())
    }

    /// Iterate (rid, row) over live slots in slot order.
    pub fn scan(&self) -> impl Iterator<Item = (u64, &Row)> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.as_ref().map(|row| (i as u64, row)))
    }
    /// Replace a row's values in place, removing old index entries and
    /// inserting new ones. Caller is responsible for constraint validation.
    pub fn replace_row(&mut self, rid: u64, new: Vec<Value>) -> Result<(), DbError> {
        let idx = rid as usize;
        if idx >= self.rows.len() || self.rows[idx].is_none() {
            return Err(dberr(
                DbErrorKind::Internal(format!("no such row id {rid}")),
                format!("no such row id {rid}"),
            ));
        }
        if new.len() != self.columns.len() {
            return Err(dberr(
                DbErrorKind::ColumnCount,
                format!(
                    "column count mismatch: expected {}, got {}",
                    self.columns.len(),
                    new.len()
                ),
            ));
        }
        self.validate_row(&new, Some(rid))?;
        let old = self.rows[idx].take().unwrap();
        if self.pk.is_some() {
            let pk_idx = self.pk_col_idx().expect("has pk");
            if let Some(pk) = self.pk.as_mut() {
                pk.delete(&old[pk_idx], rid);
            }
        }
        for (cidx, tree) in &mut self.uniques {
            let k = old[*cidx].clone();
            if !matches!(k, Value::Null) {
                tree.delete(&k, rid);
            }
        }
        for index in &mut self.indexes {
            index.tree.delete(&old[index.column], rid);
        }
        if self.pk.is_some() {
            let pk_idx = self.pk_col_idx().expect("has pk");
            if let Some(pk) = self.pk.as_mut() {
                pk.insert(new[pk_idx].clone(), rid);
            }
        }
        for (cidx, tree) in &mut self.uniques {
            let k = new[*cidx].clone();
            if !matches!(k, Value::Null) {
                tree.insert(k, rid);
            }
        }
        for index in &mut self.indexes {
            let key = new[index.column].clone();
            if !index.unique || !matches!(key, Value::Null) {
                index.tree.insert(key, rid);
            }
        }
        self.rows[idx] = Some(new);
        Ok(())
    }

    /// Lazy rebuild of unique/pk indexes — used when row store is bulk-loaded
    /// or after operations that bypass per-row index maintenance.
    pub fn rebuild_indexes(&mut self) {
        if self.pk.is_some() {
            self.pk = Some(BTree::default());
        }
        self.uniques = self
            .uniques
            .drain(..)
            .map(|(i, _)| (i, BTree::default()))
            .collect();
        for index in &mut self.indexes {
            index.tree = BTree::default();
        }
        let live: Vec<(u64, Row)> = self.scan().map(|(r, row)| (r, row.clone())).collect();
        for (rid, row) in live {
            if self.pk.is_some() {
                let pk_idx = self.pk_col_idx().expect("has pk");
                if let Some(pk) = self.pk.as_mut() {
                    pk.insert(row[pk_idx].clone(), rid);
                }
            }
            for (cidx, tree) in &mut self.uniques {
                let k = row[*cidx].clone();
                if !matches!(k, Value::Null) {
                    tree.insert(k, rid);
                }
            }
            for index in &mut self.indexes {
                let key = row[index.column].clone();
                if !index.unique || !matches!(key, Value::Null) {
                    index.tree.insert(key, rid);
                }
            }
        }
    }

    pub fn has_pk(&self, v: &Value) -> bool {
        self.pk
            .as_ref()
            .map(|t| !t.lookup_eq(v).is_empty())
            .unwrap_or(false)
    }

    pub fn unique_tree(&self, col_idx: usize) -> Option<&BTree> {
        self.uniques
            .iter()
            .find(|(i, _)| *i == col_idx)
            .map(|(_, t)| t)
    }

    pub fn create_index(&mut self, name: &str, column: usize, unique: bool) -> Result<(), DbError> {
        if self
            .indexes
            .iter()
            .any(|index| index.name.eq_ignore_ascii_case(name))
        {
            return Err(dberr(
                DbErrorKind::Constraint,
                format!("index '{name}' already exists"),
            ));
        }
        if column >= self.columns.len() {
            return Err(dberr(
                DbErrorKind::UnknownColumn,
                format!("no such column index: {column}"),
            ));
        }
        let mut index = Index {
            name: name.to_string(),
            column,
            unique,
            tree: BTree::default(),
        };
        for (rid, row) in self.scan() {
            let key = row[column].clone();
            if unique && !matches!(key, Value::Null) && !index.tree.lookup_eq(&key).is_empty() {
                return Err(dberr(
                    DbErrorKind::Constraint,
                    format!("UNIQUE index '{name}' has duplicate value: {key}"),
                ));
            }
            if !unique || !matches!(key, Value::Null) {
                index.tree.insert(key, rid);
            }
        }
        self.indexes.push(index);
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str) -> bool {
        if let Some(position) = self
            .indexes
            .iter()
            .position(|index| index.name.eq_ignore_ascii_case(name))
        {
            self.indexes.remove(position);
            true
        } else {
            false
        }
    }

    pub fn has_index(&self, name: &str) -> bool {
        self.indexes
            .iter()
            .any(|index| index.name.eq_ignore_ascii_case(name))
    }

    pub fn index(&self, column: usize) -> Option<&Index> {
        self.indexes.iter().find(|index| index.column == column)
    }

    /// Look up row ids through the best equality index for a column.
    pub fn lookup_eq_index(&self, column: usize, key: &Value) -> Option<(String, Vec<u64>)> {
        if self.pk_col_idx() == Some(column) {
            return self
                .pk
                .as_ref()
                .map(|tree| ("PRIMARY KEY".into(), tree.lookup_eq(key)));
        }
        if let Some((_, tree)) = self.uniques.iter().find(|(idx, _)| *idx == column) {
            return Some((
                format!("UNIQUE({})", self.columns[column].name),
                tree.lookup_eq(key),
            ));
        }
        self.index(column)
            .map(|index| (index.name.clone(), index.tree.lookup_eq(key)))
    }

    /// Return row ids in an inclusive candidate range. Strict comparison
    /// boundaries are handled by the residual WHERE predicate.
    pub fn lookup_range_index(
        &self,
        column: usize,
        low: Option<&Value>,
        high: Option<&Value>,
    ) -> Option<(String, Vec<u64>)> {
        let (name, entries) = if self.pk_col_idx() == Some(column) {
            ("PRIMARY KEY".to_string(), self.pk.as_ref()?.scan_all())
        } else if let Some((_, tree)) = self.uniques.iter().find(|(idx, _)| *idx == column) {
            (
                format!("UNIQUE({})", self.columns[column].name),
                tree.scan_all(),
            )
        } else {
            let index = self.index(column)?;
            (index.name.clone(), index.tree.scan_all())
        };
        let row_ids = entries
            .into_iter()
            .filter(|(key, _)| {
                low.map(|bound| key.cmp_value(bound) != std::cmp::Ordering::Less)
                    .unwrap_or(true)
                    && high
                        .map(|bound| key.cmp_value(bound) != std::cmp::Ordering::Greater)
                        .unwrap_or(true)
            })
            .map(|(_, row_id)| row_id)
            .collect();
        Some((name, row_ids))
    }

    fn validate_row(&self, values: &[Value], ignore_rid: Option<u64>) -> Result<(), DbError> {
        for (i, column) in self.columns.iter().enumerate() {
            if (column.not_null || column.primary_key) && matches!(values[i], Value::Null) {
                return Err(DbError::new(
                    DbErrorKind::NotNull,
                    format!("column '{}' violates NOT NULL", column.name),
                ));
            }
            if column.ty == ColumnType::Real
                && matches!(values[i], Value::Real(value) if !value.is_finite())
            {
                return Err(DbError::new(
                    DbErrorKind::TypeMismatch,
                    format!("column '{}' cannot store a non-finite REAL", column.name),
                ));
            }
        }
        if let Some(pk) = &self.pk {
            let idx = self.pk_col_idx().expect("has pk");
            if pk
                .lookup_eq(&values[idx])
                .into_iter()
                .any(|rid| Some(rid) != ignore_rid)
            {
                return Err(DbError::new(
                    DbErrorKind::Constraint,
                    format!(
                        "UNIQUE constraint failed (PRIMARY KEY): {} already exists",
                        values[idx]
                    ),
                ));
            }
        }
        for (idx, tree) in &self.uniques {
            let key = &values[*idx];
            if !matches!(key, Value::Null)
                && tree
                    .lookup_eq(key)
                    .into_iter()
                    .any(|rid| Some(rid) != ignore_rid)
            {
                return Err(DbError::new(
                    DbErrorKind::Constraint,
                    format!(
                        "UNIQUE constraint failed on '{}': {key} already exists",
                        self.columns[*idx].name
                    ),
                ));
            }
        }
        for index in &self.indexes {
            let key = &values[index.column];
            if index.unique
                && !matches!(key, Value::Null)
                && index
                    .tree
                    .lookup_eq(key)
                    .into_iter()
                    .any(|rid| Some(rid) != ignore_rid)
            {
                return Err(DbError::new(
                    DbErrorKind::Constraint,
                    format!(
                        "UNIQUE constraint failed on index '{}': {key} already exists",
                        index.name
                    ),
                ));
            }
        }
        Ok(())
    }

    fn validate_indexes(&self) -> Result<(), DbError> {
        if let Some(pk) = &self.pk
            && pk
                .scan_all()
                .windows(2)
                .any(|pair| pair[0].0.cmp_value(&pair[1].0) == std::cmp::Ordering::Equal)
        {
            return Err(dberr(
                DbErrorKind::Io("duplicate primary key".into()),
                format!(
                    "corrupt database: duplicate primary key in table '{}'",
                    self.name
                ),
            ));
        }
        for (idx, tree) in &self.uniques {
            if tree
                .scan_all()
                .windows(2)
                .any(|pair| pair[0].0.cmp_value(&pair[1].0) == std::cmp::Ordering::Equal)
            {
                return Err(dberr(
                    DbErrorKind::Io("duplicate unique value".into()),
                    format!(
                        "corrupt database: duplicate unique value in '{}.{}'",
                        self.name, self.columns[*idx].name
                    ),
                ));
            }
        }
        for index in &self.indexes {
            if index.unique
                && index
                    .tree
                    .scan_all()
                    .windows(2)
                    .any(|pair| pair[0].0.cmp_value(&pair[1].0) == std::cmp::Ordering::Equal)
            {
                return Err(dberr(
                    DbErrorKind::Io("duplicate unique index value".into()),
                    format!(
                        "corrupt database: duplicate value in index '{}.{}'",
                        self.name, index.name
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct BinWriter {
    bytes: Vec<u8>,
}

impl BinWriter {
    fn bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.bytes(value.as_bytes());
    }
}

struct BinReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BinReader<'a> {
    fn new(bytes: &'a [u8]) -> BinReader<'a> {
        BinReader { bytes, pos: 0 }
    }

    fn at_end(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], DbError> {
        let end = self.pos.checked_add(len).ok_or_else(|| {
            dberr(
                DbErrorKind::Io("state offset overflow".into()),
                format!("corrupt database: state offset overflow while reading {what}"),
            )
        })?;
        let value = self.bytes.get(self.pos..end).ok_or_else(|| {
            dberr(
                DbErrorKind::Io("truncated state".into()),
                format!("corrupt database: truncated state while reading {what}"),
            )
        })?;
        self.pos = end;
        Ok(value)
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], DbError> {
        self.take(len, "bytes")
    }

    fn u8(&mut self) -> Result<u8, DbError> {
        Ok(self.take(1, "byte")?[0])
    }

    fn u32(&mut self) -> Result<u32, DbError> {
        Ok(u32::from_le_bytes(self.take(4, "u32")?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, DbError> {
        Ok(u64::from_le_bytes(self.take(8, "u64")?.try_into().unwrap()))
    }

    fn count(&mut self, what: &str) -> Result<usize, DbError> {
        let count = self.u32()? as usize;
        if count > 1_000_000 {
            return Err(dberr(
                DbErrorKind::Io("state collection is too large".into()),
                format!("corrupt database: too many {what}s"),
            ));
        }
        Ok(count)
    }

    fn string(&mut self, what: &str) -> Result<String, DbError> {
        let len = self.u32()? as usize;
        if len > 64 * 1024 * 1024 {
            return Err(dberr(
                DbErrorKind::Io("state string is too large".into()),
                format!("corrupt database: {what} is too large"),
            ));
        }
        let bytes = self.take(len, what)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            dberr(
                DbErrorKind::Io("invalid UTF-8 in state".into()),
                format!("corrupt database: invalid UTF-8 in {what}"),
            )
        })
    }
}

fn column_type_tag(ty: &ColumnType) -> u8 {
    match ty {
        ColumnType::Integer => 0,
        ColumnType::Real => 1,
        ColumnType::Text => 2,
        ColumnType::Boolean => 3,
        ColumnType::Any => 4,
        ColumnType::Null => 5,
    }
}

fn column_type_from_tag(tag: u8) -> Result<ColumnType, DbError> {
    match tag {
        0 => Ok(ColumnType::Integer),
        1 => Ok(ColumnType::Real),
        2 => Ok(ColumnType::Text),
        3 => Ok(ColumnType::Boolean),
        4 => Ok(ColumnType::Any),
        5 => Ok(ColumnType::Null),
        _ => Err(dberr(
            DbErrorKind::Io("invalid column type".into()),
            "corrupt database: invalid column type",
        )),
    }
}

fn encode_value(w: &mut BinWriter, value: &Value) {
    match value {
        Value::Null => w.u8(0),
        Value::Integer(v) => {
            w.u8(1);
            w.u64(*v as u64);
        }
        Value::Real(v) => {
            w.u8(2);
            w.u64(v.to_bits());
        }
        Value::Text(v) => {
            w.u8(3);
            w.string(v);
        }
        Value::Boolean(v) => {
            w.u8(4);
            w.u8(*v as u8);
        }
    }
}

fn decode_value(r: &mut BinReader<'_>) -> Result<Value, DbError> {
    match r.u8()? {
        0 => Ok(Value::Null),
        1 => Ok(Value::Integer(r.u64()? as i64)),
        2 => Ok(Value::Real(f64::from_bits(r.u64()?))),
        3 => Ok(Value::Text(r.string("text value")?)),
        4 => match r.u8()? {
            0 => Ok(Value::Boolean(false)),
            1 => Ok(Value::Boolean(true)),
            _ => Err(dberr(
                DbErrorKind::Io("invalid boolean".into()),
                "corrupt database: invalid boolean value",
            )),
        },
        _ => Err(dberr(
            DbErrorKind::Io("invalid value tag".into()),
            "corrupt database: invalid value tag",
        )),
    }
}

pub fn dberr(kind: DbErrorKind, msg: impl Into<String>) -> DbError {
    DbError::new(kind, msg)
}
