//! User-facing command-line frontend for Basalt.
//!
//! The SQL engine stays independent of frontend concerns, so the CLI keeps its
//! argument parsing, SQL buffering, result formatting, and meta commands in
//! this module. Keeping the frontend testable outside the executable makes
//! script and interactive behavior share the same connection semantics.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};

use crate::database::{Connection, Database};
use crate::db::{Column, DbError, StatementResult};
use crate::sql::parser::parse;
use crate::types::{ColumnType, Value};

pub const HELP: &str = "Basalt — embedded SQL database\n\n\
Usage:\n  basalt [OPTIONS] [DATABASE_PATH | :memory:]\n\n\
Options:\n  -c, --command SQL       Execute SQL and exit; may be repeated\n  -f, --file PATH         Execute a SQL script and exit; '-' reads stdin\n  -o, --output FORMAT     Result format: table, csv, or json\n      --table             Use table output (the default)\n      --csv               Use CSV output\n      --json              Use JSON-lines output\n      --no-header         Omit column headers in table/CSV output\n      --quiet             Suppress non-query success messages\n  -h, --help              Print this help\n  -V, --version           Print the version\n\n\
Interactive commands:\n  .help                   Show this help\n  .tables                 List tables\n  .schema [TABLE]         Show CREATE TABLE statements\n  .mode table|csv|json    Change result format\n  .headers on|off         Toggle result headers\n  .checkpoint             Flush the snapshot and truncate the WAL\n  .show                   Show frontend state\n  .clear                  Discard the pending SQL buffer\n  .quit, .exit            Leave the shell\n\n\
MCP server:\n  basalt mcp [OPTIONS] [DATABASE_PATH | :memory:]\n\n\
JSON output is one JSON object per statement (JSON Lines). CSV output emits\n\
only query rows, so it can be piped directly into another data tool.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Table,
    Csv,
    Json,
}

