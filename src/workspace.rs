//! Portable workspace lifecycle for local structured-data workflows.
//!
//! A workspace is a directory with a versioned manifest and one Basalt
//! database. Import and export deliberately use common text formats so a
//! workspace is inspectable and recoverable without Basalt-specific tooling.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use csv::{ReaderBuilder, StringRecord, Writer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};

use crate::database::Database;
use crate::db::{Column, DbError, StatementResult};
use crate::sql::ast::Statement;
use crate::sql::parser::parse;
use crate::types::{ColumnType, Value};

const MANIFEST_FILE: &str = "workspace.json";
const DATABASE_FILE: &str = "data.basalt";
const FORMAT_VERSION: u32 = 1;
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
const IMPORT_BATCH_SIZE: usize = 256;

pub const HELP: &str = "Basalt workspace — local, portable SQL workspaces\n\n\
Usage:\n  basalt workspace <COMMAND> [OPTIONS]\n\n\
Commands:\n  init PATH                         Create a workspace\n  inspect [--json] PATH             Show workspace metadata and schema\n  query [OPTIONS] PATH SQL          Run a read-only query\n  import [OPTIONS] WORKSPACE SOURCE Import CSV, JSON, JSONL, or SQL\n  export [OPTIONS] WORKSPACE TABLE OUTPUT\n                                     Export CSV, JSONL, or SQL\n\n\
Import options:\n  --table NAME                      Table name (required for stdin)\n  --format csv|json|jsonl|sql       Override format inference\n\n\
Export options:\n  --format csv|jsonl|sql             Override format inference\n\n\
Query options:\n  --output table|csv|json             Result format (table by default)\n\n\
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
}

impl Workspace {
    pub fn init(path: impl AsRef<Path>) -> Result<Workspace, WorkspaceError> {
        let root = path.as_ref().to_path_buf();
        if root.as_os_str().is_empty() {
            return Err(WorkspaceError::Invalid(
                "workspace path cannot be empty".to_string(),
            ));
        }
        if root.exists() && !root.is_dir() {
            return Err(WorkspaceError::Invalid(format!(
                "workspace path is not a directory: {}",
                root.display()
            )));
        }
        fs::create_dir_all(&root)?;
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

        let database = Database::open(&database_path)?;
        database.checkpoint()?;
        drop(database);

        Ok(Workspace { root, manifest })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Workspace, WorkspaceError> {
        let root = path.as_ref().to_path_buf();
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
        if path_is_symlink(&root.join(DATABASE_FILE))? {
            return Err(WorkspaceError::Invalid(
                "workspace database cannot be a symbolic link".to_string(),
            ));
        }
        Ok(Workspace { root, manifest })
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
        if path_is_symlink(&path)? {
            return Err(WorkspaceError::Invalid(
                "workspace database cannot be a symbolic link".to_string(),
            ));
        }
        Ok(Database::open(path)?)
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
    Import {
        workspace: PathBuf,
        source: PathBuf,
        table: Option<String>,
        format: Option<DataFormat>,
    },
    Export {
        workspace: PathBuf,
        table: String,
        output: PathBuf,
        format: Option<DataFormat>,
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
        Command::Import {
            workspace,
            source,
            table,
            format,
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
                DataFormat::Csv => import_csv(&database, table_name.as_deref(), &bytes)?,
                DataFormat::Json => import_json(&database, table_name.as_deref(), &bytes)?,
                DataFormat::JsonLines => {
                    import_json_lines(&database, table_name.as_deref(), &bytes)?
                }
                DataFormat::Sql => {
                    if table_name.is_some() {
                        return Err(WorkspaceError::Usage(
                            "--table is not valid for SQL imports".to_string(),
                        ));
                    }
                    import_sql(&database, &bytes)?
                }
            };
            writeln!(
                output,
                "imported {}{}",
                format.name(),
                imported
                    .map(|summary| format!(": {summary}"))
                    .unwrap_or_default()
            )?;
        }
        Command::Export {
            workspace,
            table,
            output: destination,
            format,
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
            let database = workspace.database()?;
            let (columns, rows) = select_table(&database, &table)?;
            let bytes = match format {
                DataFormat::Csv => export_csv(&columns, &rows)?,
                DataFormat::JsonLines => export_json_lines(&columns, &rows)?,
                DataFormat::Sql => export_sql(&database, &table, &rows)?,
                DataFormat::Json => unreachable!(),
            };
            write_output(&workspace, &destination, &bytes, output)?;
            if destination.as_os_str() == OsStr::new("-") {
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

fn parse_import(args: &[String]) -> Result<Command, WorkspaceError> {
    let mut table = None;
    let mut format = None;
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
    })
}

fn parse_export(args: &[String]) -> Result<Command, WorkspaceError> {
    let mut format = None;
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

fn import_csv(
    database: &Database,
    table: Option<&str>,
    bytes: &[u8],
) -> Result<Option<String>, WorkspaceError> {
    let table = required_table(table)?;
    let mut reader = ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_reader(bytes);
    let headers = reader.headers()?.clone();
    let columns = validate_headers(&headers)?;
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record?;
        rows.push(record.iter().map(parse_csv_cell).collect());
    }
    let imported = ImportedRows {
        table: table.to_string(),
        types: infer_types(&rows, columns.len()),
        columns,
        rows,
    };
    let summary = imported.summary();
    import_rows(database, &imported)?;
    Ok(Some(summary))
}

fn import_json(
    database: &Database,
    table: Option<&str>,
    bytes: &[u8],
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
    import_json_objects(database, table, objects)
}

fn import_json_lines(
    database: &Database,
    table: Option<&str>,
    bytes: &[u8],
) -> Result<Option<String>, WorkspaceError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        WorkspaceError::Invalid(format!("JSON Lines input is not UTF-8: {error}"))
    })?;
    let mut objects = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
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
    import_json_objects(database, table, objects)
}

