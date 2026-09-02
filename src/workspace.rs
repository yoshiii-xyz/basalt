//! Portable workspace lifecycle for local structured-data workflows.
//!
//! A workspace is a directory with a versioned manifest and one Basalt
//! database. Import and export deliberately use common text formats so a
//! workspace is inspectable and recoverable without Basalt-specific tooling.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use csv::{ReaderBuilder, StringRecord, Writer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use sha2::{Digest, Sha256};

use crate::database::Database;
use crate::db::{Column, DbError, StatementResult};
use crate::engine::{ExecutionBudget, MCP_EXECUTION_WORK_LIMIT};
use crate::sql::ast::Statement;
use crate::sql::parser::parse;
use crate::types::{ColumnType, Value};

const MANIFEST_FILE: &str = "workspace.json";
const DATABASE_FILE: &str = "data.basalt";
const WORKSPACE_LOCK_FILE: &str = ".workspace.lock";
const FORMAT_VERSION: u32 = 1;
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_MCP_IMPORT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MCP_IMPORT_ROWS: usize = 10_000;
const MAX_MCP_IMPORT_COLUMNS: usize = 256;
const MAX_MCP_IMPORT_CELLS: usize = 1_000_000;
const IMPORT_BATCH_SIZE: usize = 256;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_STATEMENTS: usize = 64;
const MAX_PREVIEW_MUTATIONS: usize = 32;
const MAX_PREVIEW_ROWS: usize = 10_000;
const MAX_MCP_MUTATION_ROWS: usize = 10_000;
const MAX_MCP_DIFF_ROWS: usize = 10_000;
const MAX_MCP_EXPORT_ROWS: usize = 10_000;
const HISTORY_DIR: &str = "history";
const PLANS_DIR: &str = "plans";
const CHANGES_DIR: &str = "changes";
const SNAPSHOTS_DIR: &str = "snapshots";

pub const HELP: &str = "Basalt workspace — local, portable SQL workspaces\n\n\
Usage:\n  basalt workspace <COMMAND> [OPTIONS]\n\n\
Commands:\n  init PATH                         Create a workspace\n  inspect [--json] PATH             Show workspace metadata and schema\n  query [OPTIONS] PATH SQL          Run a read-only query\n  preview [--json] PATH SQL         Preview a write and save its plan\n  plan [--json] PATH PLAN_ID        Load a saved preview plan\n  apply [--json] PATH PLAN_ID       Apply one exact preview plan\n  history [--json] PATH             List applied and recoverable changes\n  diff [--json] PATH [CHANGE_ID]    Compare a change recovery point\n  undo [--json] PATH CHANGE_ID      Undo the latest change safely\n  import [OPTIONS] WORKSPACE SOURCE Import CSV, JSON, JSONL, or SQL\n  export [OPTIONS] WORKSPACE TABLE OUTPUT\n                                     Export CSV, JSONL, or SQL\n\n\
Import options:\n  --table NAME                      Table name (required for stdin)\n  --format csv|json|jsonl|sql       Override format inference\n  --json                            Emit a machine-readable import report\n\n\
Export options:\n  --format csv|jsonl|sql             Override format inference\n  --json                            Emit a machine-readable export report\n\n\
Query options:\n  --output table|csv|json             Result format (table by default)\n\n\
State options:\n  --json                             Emit machine-readable JSON\n\n\
SOURCE and OUTPUT may be '-' for stdin/stdout. File extensions infer formats.\n\
Imports are atomic. Workspace data stays local and uses a versioned manifest.\n";

#[derive(Debug)]
pub enum WorkspaceError {
    Usage(String),
    Invalid(String),
    Io(io::Error),
    Database(DbError),
    Json(serde_json::Error),
    Csv(csv::Error),
}