impl OutputMode {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Ok(OutputMode::Table),
            "csv" => Ok(OutputMode::Csv),
            "json" | "jsonl" | "ndjson" => Ok(OutputMode::Json),
            _ => Err(CliError::new(format!(
                "unknown output format {value:?}; expected table, csv, or json"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            OutputMode::Table => "table",
            OutputMode::Csv => "csv",
            OutputMode::Json => "json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Command(String),
    File(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub database: String,
    pub actions: Vec<InputAction>,
    pub output: OutputMode,
    pub headers: bool,
    pub quiet: bool,
    pub help: bool,
    pub version: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            database: ":memory:".into(),
            actions: Vec::new(),
            output: OutputMode::Table,
            headers: true,
            quiet: false,
            help: false,
            version: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<DbError> for CliError {
    fn from(error: DbError) -> Self {
        Self::new(error.message)
    }
}

/// Parse command-line arguments after the executable name.
pub fn parse_args(args: &[String]) -> Result<CliOptions, CliError> {
    let mut options = CliOptions::default();
    let mut positional_only = false;
    let mut database = None;
    let mut i = 0;

    while i < args.len() {
        let argument = args[i].as_str();
        if !positional_only && argument == "--" {
            positional_only = true;
            i += 1;
            continue;
        }

        let mut take_value = |name: &str| -> Result<String, CliError> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| {
                CliError::new(format!("{name} requires a value; try --help for usage"))
            })
        };

        if !positional_only {
            match argument {
                "-h" | "--help" => {
                    options.help = true;
                    i += 1;
                    continue;
                }
                "-V" | "--version" => {
                    options.version = true;
                    i += 1;
                    continue;
                }
                "-c" | "--command" => {
                    options
                        .actions
                        .push(InputAction::Command(take_value(argument)?));
                    i += 1;
                    continue;
                }
                "-f" | "--file" => {
                    options
                        .actions
                        .push(InputAction::File(take_value(argument)?));
                    i += 1;
                    continue;
                }
                "-o" | "--output" => {
                    options.output = OutputMode::parse(&take_value(argument)?)?;
                    i += 1;
                    continue;
                }
                "--table" => {
                    options.output = OutputMode::Table;
                    i += 1;
                    continue;
                }
                "--csv" => {
                    options.output = OutputMode::Csv;
                    i += 1;
                    continue;
                }
                "--json" => {
                    options.output = OutputMode::Json;
                    i += 1;
                    continue;
                }
                "--no-header" | "--no-headers" => {
                    options.headers = false;
                    i += 1;
                    continue;
                }
                "--quiet" | "-q" => {
                    options.quiet = true;
                    i += 1;
                    continue;
                }
                _ => {}
            }

            if let Some(value) = argument.strip_prefix("--command=") {
                if value.is_empty() {
                    return Err(CliError::new("--command requires a non-empty value"));
                }
                options.actions.push(InputAction::Command(value.into()));
                i += 1;
                continue;
            }
            if let Some(value) = argument.strip_prefix("--file=") {
                if value.is_empty() {
                    return Err(CliError::new("--file requires a non-empty value"));
                }
                options.actions.push(InputAction::File(value.into()));
                i += 1;
                continue;
            }
            if let Some(value) = argument.strip_prefix("--output=") {
                options.output = OutputMode::parse(value)?;
                i += 1;
                continue;
            }
            if let Some(value) = argument.strip_prefix("-c=") {
                if value.is_empty() {
                    return Err(CliError::new("-c requires a non-empty value"));
                }
                options.actions.push(InputAction::Command(value.into()));
                i += 1;
                continue;
            }
            if let Some(value) = argument.strip_prefix("-f=") {
                if value.is_empty() {
                    return Err(CliError::new("-f requires a non-empty value"));
                }
                options.actions.push(InputAction::File(value.into()));
                i += 1;
                continue;
            }
            if argument.starts_with('-') {
                return Err(CliError::new(format!(
                    "unknown option {argument:?}; try --help for usage"
                )));
            }
        }

        if database.replace(argument.to_string()).is_some() {
            return Err(CliError::new(
                "only one database path may be provided; try --help for usage",
            ));
        }
        i += 1;
    }

    if let Some(database) = database {
        options.database = database;
    }
    Ok(options)
}

/// Run either the requested commands/scripts or the interactive shell.
pub fn run<R: BufRead>(
    options: &CliOptions,
    database: Database,
    input: &mut R,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    if options.actions.is_empty() {
        run_interactive(options, &database, input, output)
    } else {
        run_actions(options, &database, input, output)
    }
}

fn run_actions<R: BufRead>(
    options: &CliOptions,
    database: &Database,
    input: &mut R,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    let mut connection = database.connect();
    for action in &options.actions {
        let (source, sql) = match action {
            InputAction::Command(sql) => ("command line".to_string(), sql.clone()),
            InputAction::File(path) if path == "-" => {
                let mut sql = String::new();
                input.read_to_string(&mut sql)?;
                ("stdin".to_string(), sql)
            }
            InputAction::File(path) => {
                let mut sql = String::new();
                File::open(path)
                    .map_err(|error| CliError::new(format!("{path}: {error}")))?
                    .read_to_string(&mut sql)
                    .map_err(|error| CliError::new(format!("{path}: {error}")))?;
                (path.clone(), sql)
            }
        };
        let results = connection
            .execute_sql(&sql)
            .map_err(|error| CliError::new(format!("{source}: {error}")))?;
        render_results(
            &results,
            options.output,
            options.headers,
            options.quiet,
            output,
        )?;
    }
    output.flush()?;
    Ok(())
}

fn run_interactive<R: BufRead>(
    options: &CliOptions,
    database: &Database,
    input: &mut R,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    let mut connection = database.connect();
    let mut mode = options.output;
    let mut headers = options.headers;
    let mut buffer = String::new();

    write!(output, "basalt> ")?;
    output.flush()?;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            if !buffer.trim().is_empty() {
                execute_interactive_sql(
                    &mut connection,
                    &buffer,
                    mode,
                    headers,
                    options.quiet,
                    output,
                )?;
            }
            break;
        }

        let trimmed = line.trim();
        if trimmed.starts_with('.') {
            if trimmed.eq_ignore_ascii_case(".clear") {
                buffer.clear();
            } else if buffer.trim().is_empty() {
                match handle_meta(
                    trimmed,
                    database,
                    &mut mode,
                    &mut headers,
                    &connection,
                    output,
                ) {
                    Ok(MetaAction::Quit) => break,
                    Ok(MetaAction::Continue) => {}
                    Err(error) => writeln!(output, "error: {error}")?,
                }
            } else {
                writeln!(
                    output,
                    "error: finish the pending SQL statement before using {trimmed}"
                )?;
            }
            write!(
                output,
                "{}> ",
                if buffer.trim().is_empty() {
                    "basalt"
                } else {
                    "   ..."
                }
            )?;
            output.flush()?;
            continue;
        }

        if trimmed.is_empty() && buffer.trim().is_empty() {
            write!(output, "basalt> ")?;
            output.flush()?;
            continue;
        }

        buffer.push_str(&line);
        loop {
            if let Some(end) = top_level_semicolon(&buffer) {
                let statement = buffer[..end].to_string();
                buffer.drain(..end);
                execute_interactive_sql(
                    &mut connection,
                    &statement,
                    mode,
                    headers,
                    options.quiet,
                    output,
                )?;
                continue;
            }
            if buffer.trim().is_empty() {
                buffer.clear();
                break;
            }
            if sql_has_open_construct(&buffer) {
                break;
            }
            match parse(&buffer) {
                Ok(statements) if !statements.is_empty() => {
                    let statement = std::mem::take(&mut buffer);
                    execute_interactive_sql(
                        &mut connection,
                        &statement,
                        mode,
                        headers,
                        options.quiet,
                        output,
                    )?;
                }
                Ok(_) => buffer.clear(),
                Err(_) => {
                    let statement = std::mem::take(&mut buffer);
                    execute_interactive_sql(
                        &mut connection,
                        &statement,
                        mode,
                        headers,
                        options.quiet,
                        output,
                    )?;
                }
            }
            break;
        }

        write!(
            output,
            "{}> ",
            if buffer.trim().is_empty() {
                "basalt"
            } else {
                "   ..."
            }
        )?;
        output.flush()?;
    }
    Ok(())
}

fn execute_interactive_sql(
    connection: &mut Connection,
    sql: &str,
    mode: OutputMode,
    headers: bool,
    quiet: bool,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    match connection.execute_sql(sql) {
        Ok(results) => render_results(&results, mode, headers, quiet, output)?,
        Err(error) => writeln!(output, "error: {error}")?,
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetaAction {
    Continue,
    Quit,
}

fn handle_meta(
    command: &str,
    database: &Database,
    mode: &mut OutputMode,
    headers: &mut bool,
    connection: &Connection,
    output: &mut dyn Write,
) -> Result<MetaAction, CliError> {
    let mut parts = command.split_whitespace();
    let name = parts.next().unwrap_or_default().to_ascii_lowercase();
    match name.as_str() {
        ".quit" | ".exit" => Ok(MetaAction::Quit),
        ".help" => {
            write!(output, "{HELP}")?;
            Ok(MetaAction::Continue)
        }
        ".tables" => {
            let names = database.table_names()?;
            if names.is_empty() {
                writeln!(output, "No tables.")?;
            } else {
                writeln!(output, "{}", names.join("  "))?;
            }
            Ok(MetaAction::Continue)
        }
        ".schema" => {
            let table = parts.next();
            if parts.next().is_some() {
                return Err(CliError::new("usage: .schema [TABLE]"));
            }
            render_schema(database, table, output)?;
            Ok(MetaAction::Continue)
        }
        ".mode" => {
            let value = parts
                .next()
                .ok_or_else(|| CliError::new("usage: .mode table|csv|json"))?;
            if parts.next().is_some() {
                return Err(CliError::new("usage: .mode table|csv|json"));
            }
            *mode = OutputMode::parse(value)?;
            writeln!(output, "output mode: {}", mode.name())?;
            Ok(MetaAction::Continue)
        }
        ".headers" => {
            let value = parts
                .next()
                .ok_or_else(|| CliError::new("usage: .headers on|off"))?;
            if parts.next().is_some() {
                return Err(CliError::new("usage: .headers on|off"));
            }
            *headers = match value.to_ascii_lowercase().as_str() {
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                _ => return Err(CliError::new("usage: .headers on|off")),
            };
            writeln!(output, "headers: {}", if *headers { "on" } else { "off" })?;
            Ok(MetaAction::Continue)
        }
        ".checkpoint" => {
            if connection.in_transaction() {
                return Err(CliError::new(
                    "cannot checkpoint while a transaction is active",
                ));
            }
            database.checkpoint()?;
            writeln!(output, "CHECKPOINT")?;
            Ok(MetaAction::Continue)
        }
        ".show" => {
            writeln!(output, "mode: {}", mode.name())?;
            writeln!(output, "headers: {}", if *headers { "on" } else { "off" })?;
            writeln!(
                output,
                "transaction: {}",
                if connection.in_transaction() {
                    "active"
                } else {
                    "none"
                }
            )?;
            Ok(MetaAction::Continue)
        }
        _ => Err(CliError::new(format!(
            "unknown command {command:?}; try .help"
        ))),
    }
}

fn render_results(
    results: &[StatementResult],
    mode: OutputMode,
    headers: bool,
    quiet: bool,
    output: &mut dyn Write,
) -> io::Result<()> {
    for result in results {
        if quiet
            && !matches!(
                result,
                StatementResult::Select { .. } | StatementResult::Explain(_)
            )
        {
            continue;
        }
        match mode {
            OutputMode::Table => render_table_result(result, headers, output)?,
            OutputMode::Csv => {
                if let StatementResult::Select { columns, rows } = result {
                    render_csv(columns, rows, headers, output)?;
                }
            }
            OutputMode::Json => render_json_result(result, output)?,
        }
    }
    Ok(())
}

fn render_table_result(
    result: &StatementResult,
    headers: bool,
    output: &mut dyn Write,
) -> io::Result<()> {
    match result {
        StatementResult::Select { columns, rows } => {
            let values: Vec<Vec<String>> = rows
                .iter()
                .map(|row| row.iter().map(value_text).collect())
                .collect();
            if headers {
                let widths = table_widths(columns, &values);
                writeln!(output, "{}", padded_row(columns, &widths))?;
                writeln!(output, "{}", separator_row(&widths))?;
                for row in &values {
                    writeln!(output, "{}", padded_row(row, &widths))?;
                }
            } else {
                for row in &values {
                    writeln!(output, "{}", row.join(" | "))?;
                }
            }
            writeln!(output, "{} row(s)", rows.len())?;
        }
        StatementResult::Insert { rows_affected }
        | StatementResult::Update { rows_affected }
        | StatementResult::Delete { rows_affected } => {
            writeln!(output, "{rows_affected} row(s) affected")?;
        }
        StatementResult::CreateTable { name } => writeln!(output, "table '{name}' created")?,
        StatementResult::DropTable { name } => writeln!(output, "table '{name}' dropped")?,
        StatementResult::CreateIndex { name, .. } => writeln!(output, "index '{name}' created")?,
        StatementResult::DropIndex { name } => writeln!(output, "index '{name}' dropped")?,
        StatementResult::Explain(value) => writeln!(output, "{value}")?,
        StatementResult::Begin => writeln!(output, "BEGIN")?,
        StatementResult::Commit => writeln!(output, "COMMIT")?,
        StatementResult::Rollback => writeln!(output, "ROLLBACK")?,
        StatementResult::Checkpoint => writeln!(output, "CHECKPOINT")?,
        StatementResult::Echo(value) => writeln!(output, "{value}")?,
    }
    Ok(())
}

fn render_csv(
    columns: &[String],
    rows: &[Vec<Value>],
    headers: bool,
    output: &mut dyn Write,
) -> io::Result<()> {
    if headers {
        write_csv_row(columns.iter().map(String::as_str), output)?;
    }
    for row in rows {
        write_csv_row(row.iter().map(|value| value_text(value)), output)?;
    }
    Ok(())
}

fn write_csv_row<I, S>(values: I, output: &mut dyn Write) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut first = true;
    for value in values {
        if !first {
            output.write_all(b",")?;
        }
        first = false;
        write_csv_field(value.as_ref(), output)?;
    }
    output.write_all(b"\n")?;
    Ok(())
}

fn write_csv_field(value: &str, output: &mut dyn Write) -> io::Result<()> {
    if value
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'))
    {
        write!(output, "\"{}\"", value.replace('"', "\"\""))?;
    } else {
        output.write_all(value.as_bytes())?;
    }
    Ok(())
}

fn render_json_result(result: &StatementResult, output: &mut dyn Write) -> io::Result<()> {
    let json = match result {
        StatementResult::Select { columns, rows } => {
            let columns = columns
                .iter()
                .map(|column| json_string(column))
                .collect::<Vec<_>>()
                .join(",");
            let rows = rows
                .iter()
                .map(|row| {
                    format!(
                        "[{}]",
                        row.iter().map(json_value).collect::<Vec<_>>().join(",")
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{\"type\":\"select\",\"columns\":[{columns}],\"rows\":[{rows}]}}")
        }
        StatementResult::Insert { rows_affected } => {
            format!("{{\"type\":\"insert\",\"rows_affected\":{rows_affected}}}")
        }
        StatementResult::Update { rows_affected } => {
            format!("{{\"type\":\"update\",\"rows_affected\":{rows_affected}}}")
        }
        StatementResult::Delete { rows_affected } => {
            format!("{{\"type\":\"delete\",\"rows_affected\":{rows_affected}}}")
        }
        StatementResult::CreateTable { name } => {
            format!(
                "{{\"type\":\"create_table\",\"name\":{}}}",
                json_string(name)
            )
        }
        StatementResult::DropTable { name } => {
            format!("{{\"type\":\"drop_table\",\"name\":{}}}", json_string(name))
        }
        StatementResult::CreateIndex {
            name,
            table,
            column,
        } => format!(
            "{{\"type\":\"create_index\",\"name\":{},\"table\":{},\"column\":{}}}",
            json_string(name),
            json_string(table),
            json_string(column)
        ),
        StatementResult::DropIndex { name } => {
            format!("{{\"type\":\"drop_index\",\"name\":{}}}", json_string(name))
        }
        StatementResult::Explain(value) => {
            format!("{{\"type\":\"explain\",\"value\":{}}}", json_string(value))
        }
        StatementResult::Begin => "{\"type\":\"begin\"}".into(),
        StatementResult::Commit => "{\"type\":\"commit\"}".into(),
        StatementResult::Rollback => "{\"type\":\"rollback\"}".into(),
        StatementResult::Checkpoint => "{\"type\":\"checkpoint\"}".into(),
        StatementResult::Echo(value) => {
            format!("{{\"type\":\"echo\",\"value\":{}}}", json_string(value))
        }
    };
    writeln!(output, "{json}")
}

fn render_schema(
    database: &Database,
    requested_table: Option<&str>,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    let tables = if let Some(table) = requested_table {
        let actual = database
            .table_names()?
            .into_iter()
            .find(|name| name.eq_ignore_ascii_case(table))
            .ok_or_else(|| CliError::new(format!("no such table: {table}")))?;
        vec![actual]
    } else {
        database.table_names()?
    };
    if tables.is_empty() {
        writeln!(output, "No tables.")?;
        return Ok(());
    }
    for table in tables {
        let columns = database.columns(&table)?;
        let definitions = columns
            .iter()
            .map(column_definition)
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            output,
            "CREATE TABLE {} ({});",
            quote_identifier(&table),
            definitions
        )?;
    }
    Ok(())
}

fn column_definition(column: &Column) -> String {
    let mut definition = format!(
        "{} {}",
        quote_identifier(&column.name),
        column_type_name(&column.ty)
    );
    if column.primary_key {
        definition.push_str(" PRIMARY KEY");
    } else if column.unique {
        definition.push_str(" UNIQUE");
    }
    if column.not_null && !column.primary_key {
        definition.push_str(" NOT NULL");
    }
    definition
}

fn column_type_name(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Integer => "INTEGER",
        ColumnType::Real => "REAL",
        ColumnType::Text => "TEXT",
        ColumnType::Boolean => "BOOLEAN",
        ColumnType::Any => "ANY",
        ColumnType::Null => "NULL",
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Text(value) => value.clone(),
        Value::Boolean(value) => value.to_string(),
    }
}

fn table_widths(columns: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = columns.iter().map(|value| value.chars().count()).collect();
    for row in rows {
        if widths.len() < row.len() {
            widths.resize(row.len(), 0);
        }
        for (width, value) in widths.iter_mut().zip(row) {
            *width = (*width).max(value.chars().count());
        }
    }
    widths
}

fn padded_row(values: &[String], widths: &[usize]) -> String {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let width = widths.get(index).copied().unwrap_or(0);
            let padding = if index + 1 == values.len() {
                0
            } else {
                width.saturating_sub(value.chars().count())
            };
            format!("{value}{}", " ".repeat(padding))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn separator_row(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("-+-")
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) if value.is_finite() => value.to_string(),
        Value::Real(_) => "null".into(),
        Value::Text(value) => json_string(value),
        Value::Boolean(value) => value.to_string(),
    }
}

/// Return the byte offset just after the first top-level semicolon.
fn top_level_semicolon(input: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut parentheses = 0usize;
    let mut quote = None;
    let mut line_comment = false;
    let mut block_comment = false;

    while i < bytes.len() {
        let byte = bytes[i];
        if line_comment {
            if byte == b'\n' {
                line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_comment {
            if byte == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if byte == delimiter {
                if bytes.get(i + 1) == Some(&delimiter) {
                    i += 2;
                } else {
                    quote = None;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        match byte {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_comment = true;
                i += 2;
            }
            b'\'' | b'"' | b'[' => {
                quote = Some(if byte == b'[' { b']' } else { byte });
                i += 1;
            }
            b'(' => {
                parentheses += 1;
                i += 1;
            }
            b')' => {
                parentheses = parentheses.saturating_sub(1);
                i += 1;
            }
            b';' if parentheses == 0 => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn sql_has_open_construct(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut parentheses = 0usize;
    let mut quote = None;
    let mut line_comment = false;
    let mut block_comment = false;

    while i < bytes.len() {
        let byte = bytes[i];
        if line_comment {
            if byte == b'\n' {
                line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_comment {
            if byte == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if byte == delimiter {
                if bytes.get(i + 1) == Some(&delimiter) {
                    i += 2;
                } else {
                    quote = None;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        match byte {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_comment = true;
                i += 2;
            }
            b'\'' | b'"' | b'[' => {
                quote = Some(if byte == b'[' { b']' } else { byte });
                i += 1;
            }
            b'(' => {
                parentheses += 1;
                i += 1;
            }
            b')' => {
                parentheses = parentheses.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    parentheses > 0 || quote.is_some() || block_comment
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn parses_options_and_preserves_action_order() {
        let options = parse_args(&args(&[
            "--json",
            "-c",
            "SELECT 1",
            "-f=seed.sql",
            "--no-header",
            "demo.db",
        ]))
        .unwrap();
        assert_eq!(options.database, "demo.db");
        assert_eq!(options.output, OutputMode::Json);
        assert!(!options.headers);
        assert_eq!(
            options.actions,
            vec![
                InputAction::Command("SELECT 1".into()),
                InputAction::File("seed.sql".into())
            ]
        );
    }

    #[test]
    fn sql_scanner_ignores_nested_literals_and_comments() {
        let sql = "SELECT '(' AS x /* ; */; SELECT \";\" AS y;";
        assert_eq!(top_level_semicolon(sql), Some(24));
        assert!(!sql_has_open_construct("SELECT (1 + 2)"));
        assert!(sql_has_open_construct("SELECT (1 + 2"));
        assert!(sql_has_open_construct("SELECT 'unfinished"));
    }

    #[test]
    fn renders_json_and_csv_without_external_serializers() {
        let results = vec![StatementResult::Select {
            columns: vec!["name".into(), "note".into()],
            rows: vec![vec![
                Value::Text("Ada".into()),
                Value::Text("say, \"hi\"".into()),
            ]],
        }];
        let mut json = Vec::new();
        render_results(&results, OutputMode::Json, true, false, &mut json).unwrap();
        assert_eq!(
            String::from_utf8(json).unwrap(),
            "{\"type\":\"select\",\"columns\":[\"name\",\"note\"],\"rows\":[[\"Ada\",\"say, \\\"hi\\\"\"]]}\n"
        );
        let mut csv = Vec::new();
        render_results(&results, OutputMode::Csv, true, false, &mut csv).unwrap();
        assert_eq!(
            String::from_utf8(csv).unwrap(),
            "name,note\nAda,\"say, \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn command_mode_uses_one_connection_for_transactions() {
        let options = parse_args(&args(&[
            "-c",
            "CREATE TABLE t (id INTEGER);",
            "-c",
            "BEGIN;",
            "-c",
            "INSERT INTO t VALUES (1);",
            "-c",
            "ROLLBACK;",
            "-c",
            "SELECT * FROM t;",
            "--json",
        ]))
        .unwrap();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        run(&options, Database::in_memory(), &mut input, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\"rows\":[]"));
    }
}