fn import_json_objects(
    database: &Database,
    table: Option<&str>,
    objects: Vec<JsonValue>,
) -> Result<Option<String>, WorkspaceError> {
    let table = required_table(table)?;
    if objects.is_empty() {
        return Err(WorkspaceError::Invalid(
            "JSON import contains no objects; a table schema cannot be inferred".to_string(),
        ));
    }
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
    import_rows(database, &imported)?;
    Ok(Some(summary))
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

fn import_rows(database: &Database, imported: &ImportedRows) -> Result<(), WorkspaceError> {
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

    let mut connection = database.connect();
    connection.execute_sql("BEGIN")?;
    let result = (|| {
        connection.execute_sql(&create)?;
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
            connection.execute_sql(&insert)?;
        }
        connection.execute_sql("COMMIT")?;
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
    let mut writer = Writer::from_writer(Vec::new());
    writer.write_record(columns)?;
    for row in rows {
        ensure_row_width(columns, row)?;
        let values = row.iter().map(csv_field).collect::<Vec<_>>();
        writer.write_record(values)?;
    }
    writer
        .into_inner()
        .map_err(|error| WorkspaceError::Invalid(format!("could not finish CSV export: {error}")))
}

fn export_json_lines(columns: &[String], rows: &[Vec<Value>]) -> Result<Vec<u8>, WorkspaceError> {
    let mut output = Vec::new();
    for row in rows {
        ensure_row_width(columns, row)?;
        let mut object = Map::new();
        for (column, value) in columns.iter().zip(row) {
            object.insert(column.clone(), value_to_json(value)?);
        }
        serde_json::to_writer(&mut output, &JsonValue::Object(object))?;
        output.push(b'\n');
    }
    Ok(output)
}

fn export_sql(
    database: &Database,
    table: &str,
    rows: &[Vec<Value>],
) -> Result<Vec<u8>, WorkspaceError> {
    let columns = database.columns(table)?;
    let mut output = String::new();
    output.push_str("CREATE TABLE ");
    output.push_str(&quote_identifier(table));
    output.push_str(" (");
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str(&column_definition(column));
    }
    output.push_str(");\n");
    for row in rows {
        ensure_row_width(
            &columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>(),
            row,
        )?;
        output.push_str("INSERT INTO ");
        output.push_str(&quote_identifier(table));
        output.push_str(" VALUES (");
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            output.push_str(&sql_literal(value));
        }
        output.push_str(");\n");
    }
    Ok(output.into_bytes())
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

#[derive(Debug, Serialize)]
struct InspectReport {
    path: String,
    format_version: u32,
    database: String,
    tables: Vec<InspectTable>,
}

#[derive(Debug, Serialize)]
struct InspectTable {
    name: String,
    rows: usize,
    columns: Vec<InspectColumn>,
}

#[derive(Debug, Serialize)]
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
    let sql = format!("SELECT COUNT(*) FROM {}", quote_identifier(table));
    let results = database.execute_sql(&sql)?;
    let Some(StatementResult::Select { rows, .. }) = results.into_iter().next() else {
        return Err(WorkspaceError::Invalid(
            "row count did not return a result".to_string(),
        ));
    };
    let Some(row) = rows.first() else {
        return Ok(0);
    };
    match row.first() {
        Some(Value::Integer(value)) if *value >= 0 => Ok(*value as usize),
        Some(value) => Err(WorkspaceError::Invalid(format!(
            "row count returned unexpected value {value}"
        ))),
        None => Ok(0),
    }
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
    let temporary = temporary_path(destination);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if destination.exists() => {
                fs::remove_file(destination)?;
                fs::rename(&temporary, destination).map_err(|_| error)
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
    let absolute = |path: &Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|directory| directory.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    };
    absolute(left) == absolute(right)
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
    fn json_values_keep_nested_data_as_text() {
        assert!(matches!(
            json_cell(&serde_json::json!({"nested": true})),
            ImportedCell::Text(value) if value == "{\"nested\":true}"
        ));
    }
}