impl WorkspaceError {
    pub fn exit_code(&self) -> i32 {
        match self {
            WorkspaceError::Usage(_) => 2,
            WorkspaceError::Invalid(_)
            | WorkspaceError::Io(_)
            | WorkspaceError::Database(_)
            | WorkspaceError::Json(_)
            | WorkspaceError::Csv(_) => 1,
        }
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::Usage(message) | WorkspaceError::Invalid(message) => {
                write!(f, "{message}")
            }
            WorkspaceError::Io(error) => write!(f, "{error}"),
            WorkspaceError::Database(error) => write!(f, "{error}"),
            WorkspaceError::Json(error) => write!(f, "{error}"),
            WorkspaceError::Csv(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<io::Error> for WorkspaceError {
    fn from(error: io::Error) -> Self {
        WorkspaceError::Io(error)
    }
}

impl From<DbError> for WorkspaceError {
    fn from(error: DbError) -> Self {
        WorkspaceError::Database(error)
    }
}

impl From<serde_json::Error> for WorkspaceError {
    fn from(error: serde_json::Error) -> Self {
        WorkspaceError::Json(error)
    }
}

impl From<csv::Error> for WorkspaceError {
    fn from(error: csv::Error) -> Self {
        WorkspaceError::Csv(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceManifest {
    pub format_version: u32,
    pub database: String,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    manifest: WorkspaceManifest,
    _lock: Arc<File>,
}

impl Workspace {
    pub fn init(path: impl AsRef<Path>) -> Result<Workspace, WorkspaceError> {
        let root = path.as_ref().to_path_buf();
        if root.as_os_str().is_empty() {
            return Err(WorkspaceError::Invalid(
                "workspace path cannot be empty".to_string(),
            ));
        }
        if path_is_symlink(&root)? {
            return Err(WorkspaceError::Invalid(
                "workspace path cannot be a symbolic link".to_string(),
            ));
        }
        if root.exists() && !root.is_dir() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace path is not a directory: {}",
                root.display()
            )));
        }
        fs::create_dir_all(&root)?;
        let lock = acquire_workspace_lock(&root)?;
        let manifest_path = root.join(MANIFEST_FILE);
        let database_path = root.join(DATABASE_FILE);
        if path_is_symlink(&manifest_path)? || manifest_path.exists() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace already exists: {}",
                root.display()
            )));
        }
        if path_is_symlink(&database_path)? || database_path.exists() {
            return Err(WorkspaceError::Invalid(format!(
                "reserved database path already exists: {}",
                database_path.display()
            )));
        }

        let manifest = WorkspaceManifest {
            format_version: FORMAT_VERSION,
            database: DATABASE_FILE.to_string(),
        };
        write_new_file(&manifest_path, &manifest_bytes(&manifest)?)?;

        let database = Database::open_in_workspace(&database_path)?;
        database.checkpoint()?;
        drop(database);

        Ok(Workspace {
            root,
            manifest,
            _lock: lock,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Workspace, WorkspaceError> {
        let root = path.as_ref().to_path_buf();
        if path_is_symlink(&root)? {
            return Err(WorkspaceError::Invalid(
                "workspace path cannot be a symbolic link".to_string(),
            ));
        }
        if !root.is_dir() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace directory does not exist: {}",
                root.display()
            )));
        }
        let manifest_path = root.join(MANIFEST_FILE);
        if path_is_symlink(&manifest_path)? {
            return Err(WorkspaceError::Invalid(
                "workspace manifest cannot be a symbolic link".to_string(),
            ));
        }
        let bytes = fs::read(&manifest_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                WorkspaceError::Invalid(format!(
                    "not a Basalt workspace: missing {}",
                    manifest_path.display()
                ))
            } else {
                WorkspaceError::Io(error)
            }
        })?;
        let manifest: WorkspaceManifest = serde_json::from_slice(&bytes).map_err(|error| {
            WorkspaceError::Invalid(format!(
                "invalid workspace manifest {}: {error}",
                manifest_path.display()
            ))
        })?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(WorkspaceError::Invalid(format!(
                "unsupported workspace format version {}; expected {}",
                manifest.format_version, FORMAT_VERSION
            )));
        }
        if manifest.database != DATABASE_FILE {
            return Err(WorkspaceError::Invalid(format!(
                "workspace database must be {DATABASE_FILE:?}, got {:?}",
                manifest.database
            )));
        }
        validate_database_paths(&root)?;
        let lock = acquire_workspace_lock(&root)?;
        Ok(Workspace {
            root,
            manifest,
            _lock: lock,
        })
    }

    /// Open a workspace, creating it only when the requested path is missing.
    pub fn open_or_init(path: impl AsRef<Path>) -> Result<Workspace, WorkspaceError> {
        let path = path.as_ref().to_path_buf();
        match Self::open(&path) {
            Ok(workspace) => Ok(workspace),
            Err(_error) if !path.exists() => Self::init(path),
            Err(error) => Err(error),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &WorkspaceManifest {
        &self.manifest
    }

    pub fn database_path(&self) -> PathBuf {
        self.root.join(DATABASE_FILE)
    }

    pub fn database(&self) -> Result<Database, WorkspaceError> {
        let path = self.database_path();
        validate_database_paths(&self.root)?;
        Ok(Database::open_in_workspace(path)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataFormat {
    Csv,
    Json,
    JsonLines,
    Sql,
}

impl DataFormat {
    fn parse(value: &str) -> Result<DataFormat, WorkspaceError> {
        match value.to_ascii_lowercase().as_str() {
            "csv" => Ok(DataFormat::Csv),
            "json" => Ok(DataFormat::Json),
            "jsonl" | "ndjson" => Ok(DataFormat::JsonLines),
            "sql" => Ok(DataFormat::Sql),
            _ => Err(WorkspaceError::Usage(format!(
                "unknown format {value:?}; expected csv, json, jsonl, or sql"
            ))),
        }
    }

    fn from_path(path: &Path) -> Option<DataFormat> {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)?
            .to_ascii_lowercase();
        match extension.as_str() {
            "csv" => Some(DataFormat::Csv),
            "json" => Some(DataFormat::Json),
            "jsonl" | "ndjson" => Some(DataFormat::JsonLines),
            "sql" => Some(DataFormat::Sql),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            DataFormat::Csv => "csv",
            DataFormat::Json => "json",
            DataFormat::JsonLines => "jsonl",
            DataFormat::Sql => "sql",
        }
    }
}

#[derive(Debug)]
enum Command {
    Help,
    Init(PathBuf),
    Inspect {
        json: bool,
        workspace: PathBuf,
    },
    Query {
        workspace: PathBuf,
        sql: String,
        output: crate::cli::OutputMode,
    },
    Preview {
        workspace: PathBuf,
        sql: String,
        json: bool,
    },
    Plan {
        workspace: PathBuf,
        plan_id: String,
        json: bool,
    },
    Apply {
        workspace: PathBuf,
        plan_id: String,
        json: bool,
    },
    History {
        workspace: PathBuf,
        json: bool,
    },
    Diff {
        workspace: PathBuf,
        change_id: Option<String>,
        json: bool,
    },
    Undo {
        workspace: PathBuf,
        change_id: String,
        json: bool,
    },
    Import {
        workspace: PathBuf,
        source: PathBuf,
        table: Option<String>,
        format: Option<DataFormat>,
        json: bool,
    },
    Export {
        workspace: PathBuf,
        table: String,
        output: PathBuf,
        format: Option<DataFormat>,
        json: bool,
    },
}

pub fn run<R: Read, W: Write>(
    args: &[String],
    input: &mut R,
    output: &mut W,
) -> Result<(), WorkspaceError> {
    if args.is_empty() || args.iter().any(|arg| arg == "--help") {
        output.write_all(HELP.as_bytes())?;
        return Ok(());
    }
    let command = parse_command(args)?;
    match command {
        Command::Help => output.write_all(HELP.as_bytes())?,
        Command::Init(path) => {
            let workspace = Workspace::init(path)?;
            writeln!(output, "created workspace {}", workspace.root().display())?;
        }
        Command::Inspect { json, workspace } => {
            let workspace = Workspace::open(workspace)?;
            let database = workspace.database()?;
            let report = inspect(&workspace, &database)?;
            if json {
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                render_inspect(&report, output)?;
            }
        }
        Command::Query {
            workspace,
            sql,
            output: output_mode,
        } => {
            let workspace = Workspace::open(workspace)?;
            let statements = parse(&sql).map_err(|error| {
                WorkspaceError::Invalid(format!(
                    "query parse error at byte {}: {}",
                    error.offset, error.message
                ))
            })?;
            if statements.is_empty() || statements.iter().any(|statement| !is_read_only(statement))
            {
                return Err(WorkspaceError::Invalid(
                    "workspace query only accepts SELECT and EXPLAIN SELECT".to_string(),
                ));
            }
            let options = crate::cli::CliOptions {
                database: workspace.database_path().display().to_string(),
                actions: vec![crate::cli::InputAction::Command(sql)],
                output: output_mode,
                headers: true,
                quiet: false,
                help: false,
                version: false,
            };
            let database = workspace.database()?;
            let mut empty_input = io::Cursor::new(Vec::<u8>::new());
            crate::cli::run(&options, database, &mut empty_input, output)
                .map_err(|error| WorkspaceError::Invalid(error.to_string()))?;
        }
        Command::Preview {
            workspace,
            sql,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let plan = preview_plan(&workspace, &sql)?;
            if json {
                let report = PlanReport::from(&plan);
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                render_plan(&workspace, &plan, output)?;
            }
        }
        Command::Plan {
            workspace,
            plan_id,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let plan = load_plan(&workspace, &plan_id)?;
            if json {
                let report = PlanReport::from(&plan);
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                render_plan(&workspace, &plan, output)?;
            }
        }
        Command::Apply {
            workspace,
            plan_id,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let report = apply_plan(&workspace, &plan_id, None, None)?;
            if json {
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                render_apply(&report, output)?;
            }
        }
        Command::History { workspace, json } => {
            let workspace = Workspace::open(workspace)?;
            let entries = history(&workspace)?;
            if json {
                serde_json::to_writer_pretty(&mut *output, &entries)?;
                output.write_all(b"\n")?;
            } else {
                render_history(&entries, output)?;
            }
        }
        Command::Diff {
            workspace,
            change_id,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let report = diff(&workspace, change_id.as_deref())?;
            if json {
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                render_diff(&report, output)?;
            }
        }
        Command::Undo {
            workspace,
            change_id,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let report = undo(&workspace, &change_id)?;
            if json {
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                render_undo(&report, output)?;
            }
        }
        Command::Import {
            workspace,
            source,
            table,
            format,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let format = format
                .or_else(|| DataFormat::from_path(&source))
                .ok_or_else(|| {
                    WorkspaceError::Usage(
                        "cannot infer import format; provide --format or use a supported extension"
                            .to_string(),
                    )
                })?;
            let bytes = read_source(&source, input)?;
            let database = workspace.database()?;
            let table_name = if format == DataFormat::Sql {
                None
            } else {
                table.or_else(|| inferred_table_name(&source))
            };
            let imported = match format {
                DataFormat::Csv => import_csv(
                    &database,
                    table_name.as_deref(),
                    &bytes,
                    ImportLimits::unbounded(),
                )?,
                DataFormat::Json => import_json(
                    &database,
                    table_name.as_deref(),
                    &bytes,
                    ImportLimits::unbounded(),
                )?,
                DataFormat::JsonLines => import_json_lines(
                    &database,
                    table_name.as_deref(),
                    &bytes,
                    ImportLimits::unbounded(),
                )?,
                DataFormat::Sql => {
                    if table_name.is_some() {
                        return Err(WorkspaceError::Usage(
                            "--table is not valid for SQL imports".to_string(),
                        ));
                    }
                    import_sql(&database, &bytes)?
                }
            };
            if json {
                let report = CliImportReport {
                    operation: "import",
                    workspace: workspace.root.display().to_string(),
                    source: source.display().to_string(),
                    format: format.name().to_string(),
                    table: table_name,
                    bytes: bytes.len(),
                    summary: imported,
                };
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else {
                writeln!(
                    output,
                    "imported {}{}",
                    format.name(),
                    imported
                        .map(|summary| format!(": {summary}"))
                        .unwrap_or_default()
                )?;
            }
        }
        Command::Export {
            workspace,
            table,
            output: destination,
            format,
            json,
        } => {
            let workspace = Workspace::open(workspace)?;
            let format = format
                .or_else(|| DataFormat::from_path(&destination))
                .ok_or_else(|| {
                    WorkspaceError::Usage(
                        "cannot infer export format; provide --format or use a supported extension"
                            .to_string(),
                    )
                })?;
            if format == DataFormat::Json {
                return Err(WorkspaceError::Usage(
                    "JSON export is JSON Lines; use --format jsonl or a .jsonl file".to_string(),
                ));
            }
            if json && destination.as_os_str() == OsStr::new("-") {
                return Err(WorkspaceError::Usage(
                    "--json cannot be combined with stdout export; write the export to a file"
                        .to_string(),
                ));
            }
            let database = workspace.database()?;
            let (columns, rows) = select_table(&database, &table)?;
            let bytes = match format {
                DataFormat::Csv => export_csv(&columns, &rows)?,
                DataFormat::JsonLines => export_json_lines(&columns, &rows)?,
                DataFormat::Sql => export_sql(&database, &table, &rows)?,
                DataFormat::Json => unreachable!(),
            };
            write_output(&workspace, &destination, &bytes, output)?;
            if json {
                let report = CliExportReport {
                    operation: "export",
                    workspace: workspace.root.display().to_string(),
                    table,
                    format: format.name().to_string(),
                    output: destination.display().to_string(),
                    rows: rows.len(),
                    bytes: bytes.len(),
                };
                serde_json::to_writer_pretty(&mut *output, &report)?;
                output.write_all(b"\n")?;
            } else if destination.as_os_str() == OsStr::new("-") {
                output.flush()?;
            } else {
                writeln!(
                    output,
                    "exported {} rows to {}",
                    rows.len(),
                    destination.display()
                )?;
            }
        }
    }
    output.flush()?;
    Ok(())
}

fn parse_command(args: &[String]) -> Result<Command, WorkspaceError> {
    match args.first().map(String::as_str) {
        Some("init") => parse_init(&args[1..]),
        Some("inspect") => parse_inspect(&args[1..]),
        Some("query") => parse_query(&args[1..]),
        Some("preview") => parse_preview(&args[1..]),
        Some("plan") => parse_plan(&args[1..]),
        Some("apply") => parse_apply(&args[1..]),
        Some("history") => parse_history(&args[1..]),
        Some("diff") => parse_diff(&args[1..]),
        Some("undo") => parse_undo(&args[1..]),
        Some("import") => parse_import(&args[1..]),
        Some("export") => parse_export(&args[1..]),
        Some("--help") => Ok(Command::Help),
        Some(command) => Err(WorkspaceError::Usage(format!(
            "unknown workspace command {command:?}; run `basalt workspace --help`"
        ))),
        None => Err(WorkspaceError::Usage(
            "missing workspace command".to_string(),
        )),
    }
}

fn parse_init(args: &[String]) -> Result<Command, WorkspaceError> {
    if args.len() == 1 && args[0] != "--help" {
        return Ok(Command::Init(PathBuf::from(&args[0])));
    }
    Err(WorkspaceError::Usage(
        "usage: basalt workspace init PATH".to_string(),
    ))
}

fn parse_inspect(args: &[String]) -> Result<Command, WorkspaceError> {
    let mut json = false;
    let mut positional = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg == "--json" {
            json = true;
        } else if options && arg == "--help" {
            return Err(WorkspaceError::Usage(HELP.to_string()));
        } else if options && arg.starts_with('-') {
            return Err(WorkspaceError::Usage(format!(
                "unknown inspect option {arg:?}"
            )));
        } else {
            positional.push(arg);
        }
    }
    if positional.len() != 1 {
        return Err(WorkspaceError::Usage(
            "usage: basalt workspace inspect [--json] PATH".to_string(),
        ));
    }
    Ok(Command::Inspect {
        json,
        workspace: PathBuf::from(positional[0]),
    })
}

fn parse_query(args: &[String]) -> Result<Command, WorkspaceError> {
    let mut output = crate::cli::OutputMode::Table;
    let mut positional = Vec::new();
    let mut index = 0;
    let mut options = true;
    while index < args.len() {
        let arg = &args[index];
        if options && arg == "--" {
            options = false;
        } else if options && arg == "--json" {
            output = crate::cli::OutputMode::Json;
        } else if options && arg == "--csv" {
            output = crate::cli::OutputMode::Csv;
        } else if options && arg == "--table" {
            output = crate::cli::OutputMode::Table;
        } else if options && arg == "--output" {
            index += 1;
            output = parse_query_output(&option_value(args, index, "--output")?)?;
        } else if options && arg == "--help" {
            return Err(WorkspaceError::Usage(HELP.to_string()));
        } else if options && arg.starts_with('-') {
            return Err(WorkspaceError::Usage(format!(
                "unknown query option {arg:?}"
            )));
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    if positional.len() != 2 {
        return Err(WorkspaceError::Usage(
            "usage: basalt workspace query [OPTIONS] PATH SQL".to_string(),
        ));
    }
    Ok(Command::Query {
        workspace: PathBuf::from(&positional[0]),
        sql: positional[1].clone(),
        output,
    })
}

fn parse_query_output(value: &str) -> Result<crate::cli::OutputMode, WorkspaceError> {
    match value.to_ascii_lowercase().as_str() {
        "table" => Ok(crate::cli::OutputMode::Table),
        "csv" => Ok(crate::cli::OutputMode::Csv),
        "json" | "jsonl" | "ndjson" => Ok(crate::cli::OutputMode::Json),
        _ => Err(WorkspaceError::Usage(format!(
            "unknown query output format {value:?}; expected table, csv, or json"
        ))),
    }
}

fn parse_json_flagged_command(
    args: &[String],
    usage: &str,
    positionals: std::ops::RangeInclusive<usize>,
) -> Result<(bool, Vec<String>), WorkspaceError> {
    let mut json = false;
    let mut positional = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg == "--json" {
            json = true;
        } else if options && arg == "--help" {
            return Err(WorkspaceError::Usage(HELP.to_string()));
        } else if options && arg.starts_with('-') {
            return Err(WorkspaceError::Usage(format!("unknown option {arg:?}")));
        } else {
            positional.push(arg.clone());
        }
    }
    if !positionals.contains(&positional.len()) {
        return Err(WorkspaceError::Usage(usage.to_string()));
    }
    Ok((json, positional))
}

fn parse_preview(args: &[String]) -> Result<Command, WorkspaceError> {
    let (json, positional) = parse_json_flagged_command(
        args,
        "usage: basalt workspace preview [--json] PATH SQL",
        2..=2,
    )?;
    Ok(Command::Preview {
        workspace: PathBuf::from(&positional[0]),
        sql: positional[1].clone(),
        json,
    })
}

fn parse_apply(args: &[String]) -> Result<Command, WorkspaceError> {
    let (json, positional) = parse_json_flagged_command(
        args,
        "usage: basalt workspace apply [--json] PATH PLAN_ID",
        2..=2,
    )?;
    Ok(Command::Apply {
        workspace: PathBuf::from(&positional[0]),
        plan_id: positional[1].clone(),
        json,
    })
}

fn parse_plan(args: &[String]) -> Result<Command, WorkspaceError> {
    let (json, positional) = parse_json_flagged_command(
        args,
        "usage: basalt workspace plan [--json] PATH PLAN_ID",
        2..=2,
    )?;
    Ok(Command::Plan {
        workspace: PathBuf::from(&positional[0]),
        plan_id: positional[1].clone(),
        json,
    })
}

fn parse_history(args: &[String]) -> Result<Command, WorkspaceError> {
    let (json, positional) =
        parse_json_flagged_command(args, "usage: basalt workspace history [--json] PATH", 1..=1)?;
    Ok(Command::History {
        workspace: PathBuf::from(&positional[0]),
        json,
    })
}

fn parse_diff(args: &[String]) -> Result<Command, WorkspaceError> {
    let (json, positional) = parse_json_flagged_command(
        args,
        "usage: basalt workspace diff [--json] PATH [CHANGE_ID]",
        1..=2,
    )?;
    Ok(Command::Diff {
        workspace: PathBuf::from(&positional[0]),
        change_id: positional.get(1).cloned(),
        json,
    })
}

fn parse_undo(args: &[String]) -> Result<Command, WorkspaceError> {
    let (json, positional) = parse_json_flagged_command(
        args,
        "usage: basalt workspace undo [--json] PATH CHANGE_ID",
        2..=2,
    )?;
    Ok(Command::Undo {
        workspace: PathBuf::from(&positional[0]),
        change_id: positional[1].clone(),
        json,
    })
}

fn parse_import(args: &[String]) -> Result<Command, WorkspaceError> {
    let mut table = None;
    let mut format = None;
    let mut json = false;
    let mut positional = Vec::new();
    let mut index = 0;
    let mut options = true;
    while index < args.len() {
        let arg = &args[index];
        if options && arg == "--" {
            options = false;
        } else if options && arg == "--table" {
            index += 1;
            table = Some(option_value(args, index, "--table")?);
        } else if options && arg == "--format" {
            index += 1;
            format = Some(DataFormat::parse(&option_value(args, index, "--format")?)?);
        } else if options && arg == "--json" {
            json = true;
        } else if options && arg == "--help" {
            return Err(WorkspaceError::Usage(HELP.to_string()));
        } else if options && arg.starts_with('-') && arg != "-" {
            return Err(WorkspaceError::Usage(format!(
                "unknown import option {arg:?}"
            )));
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    if positional.len() != 2 {
        return Err(WorkspaceError::Usage(
            "usage: basalt workspace import [OPTIONS] WORKSPACE SOURCE".to_string(),
        ));
    }
    let source = PathBuf::from(&positional[1]);
    if source.as_os_str() == OsStr::new("-") && table.is_none() && format != Some(DataFormat::Sql) {
        return Err(WorkspaceError::Usage(
            "stdin imports require --table NAME".to_string(),
        ));
    }
    if format == Some(DataFormat::Sql) && table.is_some() {
        return Err(WorkspaceError::Usage(
            "--table is not valid for SQL imports".to_string(),
        ));
    }
    Ok(Command::Import {
        workspace: PathBuf::from(&positional[0]),
        source,
        table,
        format,
        json,
    })
}

fn parse_export(args: &[String]) -> Result<Command, WorkspaceError> {
    let mut format = None;
    let mut json = false;
    let mut positional = Vec::new();
    let mut index = 0;
    let mut options = true;
    while index < args.len() {
        let arg = &args[index];
        if options && arg == "--" {
            options = false;
        } else if options && arg == "--format" {
            index += 1;
            format = Some(DataFormat::parse(&option_value(args, index, "--format")?)?);
        } else if options && arg == "--json" {
            json = true;
        } else if options && arg == "--help" {
            return Err(WorkspaceError::Usage(HELP.to_string()));
        } else if options && arg.starts_with('-') && arg != "-" {
            return Err(WorkspaceError::Usage(format!(
                "unknown export option {arg:?}"
            )));
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    if positional.len() != 3 {
        return Err(WorkspaceError::Usage(
            "usage: basalt workspace export [OPTIONS] WORKSPACE TABLE OUTPUT".to_string(),
        ));
    }
    let output = PathBuf::from(&positional[2]);
    if output.as_os_str() == OsStr::new("-") && format.is_none() {
        return Err(WorkspaceError::Usage(
            "stdout exports require --format FORMAT".to_string(),
        ));
    }
    Ok(Command::Export {
        workspace: PathBuf::from(&positional[0]),
        table: positional[1].clone(),
        output,
        format,
        json,
    })
}

fn option_value(args: &[String], index: usize, option: &str) -> Result<String, WorkspaceError> {
    args.get(index)
        .cloned()
        .filter(|value| !value.starts_with('-') || value == "-")
        .ok_or_else(|| WorkspaceError::Usage(format!("{option} requires a value")))
}

fn read_source<R: Read>(source: &Path, input: &mut R) -> Result<Vec<u8>, WorkspaceError> {
    if source.as_os_str() == OsStr::new("-") {
        return read_limited(input);
    }
    let mut file = File::open(source)?;
    read_limited(&mut file)
}

fn read_limited<R: Read>(reader: &mut R) -> Result<Vec<u8>, WorkspaceError> {
    let mut limited = reader.take(MAX_IMPORT_BYTES + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(WorkspaceError::Invalid(format!(
            "input exceeds the {} MiB limit",
            MAX_IMPORT_BYTES / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

fn inferred_table_name(source: &Path) -> Option<String> {
    if source.as_os_str() == OsStr::new("-") {
        return None;
    }
    source
        .file_stem()
        .and_then(OsStr::to_str)
        .map(str::to_string)
}

#[derive(Debug, Clone)]
enum ImportedCell {
    Null,
    Empty,
    Integer(i64),
    Real(f64),
    Boolean(bool),
    Text(String),
}

#[derive(Debug, Clone)]
struct ImportedRows {
    table: String,
    columns: Vec<String>,
    types: Vec<ColumnType>,
    rows: Vec<Vec<ImportedCell>>,
}

#[derive(Debug, Clone, Copy)]
struct ImportLimits {
    max_rows: Option<usize>,
    max_columns: Option<usize>,
    max_cells: Option<usize>,
    max_work: Option<usize>,
}

impl ImportLimits {
    fn unbounded() -> Self {
        Self {
            max_rows: None,
            max_columns: None,
            max_cells: None,
            max_work: None,
        }
    }

    fn mcp() -> Self {
        Self {
            max_rows: Some(MAX_MCP_IMPORT_ROWS),
            max_columns: Some(MAX_MCP_IMPORT_COLUMNS),
            max_cells: Some(MAX_MCP_IMPORT_CELLS),
            max_work: Some(MCP_EXECUTION_WORK_LIMIT),
        }
    }
}

#[derive(Debug, Serialize)]
struct CliImportReport {
    operation: &'static str,
    workspace: String,
    source: String,
    format: String,
    table: Option<String>,
    bytes: usize,
    summary: Option<String>,
}

#[derive(Debug, Serialize)]
struct CliExportReport {
    operation: &'static str,
    workspace: String,
    table: String,
    format: String,
    output: String,
    rows: usize,
    bytes: usize,
}

fn import_csv(
    database: &Database,
    table: Option<&str>,
    bytes: &[u8],
    limits: ImportLimits,
) -> Result<Option<String>, WorkspaceError> {
    let table = required_table(table)?;
    let mut reader = ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_reader(bytes);
    let headers = reader.headers()?.clone();
    let columns = validate_headers(&headers)?;
    enforce_import_limits(0, columns.len(), limits)?;
    let mut rows = Vec::new();
    for record in reader.records() {
        if let Some(max_rows) = limits.max_rows
            && rows.len() >= max_rows
        {
            return Err(WorkspaceError::Invalid(format!(
                "MCP import is limited to {max_rows} rows; use the CLI for larger imports"
            )));
        }
        let record = record?;
        enforce_import_limits(rows.len().saturating_add(1), columns.len(), limits)?;
        rows.push(record.iter().map(parse_csv_cell).collect());
    }
    let imported = ImportedRows {
        table: table.to_string(),
        types: infer_types(&rows, columns.len()),
        columns,
        rows,
    };
    let summary = imported.summary();
    import_rows(database, &imported, limits.max_work)?;
    Ok(Some(summary))
}

fn import_json(
    database: &Database,
    table: Option<&str>,
    bytes: &[u8],
    limits: ImportLimits,
) -> Result<Option<String>, WorkspaceError> {
    let value: JsonValue = serde_json::from_slice(bytes)?;
    let objects = match value {
        JsonValue::Array(values) => values,
        JsonValue::Object(object) => vec![JsonValue::Object(object)],
        _ => {
            return Err(WorkspaceError::Invalid(
                "JSON import expects an object or an array of objects".to_string(),
            ));
        }
    };
    import_json_objects(database, table, objects, limits)
}

fn import_json_lines(
    database: &Database,
    table: Option<&str>,
    bytes: &[u8],
    limits: ImportLimits,
) -> Result<Option<String>, WorkspaceError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        WorkspaceError::Invalid(format!("JSON Lines input is not UTF-8: {error}"))
    })?;
    let mut objects = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(max_rows) = limits.max_rows
            && objects.len() >= max_rows
        {
            return Err(WorkspaceError::Invalid(format!(
                "MCP import is limited to {max_rows} rows; use the CLI for larger imports"
            )));
        }
        let value: JsonValue = serde_json::from_str(line).map_err(|error| {
            WorkspaceError::Invalid(format!("invalid JSON on line {}: {error}", line_number + 1))
        })?;
        if !value.is_object() {
            return Err(WorkspaceError::Invalid(format!(
                "JSON Lines line {} is not an object",
                line_number + 1
            )));
        }
        objects.push(value);
    }
    import_json_objects(database, table, objects, limits)
}

fn import_json_objects(
    database: &Database,
    table: Option<&str>,
    objects: Vec<JsonValue>,
    limits: ImportLimits,
) -> Result<Option<String>, WorkspaceError> {
    let table = required_table(table)?;
    let object_count = objects.len();
    if objects.is_empty() {
        return Err(WorkspaceError::Invalid(
            "JSON import contains no objects; a table schema cannot be inferred".to_string(),
        ));
    }
    enforce_import_limits(object_count, 0, limits)?;
    let mut names = BTreeSet::new();
    let mut parsed_objects = Vec::with_capacity(objects.len());
    for value in objects {
        let JsonValue::Object(object) = value else {
            return Err(WorkspaceError::Invalid(
                "JSON import expects every row to be an object".to_string(),
            ));
        };
        for name in object.keys() {
            validate_name(name, "JSON key")?;
            names.insert(name.clone());
        }
        parsed_objects.push(object);
    }
    if names.is_empty() {
        return Err(WorkspaceError::Invalid(
            "JSON objects contain no fields; a table schema cannot be inferred".to_string(),
        ));
    }
    let columns: Vec<String> = names.into_iter().collect();
    enforce_import_limits(object_count, columns.len(), limits)?;
    let rows = parsed_objects
        .iter()
        .map(|object| {
            columns
                .iter()
                .map(|name| {
                    object
                        .get(name)
                        .map(json_cell)
                        .unwrap_or(ImportedCell::Null)
                })
                .collect()
        })
        .collect::<Vec<Vec<ImportedCell>>>();
    let imported = ImportedRows {
        table: table.to_string(),
        types: infer_types(&rows, columns.len()),
        columns,
        rows,
    };
    let summary = imported.summary();
    import_rows(database, &imported, limits.max_work)?;
    Ok(Some(summary))
}

fn enforce_import_limits(
    rows: usize,
    columns: usize,
    limits: ImportLimits,
) -> Result<(), WorkspaceError> {
    if let Some(max_rows) = limits.max_rows
        && rows > max_rows
    {
        return Err(WorkspaceError::Invalid(format!(
            "MCP import is limited to {max_rows} rows; use the CLI for larger imports"
        )));
    }
    if let Some(max_columns) = limits.max_columns
        && columns > max_columns
    {
        return Err(WorkspaceError::Invalid(format!(
            "MCP import is limited to {max_columns} columns; use the CLI for wider imports"
        )));
    }
    if let Some(max_cells) = limits.max_cells {
        let cells = rows.saturating_mul(columns);
        if cells > max_cells {
            return Err(WorkspaceError::Invalid(format!(
                "MCP import is limited to {max_cells} cells; use the CLI for larger imports"
            )));
        }
    }
    Ok(())
}

fn import_sql(database: &Database, bytes: &[u8]) -> Result<Option<String>, WorkspaceError> {
    let sql = std::str::from_utf8(bytes)
        .map_err(|error| WorkspaceError::Invalid(format!("SQL input is not UTF-8: {error}")))?;
    let statements = parse(sql).map_err(|error| {
        WorkspaceError::Invalid(format!(
            "SQL import parse error at byte {}: {}",
            error.offset, error.message
        ))
    })?;
    if statements.is_empty() {
        return Err(WorkspaceError::Invalid(
            "SQL import contains no statements".to_string(),
        ));
    }
    if statements.iter().any(statement_contains_control) {
        return Err(WorkspaceError::Invalid(
            "SQL imports must not contain BEGIN, COMMIT, ROLLBACK, or CHECKPOINT".to_string(),
        ));
    }

    let mut connection = database.connect();
    connection.execute_sql("BEGIN")?;
    let result = connection.execute_sql(sql);
    match result {
        Ok(results) => {
            connection.execute_sql("COMMIT")?;
            let mutations = results
                .iter()
                .filter(|result| {
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
                })
                .count();
            Ok(Some(format!("{mutations} statements from SQL")))
        }
        Err(error) => {
            let _ = connection.execute_sql("ROLLBACK");
            Err(error.into())
        }
    }
}

fn statement_contains_control(statement: &Statement) -> bool {
    match statement {
        Statement::Begin | Statement::Commit | Statement::Rollback | Statement::Checkpoint => true,
        Statement::Explain(inner) => statement_contains_control(inner),
        _ => false,
    }
}

fn is_read_only(statement: &Statement) -> bool {
    match statement {
        Statement::Select { .. } => true,
        Statement::Explain(inner) => is_read_only(inner),
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
enum ChangeKind {
    #[serde(rename = "apply")]
    Apply,
    #[serde(rename = "undo")]
    Undo,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
enum ChangeStatus {
    #[serde(rename = "prepared")]
    Prepared,
    #[serde(rename = "committed")]
    Committed,
    #[serde(rename = "recovered")]
    Recovered,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "unresolved")]
    Unresolved,
}

impl ChangeStatus {
    fn is_committed(&self) -> bool {
        matches!(self, ChangeStatus::Committed | ChangeStatus::Recovered)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
struct PreviewItem {
    statement: usize,
    kind: String,
    mutating: bool,
    rows_affected: Option<usize>,
    rows_returned: Option<usize>,
    object: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PlanRecord {
    format_version: u32,
    plan_id: String,
    base_generation: u64,
    base_state: String,
    sql: String,
    statements: Vec<PreviewItem>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PlanReport {
    plan_id: String,
    sql: String,
    base_generation: u64,
    base_state: String,
    statement_count: usize,
    mutating_statements: usize,
    statements: Vec<PreviewItem>,
}

impl From<&PlanRecord> for PlanReport {
    fn from(plan: &PlanRecord) -> Self {
        Self {
            plan_id: plan.plan_id.clone(),
            sql: plan.sql.clone(),
            base_generation: plan.base_generation,
            base_state: plan.base_state.clone(),
            statement_count: plan.statements.len(),
            mutating_statements: plan.statements.iter().filter(|item| item.mutating).count(),
            statements: plan.statements.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportMetadata {
    request_key: String,
    format: String,
    table: String,
    bytes: usize,
    summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeRecord {
    format_version: u32,
    sequence: u64,
    change_id: String,
    kind: ChangeKind,
    plan_id: Option<String>,
    target_change_id: Option<String>,
    base_generation: u64,
    base_state: String,
    expected_state: Option<String>,
    snapshot_id: String,
    sql: Option<String>,
    status: ChangeStatus,
    committed_generation: Option<u64>,
    after_state: Option<String>,
    error: Option<String>,
    #[serde(default)]
    import: Option<ImportMetadata>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ApplyReport {
    change_id: String,
    plan_id: String,
    base_state: String,
    after_state: String,
    generation: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct HistoryEntry {
    sequence: u64,
    change_id: String,
    kind: ChangeKind,
    status: ChangeStatus,
    plan_id: Option<String>,
    target_change_id: Option<String>,
    base_state: String,
    after_state: Option<String>,
    committed_generation: Option<u64>,
    error: Option<String>,
    import: Option<HistoryImport>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct HistoryImport {
    format: String,
    table: String,
    bytes: usize,
    summary: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct DiffReport {
    change_id: String,
    kind: ChangeKind,
    precision: &'static str,
    before_state: String,
    current_state: String,
    state_changed: bool,
    tables: Vec<TableDiff>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TableDiff {
    table: String,
    before_rows: Option<usize>,
    after_rows: Option<usize>,
    schema_changed: bool,
    data_changed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct UndoReport {
    change_id: String,
    undone_change_id: String,
    restored_state: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct TableSnapshot {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

fn preview_plan(workspace: &Workspace, sql: &str) -> Result<PlanRecord, WorkspaceError> {
    preview_plan_with_output_limit(workspace, sql, None, None, None)
}

fn preview_plan_with_output_limit(
    workspace: &Workspace,
    sql: &str,
    max_output_bytes: Option<usize>,
    max_work: Option<usize>,
    max_mutation_rows: Option<usize>,
) -> Result<PlanRecord, WorkspaceError> {
    if sql.len() > MAX_PREVIEW_BYTES {
        return Err(WorkspaceError::Invalid(format!(
            "SQL exceeds the {} MiB preview limit",
            MAX_PREVIEW_BYTES / (1024 * 1024)
        )));
    }
    let statements = parse(sql).map_err(|error| {
        WorkspaceError::Invalid(format!(
            "preview parse error at byte {}: {}",
            error.offset, error.message
        ))
    })?;
    if statements.is_empty() {
        return Err(WorkspaceError::Invalid(
            "preview requires at least one SQL statement".to_string(),
        ));
    }
    if statements.len() > MAX_PREVIEW_STATEMENTS {
        return Err(WorkspaceError::Invalid(format!(
            "preview accepts at most {MAX_PREVIEW_STATEMENTS} statements"
        )));
    }
    let mutating_statements = statements
        .iter()
        .filter(|statement| is_mutation_statement(statement))
        .count();
    if mutating_statements > MAX_PREVIEW_MUTATIONS {
        return Err(WorkspaceError::Invalid(format!(
            "preview accepts at most {MAX_PREVIEW_MUTATIONS} mutating statements"
        )));
    }
    if statements.iter().any(statement_contains_control) {
        return Err(WorkspaceError::Invalid(
            "preview SQL must not contain BEGIN, COMMIT, ROLLBACK, or CHECKPOINT".to_string(),
        ));
    }
    if !statements.iter().any(is_mutation_statement) {
        return Err(WorkspaceError::Invalid(
            "preview requires at least one mutating statement".to_string(),
        ));
    }

    let database = workspace.database()?;
    let mut budget = max_work
        .map(ExecutionBudget::bounded)
        .unwrap_or_else(ExecutionBudget::unlimited);
    database.checkpoint_with_budget(&mut budget)?;
    let base_generation = database.generation();
    let base_state = state_fingerprint(&workspace.database_path())?;
    let mut connection = database.connect();
    connection.execute_with_budget(&Statement::Begin, &mut budget)?;
    let result = (|| {
        let mut items = Vec::with_capacity(statements.len());
        let mut mutation_rows = 0;
        for (index, statement) in statements.iter().enumerate() {
            let result = connection.execute_with_budget(statement, &mut budget)?;
            enforce_mutation_row_limit(&mut mutation_rows, &result, max_mutation_rows)?;
            if let StatementResult::Select { rows, .. } = &result
                && rows.len() > MAX_PREVIEW_ROWS
            {
                return Err(WorkspaceError::Invalid(format!(
                    "preview query result exceeds the {MAX_PREVIEW_ROWS}-row limit"
                )));
            }
            items.push(preview_item(index + 1, &result));
        }
        Ok::<Vec<PreviewItem>, WorkspaceError>(items)
    })();
    let rollback = connection.execute_sql("ROLLBACK");
    let preview_items = match result {
        Ok(items) => {
            rollback?;
            items
        }
        Err(error) => {
            let _ = rollback;
            return Err(error);
        }
    };
    let plan_id = plan_id_for(&base_state, sql);
    let plan = PlanRecord {
        format_version: FORMAT_VERSION,
        plan_id,
        base_generation,
        base_state,
        sql: sql.to_string(),
        statements: preview_items,
    };
    if let Some(max_output_bytes) = max_output_bytes {
        let report = PlanReport::from(&plan);
        let output_size = serde_json::to_vec(&report)?.len();
        if output_size > max_output_bytes {
            return Err(WorkspaceError::Invalid(format!(
                "workspace preview is {output_size} bytes; response limit is {max_output_bytes} bytes"
            )));
        }
    }
    // Keep the database handle live through persistence so another process
    // cannot change the state between the captured fingerprint and plan write.
    persist_plan(workspace, &plan)?;
    Ok(plan)
}

fn persist_plan(workspace: &Workspace, plan: &PlanRecord) -> Result<(), WorkspaceError> {
    ensure_history_dirs(workspace)?;
    let path = plan_path(workspace, &plan.plan_id);
    if path.exists() {
        let existing: PlanRecord = read_json(&path)?;
        if existing != *plan {
            return Err(WorkspaceError::Invalid(
                "plan identifier collision; refusing to replace an existing plan".to_string(),
            ));
        }
    } else {
        write_new_json(&path, plan)?;
    }
    Ok(())
}

fn preview_item(statement: usize, result: &StatementResult) -> PreviewItem {
    let (kind, mutating, rows_affected, rows_returned, object) = match result {
        StatementResult::Select { rows, .. } => ("select", false, None, Some(rows.len()), None),
        StatementResult::Insert { rows_affected } => {
            ("insert", true, Some(*rows_affected), None, None)
        }
        StatementResult::Update { rows_affected } => {
            ("update", true, Some(*rows_affected), None, None)
        }
        StatementResult::Delete { rows_affected } => {
            ("delete", true, Some(*rows_affected), None, None)
        }
        StatementResult::CreateTable { name } => {
            ("create_table", true, None, None, Some(name.clone()))
        }
        StatementResult::DropTable { name } => ("drop_table", true, None, None, Some(name.clone())),
        StatementResult::CreateIndex { name, .. } => {
            ("create_index", true, None, None, Some(name.clone()))
        }
        StatementResult::DropIndex { name } => ("drop_index", true, None, None, Some(name.clone())),
        StatementResult::Explain(_) => ("explain", false, None, None, None),
        StatementResult::Begin => ("begin", false, None, None, None),
        StatementResult::Commit => ("commit", false, None, None, None),
        StatementResult::Rollback => ("rollback", false, None, None, None),
        StatementResult::Checkpoint => ("checkpoint", false, None, None, None),
        StatementResult::Echo(_) => ("echo", false, None, None, None),
    };
    PreviewItem {
        statement,
        kind: kind.to_string(),
        mutating,
        rows_affected,
        rows_returned,
        object,
    }
}

fn enforce_mutation_row_limit(
    total_rows: &mut usize,
    result: &StatementResult,
    max_rows: Option<usize>,
) -> Result<(), WorkspaceError> {
    let rows = match result {
        StatementResult::Insert { rows_affected }
        | StatementResult::Update { rows_affected }
        | StatementResult::Delete { rows_affected } => *rows_affected,
        _ => return Ok(()),
    };
    *total_rows = total_rows.saturating_add(rows);
    if let Some(max_rows) = max_rows
        && *total_rows > max_rows
    {
        return Err(WorkspaceError::Invalid(format!(
            "MCP workspace mutations are limited to {max_rows} affected rows per plan; split the operation into smaller reviewed plans"
        )));
    }
    Ok(())
}

fn is_mutation_statement(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::CreateTable { .. }
            | Statement::DropTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::DropIndex { .. }
            | Statement::Insert { .. }
            | Statement::InsertSelect { .. }
            | Statement::Update { .. }
            | Statement::Delete { .. }
    )
}

fn ensure_history_dirs(workspace: &Workspace) -> Result<(), WorkspaceError> {
    let directories = history_directories(workspace);
    validate_history_dirs(workspace)?;
    for directory in directories {
        fs::create_dir_all(directory)?;
    }
    validate_history_dirs(workspace)?;
    Ok(())
}

fn history_directories(workspace: &Workspace) -> [PathBuf; 4] {
    let history = workspace.root.join(HISTORY_DIR);
    [
        history.clone(),
        history.join(PLANS_DIR),
        history.join(CHANGES_DIR),
        history.join(SNAPSHOTS_DIR),
    ]
}

fn validate_history_dirs(workspace: &Workspace) -> Result<(), WorkspaceError> {
    for directory in history_directories(workspace) {
        if path_is_symlink(&directory)? {
            return Err(WorkspaceError::Invalid(format!(
                "workspace history directory cannot be a symbolic link: {}",
                directory.display()
            )));
        }
        if directory.exists() && !directory.is_dir() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace history path is not a directory: {}",
                directory.display()
            )));
        }
    }
    Ok(())
}

fn valid_id(id: &str) -> Result<(), WorkspaceError> {
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorkspaceError::Invalid(
            "identifier must be a 64-character hexadecimal value".to_string(),
        ));
    }
    Ok(())
}

fn plan_path(workspace: &Workspace, plan_id: &str) -> PathBuf {
    workspace
        .root
        .join(HISTORY_DIR)
        .join(PLANS_DIR)
        .join(format!("{plan_id}.json"))
}

fn change_path(workspace: &Workspace, change_id: &str) -> PathBuf {
    workspace
        .root
        .join(HISTORY_DIR)
        .join(CHANGES_DIR)
        .join(format!("{change_id}.json"))
}

fn snapshot_path(workspace: &Workspace, snapshot_id: &str) -> PathBuf {
    workspace
        .root
        .join(HISTORY_DIR)
        .join(SNAPSHOTS_DIR)
        .join(format!("{snapshot_id}.basalt"))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, WorkspaceError> {
    if path_is_symlink(path)? {
        return Err(WorkspaceError::Invalid(format!(
            "workspace metadata file cannot be a symbolic link: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<(), WorkspaceError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_new_file(path, &bytes)
}

fn write_atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), WorkspaceError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write_file(path, &bytes)
}

fn state_fingerprint(path: &Path) -> Result<String, WorkspaceError> {
    if path_is_symlink(path)? {
        return Err(WorkspaceError::Invalid(format!(
            "workspace state file cannot be a symbolic link: {}",
            path.display()
        )));
    }
    Ok(format!("sha256:{}", sha256_bytes(&fs::read(path)?)))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn plan_id_for(base_state: &str, sql: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"basalt-plan-v1\0");
    hasher.update(base_state.as_bytes());
    hasher.update(b"\0");
    hasher.update(sql.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn import_change_id(base_state: &str, format: DataFormat, table: &str, content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"basalt-import-v1\0");
    hasher.update(base_state.as_bytes());
    hasher.update(b"\0");
    hasher.update(format.name().as_bytes());
    hasher.update(b"\0");
    hasher.update(table.as_bytes());
    hasher.update(b"\0");
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn import_request_key(format: DataFormat, table: &str, content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"basalt-import-request-v1\0");
    hasher.update(format.name().as_bytes());
    hasher.update(b"\0");
    hasher.update(table.as_bytes());
    hasher.update(b"\0");
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn apply_change_id(plan_id: &str, base_state: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"basalt-apply-v1\0");
    hasher.update(plan_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(base_state.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn undo_change_id(change_id: &str, base_state: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"basalt-undo-v1\0");
    hasher.update(change_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(base_state.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn next_sequence(changes: &[ChangeRecord]) -> Result<u64, WorkspaceError> {
    changes
        .iter()
        .map(|change| change.sequence)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| WorkspaceError::Invalid("change sequence exhausted".to_string()))
}

fn copy_atomic(source: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    if path_is_symlink(source)? || path_is_symlink(destination)? {
        return Err(WorkspaceError::Invalid(
            "workspace recovery files cannot be symbolic links".to_string(),
        ));
    }
    let bytes = fs::read(source)?;
    atomic_write_file(destination, &bytes)
}

fn truncate_wal(database_path: &Path) -> Result<(), WorkspaceError> {
    let mut path = database_path.as_os_str().to_os_string();
    path.push(".wal");
    let file = File::create(PathBuf::from(path))?;
    file.sync_all()?;
    Ok(())
}

fn load_plan(workspace: &Workspace, plan_id: &str) -> Result<PlanRecord, WorkspaceError> {
    valid_id(plan_id)?;
    validate_history_dirs(workspace)?;
    let path = plan_path(workspace, plan_id);
    if !path.is_file() {
        return Err(WorkspaceError::Invalid(format!(
            "plan does not exist: {plan_id}"
        )));
    }
    let plan: PlanRecord = read_json(&path)?;
    if plan.format_version != FORMAT_VERSION
        || plan.plan_id != plan_id
        || plan_id_for(&plan.base_state, &plan.sql) != plan.plan_id
    {
        return Err(WorkspaceError::Invalid(format!(
            "plan is invalid or has been modified: {plan_id}"
        )));
    }
    Ok(plan)
}

fn load_changes(workspace: &Workspace) -> Result<Vec<ChangeRecord>, WorkspaceError> {
    validate_history_dirs(workspace)?;
    let directory = workspace.root.join(HISTORY_DIR).join(CHANGES_DIR);
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut changes = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if path_is_symlink(&path)? {
            return Err(WorkspaceError::Invalid(format!(
                "workspace history record cannot be a symbolic link: {}",
                path.display()
            )));
        }
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        if !path.is_file() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace history record is not a file: {}",
                path.display()
            )));
        }
        let change: ChangeRecord = read_json(&path)?;
        valid_id(&change.change_id)?;
        if path.file_stem() != Some(OsStr::new(&change.change_id)) {
            return Err(WorkspaceError::Invalid(format!(
                "change filename does not match its identifier: {}",
                path.display()
            )));
        }
        if change.format_version != FORMAT_VERSION {
            return Err(WorkspaceError::Invalid(format!(
                "unsupported change format in {}",
                path.display()
            )));
        }
        changes.push(change);
    }
    changes.sort_by_key(|change| change.sequence);
    Ok(changes)
}

fn reconcile_change(
    change: &mut ChangeRecord,
    current_state: &str,
    current_generation: u64,
) -> bool {
    if change.status != ChangeStatus::Prepared {
        return false;
    }
    match change.kind {
        ChangeKind::Apply => {
            if current_generation == change.base_generation.saturating_add(1)
                && current_state != change.base_state
            {
                change.status = ChangeStatus::Recovered;
                change.committed_generation = Some(current_generation);
                change.after_state = Some(current_state.to_string());
                change.error =
                    Some("commit completed before the history record was finalized".to_string());
            } else if current_generation == change.base_generation
                && current_state == change.base_state
            {
                change.status = ChangeStatus::Failed;
                change.error =
                    Some("operation was not observed as committed after interruption".to_string());
            } else {
                change.status = ChangeStatus::Unresolved;
                change.error = Some(
                    "workspace state does not match either side of the prepared operation"
                        .to_string(),
                );
            }
        }
        ChangeKind::Undo => {
            if change.expected_state.as_deref() == Some(current_state) {
                change.status = ChangeStatus::Recovered;
                change.committed_generation = Some(current_generation);
                change.after_state = Some(current_state.to_string());
                change.error =
                    Some("restore completed before the history record was finalized".to_string());
            } else if current_generation == change.base_generation
                && current_state == change.base_state
            {
                change.status = ChangeStatus::Failed;
                change.error =
                    Some("restore was not observed as completed after interruption".to_string());
            } else {
                change.status = ChangeStatus::Unresolved;
                change.error = Some(
                    "workspace state does not match either side of the prepared restore"
                        .to_string(),
                );
            }
        }
    }
    true
}

fn reconcile_changes(
    workspace: &Workspace,
    changes: &mut [ChangeRecord],
    current_state: &str,
    current_generation: u64,
) -> Result<(), WorkspaceError> {
    for change in changes {
        if reconcile_change(change, current_state, current_generation) {
            write_atomic_json(&change_path(workspace, &change.change_id), change)?;
        }
    }
    Ok(())
}

fn history(workspace: &Workspace) -> Result<Vec<HistoryEntry>, WorkspaceError> {
    let database = workspace.database()?;
    let current_state = state_fingerprint(&workspace.database_path())?;
    let current_generation = database.generation();
    let mut changes = load_changes(workspace)?;
    reconcile_changes(workspace, &mut changes, &current_state, current_generation)?;
    Ok(changes
        .into_iter()
        .map(|change| {
            let import = change.import.map(|import| HistoryImport {
                format: import.format,
                table: import.table,
                bytes: import.bytes,
                summary: import.summary,
            });
            HistoryEntry {
                sequence: change.sequence,
                change_id: change.change_id,
                kind: change.kind,
                status: change.status,
                plan_id: change.plan_id,
                target_change_id: change.target_change_id,
                base_state: change.base_state,
                after_state: change.after_state,
                committed_generation: change.committed_generation,
                error: change.error,
                import,
            }
        })
        .collect())
}

fn latest_committed(changes: &[ChangeRecord]) -> Option<&ChangeRecord> {
    changes
        .iter()
        .filter(|change| change.status.is_committed())
        .max_by_key(|change| change.sequence)
}

fn apply_plan(
    workspace: &Workspace,
    requested_plan_id: &str,
    max_work: Option<usize>,
    max_mutation_rows: Option<usize>,
) -> Result<ApplyReport, WorkspaceError> {
    let plan = load_plan(workspace, requested_plan_id)?;
    if !plan.statements.iter().any(|item| item.mutating) {
        return Err(WorkspaceError::Invalid(
            "plan does not contain a mutating statement".to_string(),
        ));
    }
    ensure_history_dirs(workspace)?;
    let database = workspace.database()?;
    let mut budget = max_work
        .map(ExecutionBudget::bounded)
        .unwrap_or_else(ExecutionBudget::unlimited);
    database.checkpoint_with_budget(&mut budget)?;
    let current_state = state_fingerprint(&workspace.database_path())?;
    let change_id = apply_change_id(&plan.plan_id, &plan.base_state);
    let change_file = change_path(workspace, &change_id);
    let mut changes = load_changes(workspace)?;
    if let Some(existing) = changes
        .iter_mut()
        .find(|change| change.change_id == change_id)
    {
        if reconcile_change(existing, &current_state, database.generation()) {
            write_atomic_json(&change_file, existing)?;
        }
        if existing.status.is_committed() {
            if existing.after_state.as_deref() == Some(current_state.as_str()) {
                return Ok(ApplyReport {
                    change_id,
                    plan_id: plan.plan_id,
                    base_state: existing.base_state.clone(),
                    after_state: existing.after_state.clone().ok_or_else(|| {
                        WorkspaceError::Invalid(
                            "committed plan is missing its after-state".to_string(),
                        )
                    })?,
                    generation: existing.committed_generation.ok_or_else(|| {
                        WorkspaceError::Invalid(
                            "committed plan is missing its generation".to_string(),
                        )
                    })?,
                });
            }
            return Err(WorkspaceError::Invalid(format!(
                "plan has already been applied as change {change_id}; workspace state moved, so it will not be replayed"
            )));
        }
        if existing.status == ChangeStatus::Unresolved {
            return Err(WorkspaceError::Invalid(format!(
                "change {change_id} is unresolved; inspect its recovery point before continuing"
            )));
        }
    }
    if current_state != plan.base_state {
        return Err(WorkspaceError::Invalid(
            "plan is stale; preview the operation again against the current workspace".to_string(),
        ));
    }
    let snapshot = snapshot_path(workspace, &change_id);
    if snapshot.exists() {
        if state_fingerprint(&snapshot)? != plan.base_state {
            return Err(WorkspaceError::Invalid(format!(
                "recovery point for change {change_id} does not match the plan"
            )));
        }
    } else {
        copy_atomic(&workspace.database_path(), &snapshot)?;
    }
    let sequence = changes
        .iter()
        .find(|change| change.change_id == change_id)
        .map(|change| change.sequence)
        .unwrap_or(next_sequence(&changes)?);
    let mut change = ChangeRecord {
        format_version: FORMAT_VERSION,
        sequence,
        change_id: change_id.clone(),
        kind: ChangeKind::Apply,
        plan_id: Some(plan.plan_id.clone()),
        target_change_id: None,
        base_generation: plan.base_generation,
        base_state: plan.base_state.clone(),
        expected_state: None,
        snapshot_id: change_id.clone(),
        sql: Some(plan.sql.clone()),
        status: ChangeStatus::Prepared,
        committed_generation: None,
        after_state: None,
        error: None,
        import: None,
    };
    if change_file.exists() {
        write_atomic_json(&change_file, &change)?;
    } else {
        write_new_json(&change_file, &change)?;
    }

    let mut connection = database.connect();
    if let Err(error) = connection.execute_with_budget(&Statement::Begin, &mut budget) {
        change.status = ChangeStatus::Failed;
        change.error = Some(error.to_string());
        write_atomic_json(&change_file, &change)?;
        return Err(error.into());
    }
    let execution = if max_work.is_some() {
        connection.execute_sql_using_budget(&plan.sql, &mut budget)
    } else {
        connection.execute_sql(&plan.sql)
    };
    let results = match execution {
        Ok(results) => results,
        Err(error) => {
            let _ = connection.execute_sql("ROLLBACK");
            change.status = ChangeStatus::Failed;
            change.error = Some(error.to_string());
            write_atomic_json(&change_file, &change)?;
            return Err(error.into());
        }
    };
    let mut mutation_rows = 0;
    for result in &results {
        if let Err(error) =
            enforce_mutation_row_limit(&mut mutation_rows, result, max_mutation_rows)
        {
            let _ = connection.execute_sql("ROLLBACK");
            change.status = ChangeStatus::Failed;
            change.error = Some(error.to_string());
            write_atomic_json(&change_file, &change)?;
            return Err(error);
        }
    }
    if let Err(error) = connection.execute_with_budget(&Statement::Commit, &mut budget) {
        change.status = ChangeStatus::Unresolved;
        change.error = Some(error.to_string());
        write_atomic_json(&change_file, &change)?;
        return Err(error.into());
    }
    drop(connection);
    if let Err(error) = database.checkpoint() {
        change.status = ChangeStatus::Unresolved;
        change.error = Some(format!(
            "operation committed but checkpoint failed: {error}"
        ));
        write_atomic_json(&change_file, &change)?;
        return Err(error.into());
    }
    let after_state = state_fingerprint(&workspace.database_path())?;
    let generation = database.generation();
    if std::env::var_os("BASALT_CRASH_TEST_AFTER_APPLY_CHECKPOINT").is_some() {
        std::process::abort();
    }
    change.status = ChangeStatus::Committed;
    change.committed_generation = Some(generation);
    change.after_state = Some(after_state.clone());
    write_atomic_json(&change_file, &change)?;
    Ok(ApplyReport {
        change_id,
        plan_id: plan.plan_id,
        base_state: plan.base_state,
        after_state,
        generation,
    })
}

fn diff(
    workspace: &Workspace,
    requested_change_id: Option<&str>,
) -> Result<DiffReport, WorkspaceError> {
    diff_with_row_limit(workspace, requested_change_id, None)
}

fn diff_with_row_limit(
    workspace: &Workspace,
    requested_change_id: Option<&str>,
    max_total_rows: Option<usize>,
) -> Result<DiffReport, WorkspaceError> {
    let mut changes = load_changes(workspace)?;
    let database = workspace.database()?;
    let current_state = state_fingerprint(&workspace.database_path())?;
    let current_generation = database.generation();
    reconcile_changes(workspace, &mut changes, &current_state, current_generation)?;
    let change = match requested_change_id {
        Some(change_id) => {
            valid_id(change_id)?;
            changes
                .iter()
                .find(|change| change.change_id == change_id)
                .cloned()
                .ok_or_else(|| {
                    WorkspaceError::Invalid(format!("change does not exist: {change_id}"))
                })?
        }
        None => latest_committed(&changes)
            .cloned()
            .ok_or_else(|| WorkspaceError::Invalid("no committed changes to diff".to_string()))?,
    };
    if !change.status.is_committed() {
        return Err(WorkspaceError::Invalid(format!(
            "change {} is not committed; status is {:?}",
            change.change_id, change.status
        )));
    }
    let snapshot = snapshot_path(workspace, &change.snapshot_id);
    if !snapshot.is_file() {
        return Err(WorkspaceError::Invalid(format!(
            "recovery point is missing for change {}",
            change.change_id
        )));
    }
    let before_database = Database::open(&snapshot)?;
    let before = logical_snapshot(&before_database, max_total_rows)?;
    let after = logical_snapshot(&database, max_total_rows)?;
    let mut names = BTreeSet::new();
    names.extend(before.keys().cloned());
    names.extend(after.keys().cloned());
    let tables = names
        .into_iter()
        .filter_map(|name| {
            let before_table = before.get(&name);
            let after_table = after.get(&name);
            let schema_changed = match (before_table, after_table) {
                (Some(before), Some(after)) => before.columns != after.columns,
                (None, None) => false,
                _ => true,
            };
            let data_changed = match (before_table, after_table) {
                (Some(before), Some(after)) => before.rows != after.rows,
                (None, None) => false,
                _ => true,
            };
            (schema_changed || data_changed).then(|| TableDiff {
                table: name,
                before_rows: before_table.map(|table| table.rows.len()),
                after_rows: after_table.map(|table| table.rows.len()),
                schema_changed,
                data_changed,
            })
        })
        .collect::<Vec<_>>();
    Ok(DiffReport {
        change_id: change.change_id,
        kind: change.kind,
        precision: "table-level logical comparison",
        before_state: change.base_state,
        current_state,
        state_changed: !tables.is_empty(),
        tables,
    })
}

fn logical_snapshot(
    database: &Database,
    max_total_rows: Option<usize>,
) -> Result<BTreeMap<String, TableSnapshot>, WorkspaceError> {
    let mut tables = BTreeMap::new();
    let mut total_rows = 0usize;
    for table in database.table_names()? {
        let row_count = database.row_count(&table)?;
        if let Some(max_total_rows) = max_total_rows {
            total_rows = total_rows.saturating_add(row_count);
            if total_rows > max_total_rows {
                return Err(WorkspaceError::Invalid(format!(
                    "MCP diff is limited to {max_total_rows} rows across a compared database; use the CLI diff for larger workspaces"
                )));
            }
        }
        let (columns, rows) = select_table(database, &table)?;
        debug_assert_eq!(rows.len(), row_count);
        tables.insert(table, TableSnapshot { columns, rows });
    }
    Ok(tables)
}

fn undo(workspace: &Workspace, requested_change_id: &str) -> Result<UndoReport, WorkspaceError> {
    valid_id(requested_change_id)?;
    ensure_history_dirs(workspace)?;
    let mut changes = load_changes(workspace)?;
    let database = workspace.database()?;
    database.checkpoint()?;
    let current_state = state_fingerprint(&workspace.database_path())?;
    let current_generation = database.generation();
    reconcile_changes(workspace, &mut changes, &current_state, current_generation)?;
    if let Some(existing) = changes.iter().find(|change| {
        change.kind == ChangeKind::Undo
            && change.target_change_id.as_deref() == Some(requested_change_id)
            && change.status.is_committed()
    }) {
        if existing.after_state.as_deref() == Some(current_state.as_str()) {
            return Ok(UndoReport {
                change_id: existing.change_id.clone(),
                undone_change_id: requested_change_id.to_string(),
                restored_state: existing.after_state.clone().ok_or_else(|| {
                    WorkspaceError::Invalid(
                        "committed undo is missing its restored state".to_string(),
                    )
                })?,
                generation: existing.committed_generation.ok_or_else(|| {
                    WorkspaceError::Invalid("committed undo is missing its generation".to_string())
                })?,
            });
        }
        return Err(WorkspaceError::Invalid(format!(
            "change {requested_change_id} has already been undone as {}; workspace state moved, so it will not be replayed",
            existing.change_id
        )));
    }
    let target = changes
        .iter()
        .find(|change| change.change_id == requested_change_id)
        .cloned()
        .ok_or_else(|| {
            WorkspaceError::Invalid(format!("change does not exist: {requested_change_id}"))
        })?;
    if !target.status.is_committed() {
        return Err(WorkspaceError::Invalid(format!(
            "change {} is not committed; status is {:?}",
            target.change_id, target.status
        )));
    }
    let latest = latest_committed(&changes).ok_or_else(|| {
        WorkspaceError::Invalid("there are no committed changes to undo".to_string())
    })?;
    if latest.change_id != target.change_id {
        return Err(WorkspaceError::Invalid(
            "only the latest committed change can be undone; undo later changes first".to_string(),
        ));
    }
    if target.after_state.as_deref() != Some(current_state.as_str()) {
        return Err(WorkspaceError::Invalid(
            "workspace state moved after this change; refusing to discard later work".to_string(),
        ));
    }
    let target_snapshot = snapshot_path(workspace, &target.snapshot_id);
    if !target_snapshot.is_file() {
        return Err(WorkspaceError::Invalid(format!(
            "recovery point is missing for change {}",
            target.change_id
        )));
    }
    if state_fingerprint(&target_snapshot)? != target.base_state {
        return Err(WorkspaceError::Invalid(format!(
            "recovery point for change {} failed integrity verification",
            target.change_id
        )));
    }
    let undo_id = undo_change_id(&target.change_id, &current_state);
    let undo_file = change_path(workspace, &undo_id);
    if let Some(existing) = changes.iter().find(|change| change.change_id == undo_id) {
        if existing.status.is_committed() {
            return Err(WorkspaceError::Invalid(format!(
                "change has already been undone as {undo_id}"
            )));
        }
        if existing.status == ChangeStatus::Unresolved {
            return Err(WorkspaceError::Invalid(format!(
                "undo change {undo_id} is unresolved; inspect its recovery point first"
            )));
        }
    }
    let undo_snapshot = snapshot_path(workspace, &undo_id);
    if undo_snapshot.exists() {
        if state_fingerprint(&undo_snapshot)? != current_state {
            return Err(WorkspaceError::Invalid(format!(
                "recovery point for undo {undo_id} does not match the current state"
            )));
        }
    } else {
        copy_atomic(&workspace.database_path(), &undo_snapshot)?;
    }
    let sequence = changes
        .iter()
        .find(|change| change.change_id == undo_id)
        .map(|change| change.sequence)
        .unwrap_or(next_sequence(&changes)?);
    let mut undo_record = ChangeRecord {
        format_version: FORMAT_VERSION,
        sequence,
        change_id: undo_id.clone(),
        kind: ChangeKind::Undo,
        plan_id: None,
        target_change_id: Some(target.change_id.clone()),
        base_generation: current_generation,
        base_state: current_state.clone(),
        expected_state: Some(target.base_state.clone()),
        snapshot_id: undo_id.clone(),
        sql: None,
        status: ChangeStatus::Prepared,
        committed_generation: None,
        after_state: None,
        error: None,
        import: None,
    };
    if undo_file.exists() {
        write_atomic_json(&undo_file, &undo_record)?;
    } else {
        write_new_json(&undo_file, &undo_record)?;
    }
    let database_path = workspace.database_path();
    drop(database);
    if let Err(error) = truncate_wal(&database_path) {
        undo_record.status = ChangeStatus::Unresolved;
        undo_record.error = Some(error.to_string());
        write_atomic_json(&undo_file, &undo_record)?;
        return Err(error);
    }
    if let Err(error) = copy_atomic(&target_snapshot, &database_path) {
        undo_record.status = ChangeStatus::Unresolved;
        undo_record.error = Some(error.to_string());
        write_atomic_json(&undo_file, &undo_record)?;
        return Err(error);
    }
    let restored = match Database::open_in_workspace(&database_path) {
        Ok(database) => database,
        Err(error) => {
            undo_record.status = ChangeStatus::Unresolved;
            undo_record.error = Some(error.to_string());
            write_atomic_json(&undo_file, &undo_record)?;
            return Err(error.into());
        }
    };
    if let Err(error) = restored.checkpoint() {
        undo_record.status = ChangeStatus::Unresolved;
        undo_record.error = Some(error.to_string());
        drop(restored);
        write_atomic_json(&undo_file, &undo_record)?;
        return Err(error.into());
    }
    let restored_state = state_fingerprint(&database_path)?;
    let restored_generation = restored.generation();
    if std::env::var_os("BASALT_CRASH_TEST_AFTER_UNDO_RESTORE").is_some() {
        std::process::abort();
    }
    drop(restored);
    undo_record.status = ChangeStatus::Committed;
    undo_record.committed_generation = Some(restored_generation);
    undo_record.after_state = Some(restored_state.clone());
    write_atomic_json(&undo_file, &undo_record)?;
    Ok(UndoReport {
        change_id: undo_id,
        undone_change_id: target.change_id,
        restored_state,
        generation: restored_generation,
    })
}

pub(crate) fn mcp_preview(
    workspace: &Workspace,
    sql: &str,
    max_output_bytes: usize,
) -> Result<PlanReport, WorkspaceError> {
    let plan = preview_plan_with_output_limit(
        workspace,
        sql,
        Some(max_output_bytes),
        Some(MCP_EXECUTION_WORK_LIMIT),
        Some(MAX_MCP_MUTATION_ROWS),
    )?;
    Ok(PlanReport::from(&plan))
}

pub(crate) fn mcp_plan(
    workspace: &Workspace,
    plan_id: &str,
    max_output_bytes: usize,
) -> Result<PlanReport, WorkspaceError> {
    let plan = load_plan(workspace, plan_id)?;
    let report = PlanReport::from(&plan);
    let output_size = serde_json::to_vec(&report)?.len();
    if output_size > max_output_bytes {
        return Err(WorkspaceError::Invalid(format!(
            "workspace plan is {output_size} bytes; response limit is {max_output_bytes} bytes"
        )));
    }
    Ok(report)
}

pub(crate) fn mcp_apply(
    workspace: &Workspace,
    plan_id: &str,
) -> Result<ApplyReport, WorkspaceError> {
    apply_plan(
        workspace,
        plan_id,
        Some(MCP_EXECUTION_WORK_LIMIT),
        Some(MAX_MCP_MUTATION_ROWS),
    )
}

pub(crate) fn mcp_history(workspace: &Workspace) -> Result<Vec<HistoryEntry>, WorkspaceError> {
    history(workspace)
}

pub(crate) fn mcp_diff(
    workspace: &Workspace,
    change_id: Option<&str>,
) -> Result<DiffReport, WorkspaceError> {
    diff_with_row_limit(workspace, change_id, Some(MAX_MCP_DIFF_ROWS))
}

pub(crate) fn mcp_undo(
    workspace: &Workspace,
    change_id: &str,
) -> Result<UndoReport, WorkspaceError> {
    undo(workspace, change_id)
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ImportReport {
    change_id: String,
    format: String,
    table: String,
    bytes: usize,
    summary: String,
    base_state: String,
    after_state: String,
    generation: u64,
}

pub(crate) fn mcp_import(
    workspace: &Workspace,
    table: Option<&str>,
    format: &str,
    content: &str,
) -> Result<ImportReport, WorkspaceError> {
    if content.len() > MAX_MCP_IMPORT_BYTES {
        return Err(WorkspaceError::Invalid(format!(
            "MCP import content exceeds the {} MiB limit",
            MAX_MCP_IMPORT_BYTES / (1024 * 1024)
        )));
    }
    let format = DataFormat::parse(format)?;
    if format == DataFormat::Sql {
        return Err(WorkspaceError::Usage(
            "workspace_import accepts csv, json, or jsonl; use the CLI for SQL dump imports"
                .to_string(),
        ));
    }
    let table = required_mcp_table(table)?;
    let bytes = content.as_bytes();

    // Parse and type-check before touching the durable workspace. This keeps a
    // malformed agent payload from creating a failed history record.
    let validated_summary = validate_mcp_import(format, table, bytes, ImportLimits::mcp())?;

    let database = workspace.database()?;
    let mut preflight_budget = ExecutionBudget::bounded(MCP_EXECUTION_WORK_LIMIT);
    database.checkpoint_with_budget(&mut preflight_budget)?;
    let base_state = state_fingerprint(&workspace.database_path())?;
    let base_generation = database.generation();
    ensure_history_dirs(workspace)?;
    let mut changes = load_changes(workspace)?;
    reconcile_changes(workspace, &mut changes, &base_state, base_generation)?;

    let request_key = import_request_key(format, table, bytes);
    if let Some(existing) = changes.iter().rev().find(|change| {
        change
            .import
            .as_ref()
            .is_some_and(|import| import.request_key == request_key)
    }) && existing.status.is_committed()
    {
        if existing.after_state.as_deref() != Some(base_state.as_str()) {
            return Err(WorkspaceError::Invalid(format!(
                "import has already been committed as {}; workspace state moved, so it will not be replayed",
                existing.change_id
            )));
        }
        let import = existing.import.as_ref().ok_or_else(|| {
            WorkspaceError::Invalid("committed import is missing its metadata".to_string())
        })?;
        let after_state = existing.after_state.clone().ok_or_else(|| {
            WorkspaceError::Invalid("committed import is missing its after-state".to_string())
        })?;
        let generation = existing.committed_generation.ok_or_else(|| {
            WorkspaceError::Invalid("committed import is missing its generation".to_string())
        })?;
        return Ok(ImportReport {
            change_id: existing.change_id.clone(),
            format: import.format.clone(),
            table: import.table.clone(),
            bytes: import.bytes,
            summary: import.summary.clone(),
            base_state: existing.base_state.clone(),
            after_state,
            generation,
        });
    }

    let change_id = import_change_id(&base_state, format, table, bytes);
    let retry_sequence = if let Some(existing) =
        changes.iter().find(|change| change.change_id == change_id)
    {
        if existing.status.is_committed() {
            return Err(WorkspaceError::Invalid(format!(
                "import has already been committed as change {change_id}"
            )));
        }
        if existing.status == ChangeStatus::Failed
            && existing.base_state == base_state
            && existing.after_state.is_none()
            && existing
                .import
                .as_ref()
                .is_some_and(|import| import.request_key == request_key)
        {
            Some(existing.sequence)
        } else {
            return Err(WorkspaceError::Invalid(format!(
                "import already has a history record with status {:?}; inspect workspace_history before retrying",
                existing.status
            )));
        }
    } else {
        None
    };

    let snapshot = snapshot_path(workspace, &change_id);
    if snapshot.exists() {
        if state_fingerprint(&snapshot)? != base_state {
            return Err(WorkspaceError::Invalid(format!(
                "recovery point for import {change_id} does not match the workspace"
            )));
        }
    } else {
        copy_atomic(&workspace.database_path(), &snapshot)?;
    }

    let change_file = change_path(workspace, &change_id);
    let mut change = ChangeRecord {
        format_version: FORMAT_VERSION,
        sequence: match retry_sequence {
            Some(sequence) => sequence,
            None => next_sequence(&changes)?,
        },
        change_id: change_id.clone(),
        kind: ChangeKind::Apply,
        plan_id: None,
        target_change_id: None,
        base_generation,
        base_state: base_state.clone(),
        expected_state: None,
        snapshot_id: change_id.clone(),
        sql: None,
        status: ChangeStatus::Prepared,
        committed_generation: None,
        after_state: None,
        error: None,
        import: Some(ImportMetadata {
            request_key,
            format: format.name().to_string(),
            table: table.to_string(),
            bytes: bytes.len(),
            summary: validated_summary,
        }),
    };
    if change_file.exists() {
        write_atomic_json(&change_file, &change)?;
    } else {
        write_new_json(&change_file, &change)?;
    }

    let imported = match format {
        DataFormat::Csv => import_csv(&database, Some(table), bytes, ImportLimits::mcp()),
        DataFormat::Json => import_json(&database, Some(table), bytes, ImportLimits::mcp()),
        DataFormat::JsonLines => {
            import_json_lines(&database, Some(table), bytes, ImportLimits::mcp())
        }
        DataFormat::Sql => unreachable!(),
    };
    let summary = match imported {
        Ok(summary) => summary.unwrap_or_else(|| "import completed".to_string()),
        Err(error) => {
            change.status = ChangeStatus::Failed;
            change.error = Some(error.to_string());
            write_atomic_json(&change_file, &change)?;
            return Err(error);
        }
    };

    // The bounded import transaction has already charged the resulting state
    // before publishing it. This checkpoint canonicalizes that bounded state
    // for the recovery record.
    if let Err(error) = database.checkpoint() {
        change.status = ChangeStatus::Unresolved;
        change.error = Some(format!("import committed but checkpoint failed: {error}"));
        write_atomic_json(&change_file, &change)?;
        return Err(error.into());
    }
    let after_state = state_fingerprint(&workspace.database_path())?;
    let generation = database.generation();
    if std::env::var_os("BASALT_CRASH_TEST_AFTER_IMPORT_CHECKPOINT").is_some() {
        std::process::abort();
    }
    change.status = ChangeStatus::Committed;
    change.committed_generation = Some(generation);
    change.after_state = Some(after_state.clone());
    write_atomic_json(&change_file, &change)?;

    Ok(ImportReport {
        change_id,
        format: format.name().to_string(),
        table: table.to_string(),
        bytes: bytes.len(),
        summary,
        base_state,
        after_state,
        generation,
    })
}

fn required_mcp_table(table: Option<&str>) -> Result<&str, WorkspaceError> {
    let table = table.ok_or_else(|| {
        WorkspaceError::Usage("workspace_import requires an explicit table name".to_string())
    })?;
    validate_name(table, "table name")?;
    Ok(table)
}

fn validate_mcp_import(
    format: DataFormat,
    table: &str,
    bytes: &[u8],
    limits: ImportLimits,
) -> Result<String, WorkspaceError> {
    let database = Database::in_memory();
    match format {
        DataFormat::Csv => import_csv(&database, Some(table), bytes, limits),
        DataFormat::Json => import_json(&database, Some(table), bytes, limits),
        DataFormat::JsonLines => import_json_lines(&database, Some(table), bytes, limits),
        DataFormat::Sql => unreachable!(),
    }
    .map(|summary| summary.unwrap_or_else(|| "import completed".to_string()))
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ExportReport {
    table: String,
    format: String,
    content: String,
    bytes: usize,
}

pub(crate) fn mcp_export(
    workspace: &Workspace,
    table: &str,
    format: &str,
    max_content_bytes: usize,
) -> Result<ExportReport, WorkspaceError> {
    let format = DataFormat::parse(format)?;
    if format == DataFormat::Json {
        return Err(WorkspaceError::Usage(
            "JSON export is JSON Lines; use jsonl".to_string(),
        ));
    }
    let database = workspace.database()?;
    let row_count = database.row_count(table)?;
    if row_count > MAX_MCP_EXPORT_ROWS {
        return Err(WorkspaceError::Invalid(format!(
            "MCP export is limited to {MAX_MCP_EXPORT_ROWS} rows; use the CLI export for larger tables"
        )));
    }
    let (columns, rows) = select_table(&database, table)?;
    debug_assert_eq!(rows.len(), row_count);
    let mut output = LimitedBuffer::new(max_content_bytes);
    let result = match format {
        DataFormat::Csv => write_csv(&columns, &rows, &mut output),
        DataFormat::JsonLines => write_json_lines(&columns, &rows, &mut output),
        DataFormat::Sql => write_sql(&database, table, &rows, &mut output),
        DataFormat::Json => unreachable!(),
    };
    if output.exceeded {
        return Err(WorkspaceError::Invalid(format!(
            "MCP export exceeds the {max_content_bytes}-byte content limit"
        )));
    }
    result?;
    let content = String::from_utf8(output.into_inner())
        .map_err(|error| WorkspaceError::Invalid(format!("export is not valid UTF-8: {error}")))?;
    let bytes = content.len();
    Ok(ExportReport {
        table: table.to_string(),
        format: format.name().to_string(),
        bytes,
        content,
    })
}

pub(crate) fn mcp_inspect(workspace: &Workspace) -> Result<InspectReport, WorkspaceError> {
    let database = workspace.database()?;
    inspect(workspace, &database)
}

fn required_table(table: Option<&str>) -> Result<&str, WorkspaceError> {
    let table = table.ok_or_else(|| {
        WorkspaceError::Usage("row imports require --table NAME or a source filename".to_string())
    })?;
    validate_name(table, "table name")?;
    Ok(table)
}

fn validate_headers(headers: &StringRecord) -> Result<Vec<String>, WorkspaceError> {
    let mut seen = HashMap::new();
    let mut columns = Vec::with_capacity(headers.len());
    for header in headers {
        validate_name(header, "CSV header")?;
        let key = header.to_ascii_lowercase();
        if seen.insert(key, ()).is_some() {
            return Err(WorkspaceError::Invalid(format!(
                "duplicate CSV header: {header:?}"
            )));
        }
        columns.push(header.to_string());
    }
    if columns.is_empty() {
        return Err(WorkspaceError::Invalid(
            "CSV input must contain a header row".to_string(),
        ));
    }
    Ok(columns)
}

fn validate_name(name: &str, label: &str) -> Result<(), WorkspaceError> {
    if name.trim().is_empty() {
        return Err(WorkspaceError::Invalid(format!("{label} cannot be empty")));
    }
    if name.contains('\0') {
        return Err(WorkspaceError::Invalid(format!(
            "{label} cannot contain NUL bytes"
        )));
    }
    Ok(())
}

fn parse_csv_cell(value: &str) -> ImportedCell {
    if value.is_empty() {
        return ImportedCell::Empty;
    }
    if value.eq_ignore_ascii_case("true") {
        return ImportedCell::Boolean(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return ImportedCell::Boolean(false);
    }
    if let Ok(integer) = value.parse::<i64>() {
        return ImportedCell::Integer(integer);
    }
    if let Ok(real) = value.parse::<f64>()
        && real.is_finite()
    {
        return ImportedCell::Real(real);
    }
    ImportedCell::Text(value.to_string())
}

fn json_cell(value: &JsonValue) -> ImportedCell {
    match value {
        JsonValue::Null => ImportedCell::Null,
        JsonValue::Bool(value) => ImportedCell::Boolean(*value),
        JsonValue::Number(value) => value
            .as_i64()
            .map(ImportedCell::Integer)
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(ImportedCell::Real)
            })
            .unwrap_or_else(|| ImportedCell::Text(value.to_string())),
        JsonValue::String(value) => ImportedCell::Text(value.clone()),
        JsonValue::Array(_) | JsonValue::Object(_) => ImportedCell::Text(value.to_string()),
    }
}

fn infer_types(rows: &[Vec<ImportedCell>], width: usize) -> Vec<ColumnType> {
    (0..width)
        .map(|column| {
            let mut inferred = None;
            for row in rows {
                let kind = match row.get(column) {
                    Some(ImportedCell::Null | ImportedCell::Empty) | None => None,
                    Some(ImportedCell::Integer(_)) => Some(ColumnType::Integer),
                    Some(ImportedCell::Real(_)) => Some(ColumnType::Real),
                    Some(ImportedCell::Boolean(_)) => Some(ColumnType::Boolean),
                    Some(ImportedCell::Text(_)) => Some(ColumnType::Text),
                };
                inferred = merge_inferred(inferred, kind);
                if inferred == Some(ColumnType::Text) {
                    break;
                }
            }
            inferred.unwrap_or(ColumnType::Text)
        })
        .collect()
}

fn merge_inferred(current: Option<ColumnType>, next: Option<ColumnType>) -> Option<ColumnType> {
    match (current, next) {
        (None, value) | (value, None) => value,
        (Some(ColumnType::Integer), Some(ColumnType::Integer)) => Some(ColumnType::Integer),
        (Some(ColumnType::Integer), Some(ColumnType::Real))
        | (Some(ColumnType::Real), Some(ColumnType::Integer))
        | (Some(ColumnType::Real), Some(ColumnType::Real)) => Some(ColumnType::Real),
        (Some(ColumnType::Boolean), Some(ColumnType::Boolean)) => Some(ColumnType::Boolean),
        _ => Some(ColumnType::Text),
    }
}

impl ImportedRows {
    fn summary(&self) -> String {
        format!(
            "table {} ({} rows, {} columns)",
            self.table,
            self.rows.len(),
            self.columns.len()
        )
    }
}

fn import_rows(
    database: &Database,
    imported: &ImportedRows,
    max_work: Option<usize>,
) -> Result<(), WorkspaceError> {
    validate_name(&imported.table, "table name")?;
    if imported.columns.is_empty() {
        return Err(WorkspaceError::Invalid(
            "cannot import a table with no columns".to_string(),
        ));
    }
    if imported
        .rows
        .iter()
        .any(|row| row.len() != imported.columns.len())
    {
        return Err(WorkspaceError::Invalid(
            "imported row width does not match the header".to_string(),
        ));
    }
    let definitions = imported
        .columns
        .iter()
        .zip(&imported.types)
        .map(|(name, ty)| format!("{} {}", quote_identifier(name), column_type_name(ty)))
        .collect::<Vec<_>>();
    let create = format!(
        "CREATE TABLE {} ({})",
        quote_identifier(&imported.table),
        definitions.join(", ")
    );

    let mut budget = max_work
        .map(ExecutionBudget::bounded)
        .unwrap_or_else(ExecutionBudget::unlimited);
    let mut connection = database.connect();
    connection.execute_with_budget(&Statement::Begin, &mut budget)?;
    let result = (|| {
        connection.execute_sql_using_budget(&create, &mut budget)?;
        for chunk in imported.rows.chunks(IMPORT_BATCH_SIZE) {
            if chunk.is_empty() {
                continue;
            }
            let rows = chunk
                .iter()
                .map(|row| {
                    row.iter()
                        .zip(&imported.types)
                        .map(|(cell, ty)| cell_to_value(cell, ty).map(|value| sql_literal(&value)))
                        .collect::<Result<Vec<_>, WorkspaceError>>()
                        .map(|values| format!("({})", values.join(", ")))
                })
                .collect::<Result<Vec<_>, WorkspaceError>>()?;
            let insert = format!(
                "INSERT INTO {} VALUES {}",
                quote_identifier(&imported.table),
                rows.join(", ")
            );
            connection.execute_sql_using_budget(&insert, &mut budget)?;
        }
        connection.execute_with_budget(&Statement::Commit, &mut budget)?;
        Ok::<(), WorkspaceError>(())
    })();
    if let Err(error) = result {
        let _ = connection.execute_sql("ROLLBACK");
        return Err(error);
    }
    Ok(())
}

fn cell_to_value(cell: &ImportedCell, ty: &ColumnType) -> Result<Value, WorkspaceError> {
    let value = match (cell, ty) {
        (ImportedCell::Null, _) => Value::Null,
        (ImportedCell::Empty, ColumnType::Text) => Value::Text(String::new()),
        (ImportedCell::Empty, _) => Value::Null,
        (ImportedCell::Integer(value), ColumnType::Integer) => Value::Integer(*value),
        (ImportedCell::Integer(value), ColumnType::Real) => Value::Real(*value as f64),
        (ImportedCell::Integer(value), ColumnType::Boolean) => Value::Boolean(*value != 0),
        (ImportedCell::Integer(value), ColumnType::Text) => Value::Text(value.to_string()),
        (ImportedCell::Real(value), ColumnType::Real) => Value::Real(*value),
        (ImportedCell::Real(value), ColumnType::Text) => Value::Text(value.to_string()),
        (ImportedCell::Real(value), ColumnType::Integer) => {
            return Err(WorkspaceError::Invalid(format!(
                "cannot store REAL value {value} in inferred INTEGER column"
            )));
        }
        (ImportedCell::Real(value), ColumnType::Boolean) => {
            return Err(WorkspaceError::Invalid(format!(
                "cannot store REAL value {value} in inferred BOOLEAN column"
            )));
        }
        (ImportedCell::Boolean(value), ColumnType::Boolean) => Value::Boolean(*value),
        (ImportedCell::Boolean(value), ColumnType::Text) => Value::Text(value.to_string()),
        (ImportedCell::Boolean(value), ColumnType::Integer) => Value::Integer(*value as i64),
        (ImportedCell::Boolean(value), ColumnType::Real) => Value::Real(*value as u8 as f64),
        (ImportedCell::Text(value), ColumnType::Text) => Value::Text(value.clone()),
        (ImportedCell::Text(value), _) => {
            return Err(WorkspaceError::Invalid(format!(
                "text value {value:?} conflicts with inferred {:?} column",
                ty
            )));
        }
        (_, ColumnType::Any | ColumnType::Null) => Value::Null,
    };
    Ok(value)
}

fn select_table(
    database: &Database,
    table: &str,
) -> Result<(Vec<String>, Vec<Vec<Value>>), WorkspaceError> {
    validate_name(table, "table name")?;
    let sql = format!("SELECT * FROM {}", quote_identifier(table));
    let results = database.execute_sql(&sql)?;
    let Some(StatementResult::Select { columns, rows }) = results.into_iter().next() else {
        return Err(WorkspaceError::Invalid(
            "table query did not return rows".to_string(),
        ));
    };
    Ok((columns, rows))
}

fn export_csv(columns: &[String], rows: &[Vec<Value>]) -> Result<Vec<u8>, WorkspaceError> {
    let mut output = Vec::new();
    write_csv(columns, rows, &mut output)?;
    Ok(output)
}

fn write_csv<W: Write>(
    columns: &[String],
    rows: &[Vec<Value>],
    output: &mut W,
) -> Result<(), WorkspaceError> {
    let mut writer = Writer::from_writer(output);
    writer.write_record(columns)?;
    for row in rows {
        ensure_row_width(columns, row)?;
        let values = row.iter().map(csv_field).collect::<Vec<_>>();
        writer.write_record(values)?;
    }
    writer.flush()?;
    Ok(())
}

fn export_json_lines(columns: &[String], rows: &[Vec<Value>]) -> Result<Vec<u8>, WorkspaceError> {
    let mut output = Vec::new();
    write_json_lines(columns, rows, &mut output)?;
    Ok(output)
}

fn write_json_lines<W: Write>(
    columns: &[String],
    rows: &[Vec<Value>],
    output: &mut W,
) -> Result<(), WorkspaceError> {
    for row in rows {
        ensure_row_width(columns, row)?;
        let mut object = Map::new();
        for (column, value) in columns.iter().zip(row) {
            object.insert(column.clone(), value_to_json(value)?);
        }
        serde_json::to_writer(&mut *output, &JsonValue::Object(object))?;
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn export_sql(
    database: &Database,
    table: &str,
    rows: &[Vec<Value>],
) -> Result<Vec<u8>, WorkspaceError> {
    let mut output = Vec::new();
    write_sql(database, table, rows, &mut output)?;
    Ok(output)
}

fn write_sql<W: Write>(
    database: &Database,
    table: &str,
    rows: &[Vec<Value>],
    output: &mut W,
) -> Result<(), WorkspaceError> {
    let columns = database.columns(table)?;
    write!(output, "CREATE TABLE {} (", quote_identifier(table))?;
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b", ")?;
        }
        output.write_all(column_definition(column).as_bytes())?;
    }
    output.write_all(b");\n")?;
    for row in rows {
        ensure_row_width(
            &columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>(),
            row,
        )?;
        write!(output, "INSERT INTO {} VALUES (", quote_identifier(table))?;
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.write_all(b", ")?;
            }
            output.write_all(sql_literal(value).as_bytes())?;
        }
        output.write_all(b");\n")?;
    }
    Ok(())
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "export content limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn column_definition(column: &Column) -> String {
    let mut definition = format!(
        "{} {}",
        quote_identifier(&column.name),
        column_type_name(&column.ty)
    );
    if column.primary_key {
        definition.push_str(" PRIMARY KEY");
    } else if column.not_null {
        definition.push_str(" NOT NULL");
    }
    if column.unique && !column.primary_key {
        definition.push_str(" UNIQUE");
    }
    definition
}

fn ensure_row_width(columns: &[String], row: &[Value]) -> Result<(), WorkspaceError> {
    if columns.len() != row.len() {
        return Err(WorkspaceError::Invalid(format!(
            "row width mismatch: expected {}, got {}",
            columns.len(),
            row.len()
        )));
    }
    Ok(())
}

fn csv_field(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Text(value) => value.clone(),
        Value::Boolean(value) => value.to_string(),
    }
}

fn value_to_json(value: &Value) -> Result<JsonValue, WorkspaceError> {
    match value {
        Value::Null => Ok(JsonValue::Null),
        Value::Integer(value) => Ok(JsonValue::Number((*value).into())),
        Value::Real(value) => serde_json::Number::from_f64(*value)
            .map(JsonValue::Number)
            .ok_or_else(|| WorkspaceError::Invalid("cannot export non-finite REAL".to_string())),
        Value::Text(value) => Ok(JsonValue::String(value.clone())),
        Value::Boolean(value) => Ok(JsonValue::Bool(*value)),
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn column_type_name(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Integer => "INTEGER",
        ColumnType::Real => "REAL",
        ColumnType::Text => "TEXT",
        ColumnType::Boolean => "BOOLEAN",
        ColumnType::Any => "TEXT",
        ColumnType::Null => "TEXT",
    }
}

fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
        Value::Boolean(value) => value.to_string(),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct InspectReport {
    path: String,
    format_version: u32,
    database: String,
    tables: Vec<InspectTable>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct InspectTable {
    name: String,
    rows: usize,
    columns: Vec<InspectColumn>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct InspectColumn {
    name: String,
    data_type: &'static str,
    not_null: bool,
    unique: bool,
    primary_key: bool,
}

fn inspect(workspace: &Workspace, database: &Database) -> Result<InspectReport, WorkspaceError> {
    let mut tables = Vec::new();
    for table_name in database.table_names()? {
        let columns = database.columns(&table_name)?;
        let rows = table_row_count(database, &table_name)?;
        tables.push(InspectTable {
            name: table_name,
            rows,
            columns: columns
                .iter()
                .map(|column| InspectColumn {
                    name: column.name.clone(),
                    data_type: column_type_name(&column.ty),
                    not_null: column.not_null,
                    unique: column.unique,
                    primary_key: column.primary_key,
                })
                .collect(),
        });
    }
    Ok(InspectReport {
        path: workspace.root.display().to_string(),
        format_version: workspace.manifest.format_version,
        database: workspace.manifest.database.clone(),
        tables,
    })
}

fn table_row_count(database: &Database, table: &str) -> Result<usize, WorkspaceError> {
    Ok(database.row_count(table)?)
}

fn render_inspect(report: &InspectReport, output: &mut dyn Write) -> Result<(), WorkspaceError> {
    writeln!(output, "Workspace: {}", report.path)?;
    writeln!(output, "Workspace format: {}", report.format_version)?;
    writeln!(output, "Database: {}", report.database)?;
    if report.tables.is_empty() {
        writeln!(output, "Tables: none")?;
        return Ok(());
    }
    writeln!(output, "Tables: {}", report.tables.len())?;
    for table in &report.tables {
        let columns = table
            .columns
            .iter()
            .map(|column| format!("{} {}", column.name, column.data_type))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            output,
            "- {} ({} rows): {}",
            table.name, table.rows, columns
        )?;
    }
    Ok(())
}

fn render_plan(
    workspace: &Workspace,
    plan: &PlanRecord,
    output: &mut dyn Write,
) -> Result<(), WorkspaceError> {
    writeln!(output, "Plan: {}", plan.plan_id)?;
    writeln!(output, "Workspace: {}", workspace.root.display())?;
    writeln!(output, "Base state: {}", plan.base_state)?;
    writeln!(output, "Statements: {}", plan.statements.len())?;
    for item in &plan.statements {
        let detail = item
            .rows_affected
            .map(|rows| format!("{rows} row(s) affected"))
            .or_else(|| {
                item.rows_returned
                    .map(|rows| format!("{rows} row(s) returned"))
            })
            .or_else(|| item.object.clone())
            .unwrap_or_default();
        if detail.is_empty() {
            writeln!(output, "- {}: {}", item.statement, item.kind)?;
        } else {
            writeln!(output, "- {}: {} ({detail})", item.statement, item.kind)?;
        }
    }
    writeln!(
        output,
        "Apply: basalt workspace apply {} {}",
        workspace.root.display(),
        plan.plan_id
    )?;
    Ok(())
}

fn render_apply(report: &ApplyReport, output: &mut dyn Write) -> Result<(), WorkspaceError> {
    writeln!(output, "Applied change {}", report.change_id)?;
    writeln!(output, "Plan: {}", report.plan_id)?;
    writeln!(output, "State: {}", report.after_state)?;
    writeln!(output, "Generation: {}", report.generation)?;
    Ok(())
}

fn render_history(entries: &[HistoryEntry], output: &mut dyn Write) -> Result<(), WorkspaceError> {
    if entries.is_empty() {
        writeln!(output, "No changes.")?;
        return Ok(());
    }
    for entry in entries {
        writeln!(
            output,
            "#{} {} {} {}",
            entry.sequence,
            entry.change_id,
            change_kind_name(&entry.kind),
            change_status_name(&entry.status)
        )?;
        if let Some(error) = &entry.error {
            writeln!(output, "  {error}")?;
        }
    }
    Ok(())
}

fn render_diff(report: &DiffReport, output: &mut dyn Write) -> Result<(), WorkspaceError> {
    writeln!(output, "Diff for change {}", report.change_id)?;
    writeln!(output, "Precision: {}", report.precision)?;
    writeln!(output, "Before: {}", report.before_state)?;
    writeln!(output, "Current: {}", report.current_state)?;
    if report.tables.is_empty() {
        writeln!(output, "No logical table changes.")?;
        return Ok(());
    }
    for table in &report.tables {
        let before = table
            .before_rows
            .map(|rows| rows.to_string())
            .unwrap_or_else(|| "absent".to_string());
        let after = table
            .after_rows
            .map(|rows| rows.to_string())
            .unwrap_or_else(|| "absent".to_string());
        writeln!(
            output,
            "- {}: {} -> {} row(s), schema_changed={}, data_changed={}",
            table.table, before, after, table.schema_changed, table.data_changed
        )?;
    }
    Ok(())
}

fn render_undo(report: &UndoReport, output: &mut dyn Write) -> Result<(), WorkspaceError> {
    writeln!(
        output,
        "Undid change {} as {}",
        report.undone_change_id, report.change_id
    )?;
    writeln!(output, "Restored state: {}", report.restored_state)?;
    writeln!(output, "Generation: {}", report.generation)?;
    Ok(())
}

fn change_kind_name(kind: &ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Apply => "apply",
        ChangeKind::Undo => "undo",
    }
}

fn change_status_name(status: &ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Prepared => "prepared",
        ChangeStatus::Committed => "committed",
        ChangeStatus::Recovered => "recovered",
        ChangeStatus::Failed => "failed",
        ChangeStatus::Unresolved => "unresolved",
    }
}

fn write_output(
    workspace: &Workspace,
    destination: &Path,
    bytes: &[u8],
    stdout: &mut dyn Write,
) -> Result<(), WorkspaceError> {
    if destination.as_os_str() == OsStr::new("-") {
        stdout.write_all(bytes)?;
        return Ok(());
    }
    if same_path(destination, &workspace.database_path())
        || same_path(destination, &workspace.root.join(MANIFEST_FILE))
    {
        return Err(WorkspaceError::Invalid(
            "refusing to overwrite workspace metadata or database".to_string(),
        ));
    }
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    atomic_write_file(destination, bytes)
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
    if path_is_symlink(path)? {
        return Err(WorkspaceError::Invalid(format!(
            "refusing to write through a symbolic link: {}",
            path.display()
        )));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if path.exists() => {
                fs::remove_file(path)?;
                fs::rename(&temporary, path).map_err(|_| error)
            }
            Err(error) => Err(error),
        }
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map_err(WorkspaceError::Io)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
    if path_is_symlink(path)? {
        return Err(WorkspaceError::Invalid(format!(
            "refusing to create through a symbolic link: {}",
            path.display()
        )));
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn path_is_symlink(path: &Path) -> Result<bool, WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(WorkspaceError::Io(error)),
    }
}

fn acquire_workspace_lock(root: &Path) -> Result<Arc<File>, WorkspaceError> {
    let path = root.join(WORKSPACE_LOCK_FILE);
    if path_is_symlink(&path)? {
        return Err(WorkspaceError::Invalid(
            "workspace lock cannot be a symbolic link".to_string(),
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Arc::new(file)),
        Err(fs4::TryLockError::WouldBlock) => Err(WorkspaceError::Invalid(format!(
            "workspace is already open: {}",
            root.display()
        ))),
        Err(fs4::TryLockError::Error(error)) => Err(WorkspaceError::Io(error)),
    }
}

fn validate_database_paths(root: &Path) -> Result<(), WorkspaceError> {
    let database = root.join(DATABASE_FILE);
    if path_is_symlink(&database)? {
        return Err(WorkspaceError::Invalid(
            "workspace database cannot be a symbolic link".to_string(),
        ));
    }
    if database.exists() && !database.is_file() {
        return Err(WorkspaceError::Invalid(format!(
            "workspace database is not a file: {}",
            database.display()
        )));
    }
    for suffix in [".wal", ".lock", ".tmp"] {
        let mut value = database.as_os_str().to_os_string();
        value.push(suffix);
        let path = PathBuf::from(value);
        if path_is_symlink(&path)? {
            return Err(WorkspaceError::Invalid(format!(
                "workspace database sidecar cannot be a symbolic link: {}",
                path.display()
            )));
        }
        if path.exists() && !path.is_file() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace database sidecar is not a file: {}",
                path.display()
            )));
        }
    }
    let mut wal_value = database.as_os_str().to_os_string();
    wal_value.push(".wal");
    if !database.exists() && !Path::new(&wal_value).exists() {
        return Err(WorkspaceError::Invalid(format!(
            "workspace database is missing: {}",
            database.display()
        )));
    }
    Ok(())
}

fn manifest_bytes(manifest: &WorkspaceManifest) -> Result<Vec<u8>, WorkspaceError> {
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn temporary_path(destination: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("export");
    destination.with_file_name(format!(".{name}.basalt-tmp-{}-{stamp}", std::process::id()))
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (resolved_path(left), resolved_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => normalized_path(left) == normalized_path(right),
    }
}

fn resolved_path(path: &Path) -> Option<PathBuf> {
    let mut candidate = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(&candidate) {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = candidate.file_name()?.to_os_string();
                suffix.push(name);
                if !candidate.pop() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

fn normalized_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut components = Vec::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(components.last(), Some(Component::Normal(_))) {
                    components.pop();
                }
            }
            _ => components.push(component),
        }
    }
    components
        .into_iter()
        .fold(PathBuf::new(), |mut path, component| {
            path.push(component.as_os_str());
            path
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_numeric_and_text_columns() {
        let rows = vec![
            vec![ImportedCell::Integer(1), ImportedCell::Text("a".into())],
            vec![ImportedCell::Real(2.5), ImportedCell::Empty],
        ];
        assert_eq!(
            infer_types(&rows, 2),
            vec![ColumnType::Real, ColumnType::Text]
        );
    }

    #[test]
    fn quotes_identifiers_and_literals() {
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(sql_literal(&Value::Text("a'b".into())), "'a''b'");
    }

    #[test]
    fn manifest_is_stable() {
        let manifest = WorkspaceManifest {
            format_version: FORMAT_VERSION,
            database: DATABASE_FILE.to_string(),
        };
        assert_eq!(
            String::from_utf8(manifest_bytes(&manifest).unwrap()).unwrap(),
            "{\n  \"format_version\": 1,\n  \"database\": \"data.basalt\"\n}\n"
        );
    }

    #[test]
    fn open_or_init_does_not_replace_existing_directories() {
        let root = std::env::temp_dir().join(format!(
            "basalt-workspace-open-or-init-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("keep.txt");
        fs::write(&marker, b"keep").unwrap();

        let error = Workspace::open_or_init(&root).unwrap_err();
        assert!(error.to_string().contains("not a Basalt workspace"));
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_workspace_with_a_missing_database() {
        let root = std::env::temp_dir().join(format!(
            "basalt-workspace-missing-database-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = Workspace::init(&root).unwrap();
        drop(workspace);
        fs::remove_file(root.join(DATABASE_FILE)).unwrap();

        let error = Workspace::open(&root).unwrap_err();
        assert!(error.to_string().contains("workspace database is missing"));
        assert!(!root.join(DATABASE_FILE).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_path_aliases() {
        let root = std::env::temp_dir().join(format!(
            "basalt-same-path-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        let direct = root.join("workspace/data.basalt");
        let alias = root.join("workspace/../workspace/data.basalt");
        fs::write(&direct, b"database").unwrap();
        assert!(same_path(&alias, &direct));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn json_values_keep_nested_data_as_text() {
        assert!(matches!(
            json_cell(&serde_json::json!({"nested": true})),
            ImportedCell::Text(value) if value == "{\"nested\":true}"
        ));
    }

    #[test]
    fn mcp_import_limits_cover_rows_columns_and_cells() {
        let limits = ImportLimits::mcp();

        let rows = enforce_import_limits(MAX_MCP_IMPORT_ROWS + 1, 1, limits).unwrap_err();
        assert!(rows.to_string().contains("limited to 10000 rows"));

        let columns = enforce_import_limits(1, MAX_MCP_IMPORT_COLUMNS + 1, limits).unwrap_err();
        assert!(columns.to_string().contains("limited to 256 columns"));

        let cells = enforce_import_limits(10_000, 101, limits).unwrap_err();
        assert!(cells.to_string().contains("limited to 1000000 cells"));
    }

    #[test]
    fn mcp_mutation_row_limit_is_cumulative() {
        let mut total = 0;
        enforce_mutation_row_limit(
            &mut total,
            &StatementResult::Update {
                rows_affected: 6_000,
            },
            Some(MAX_MCP_MUTATION_ROWS),
        )
        .unwrap();
        let error = enforce_mutation_row_limit(
            &mut total,
            &StatementResult::Delete {
                rows_affected: 4_001,
            },
            Some(MAX_MCP_MUTATION_ROWS),
        )
        .unwrap_err();
        assert!(error.to_string().contains("limited to 10000 affected rows"));
    }

    #[test]
    fn mcp_rejects_an_over_limit_mutation_without_persisting_or_committing_it() {
        let root = std::env::temp_dir().join(format!(
            "basalt-workspace-mutation-limit-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = Workspace::init(&root).unwrap();
        let database = workspace.database().unwrap();
        database
            .execute_sql("CREATE TABLE events (id INTEGER)")
            .unwrap();
        let values = (1..=MAX_MCP_MUTATION_ROWS + 1)
            .map(|id| format!("({id})"))
            .collect::<Vec<_>>()
            .join(", ");
        database
            .execute_sql(&format!("INSERT INTO events VALUES {values}"))
            .unwrap();
        database.checkpoint().unwrap();
        drop(database);

        let error = mcp_preview(&workspace, "DELETE FROM events", 1_048_576).unwrap_err();
        assert!(error.to_string().contains("limited to 10000 affected rows"));
        assert!(!root.join(HISTORY_DIR).exists());

        let plan = preview_plan(&workspace, "DELETE FROM events").unwrap();
        let error = mcp_apply(&workspace, &plan.plan_id).unwrap_err();
        assert!(error.to_string().contains("limited to 10000 affected rows"));
        let changes = load_changes(&workspace).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].status, ChangeStatus::Failed);

        let database = workspace.database().unwrap();
        assert_eq!(
            database.row_count("events").unwrap(),
            MAX_MCP_MUTATION_ROWS + 1
        );
        drop(database);
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_an_oversized_mcp_preview_before_persisting_its_plan() {
        let root = std::env::temp_dir().join(format!(
            "basalt-workspace-preview-limit-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = Workspace::init(&root).unwrap();
        let error = mcp_preview(&workspace, "CREATE TABLE users (id INTEGER)", 1).unwrap_err();
        assert!(error.to_string().contains("response limit is 1 bytes"));
        assert!(!root.join(HISTORY_DIR).exists());
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retries_a_failed_mcp_import_when_the_base_state_is_unchanged() {
        let root = std::env::temp_dir().join(format!(
            "basalt-workspace-retry-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = Workspace::init(&root).unwrap();
        let content = "id,name\n1,Ada\n";
        let format = DataFormat::Csv;
        let table = "users";
        let database = workspace.database().unwrap();
        database.checkpoint().unwrap();
        let base_state = state_fingerprint(&workspace.database_path()).unwrap();
        let base_generation = database.generation();
        drop(database);

        ensure_history_dirs(&workspace).unwrap();
        let change_id = import_change_id(base_state.as_str(), format, table, content.as_bytes());
        copy_atomic(
            &workspace.database_path(),
            &snapshot_path(&workspace, &change_id),
        )
        .unwrap();
        let request_key = import_request_key(format, table, content.as_bytes());
        let summary =
            validate_mcp_import(format, table, content.as_bytes(), ImportLimits::mcp()).unwrap();
        let failed = ChangeRecord {
            format_version: FORMAT_VERSION,
            sequence: 1,
            change_id: change_id.clone(),
            kind: ChangeKind::Apply,
            plan_id: None,
            target_change_id: None,
            base_generation,
            base_state: base_state.clone(),
            expected_state: None,
            snapshot_id: change_id.clone(),
            sql: None,
            status: ChangeStatus::Failed,
            committed_generation: None,
            after_state: None,
            error: Some("transient test failure".to_string()),
            import: Some(ImportMetadata {
                request_key,
                format: format.name().to_string(),
                table: table.to_string(),
                bytes: content.len(),
                summary,
            }),
        };
        write_new_json(&change_path(&workspace, &change_id), &failed).unwrap();

        let report = mcp_import(&workspace, Some(table), format.name(), content).unwrap();
        assert_eq!(report.change_id, change_id);
        assert_eq!(report.summary, "table users (1 rows, 2 columns)");
        let changes = load_changes(&workspace).unwrap();
        assert_eq!(changes[0].status, ChangeStatus::Committed);
        assert_eq!(changes[0].sequence, 1);

        let database = workspace.database().unwrap();
        let rows = database.execute_sql("SELECT id, name FROM users").unwrap();
        assert!(matches!(
            &rows[0],
            StatementResult::Select { rows, .. } if rows.len() == 1
        ));
        drop(database);
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }
}
