//! MCP server frontend for agents that need to inspect and operate a Basalt database.
//!
//! The wire protocol is provided by the official `rmcp` implementation.  This
//! module only owns the database-facing tools, bounded result conversion, and
//! the small `basalt mcp` command-line parser.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    Implementation, ListResourcesResult, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData, Json, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::database::{Connection, Database};
use crate::db::{Column, StatementResult};
use crate::sql::ast::Statement;
use crate::sql::parser::parse;
use crate::types::{ColumnType, Value};

pub const HELP: &str = "Basalt MCP server\n\n\
Usage:\n  basalt mcp [OPTIONS] [DATABASE_PATH | :memory:]\n\n\
Options:\n  -d, --database PATH  Database path (default: :memory:)\n  -h, --help           Print this help\n\n\
The server speaks MCP over stdin/stdout. Diagnostics go to stderr.\n";

const DEFAULT_MAX_ROWS: usize = 100;
const MAX_ROWS: usize = 1_000;
const MAX_SQL_BYTES: usize = 1_048_576;
const MAX_STATEMENTS: usize = 100;
const MAX_OUTPUT_BYTES: usize = 1_048_576;
const SCHEMA_URI: &str = "basalt://schema";

/// Parsed options for the `basalt mcp` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOptions {
    pub database: String,
    pub help: bool,
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            database: ":memory:".into(),
            help: false,
        }
    }
}

/// Parse arguments after the `mcp` subcommand.
pub fn parse_args(args: &[String]) -> Result<McpOptions, McpCliError> {
    let mut options = McpOptions::default();
    let mut database = None;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let argument = args[index].as_str();
        if !positional_only && argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }

        if !positional_only {
            match argument {
                "-h" | "--help" => {
                    options.help = true;
                    index += 1;
                    continue;
                }
                "-d" | "--database" => {
                    index += 1;
                    let value = args.get(index).ok_or_else(|| {
                        McpCliError::new(format!("{argument} requires a value; try --help"))
                    })?;
                    set_database(&mut database, value)?;
                    index += 1;
                    continue;
                }
                _ => {}
            }

            if let Some(value) = argument.strip_prefix("--database=") {
                set_database(&mut database, value)?;
                index += 1;
                continue;
            }
            if argument.starts_with('-') {
                return Err(McpCliError::new(format!(
                    "unknown option {argument:?}; try --help"
                )));
            }
        }

        set_database(&mut database, argument)?;
        index += 1;
    }

    if let Some(database) = database {
        options.database = database;
    }
    Ok(options)
}

fn set_database(database: &mut Option<String>, value: &str) -> Result<(), McpCliError> {
    if value.is_empty() {
        return Err(McpCliError::new("database path cannot be empty"));
    }
    if database.is_some() {
        return Err(McpCliError::new(
            "database path provided more than once; choose one path",
        ));
    }
    *database = Some(value.into());
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCliError {
    message: String,
}

impl McpCliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for McpCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpCliError {}

/// Start the stdio MCP server for an already-open database.
pub fn run(database: Database) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("could not start async runtime: {error}"))?;

    runtime.block_on(async move {
        let server = BasaltMcp::new(database);
        let service = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|error| format!("could not start MCP transport: {error:?}"))?;
        service
            .waiting()
            .await
            .map(|_| ())
            .map_err(|error| format!("MCP transport stopped with an error: {error:?}"))
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SqlInput {
    /// SQL to execute. The server accepts up to 1 MiB and 100 statements per call.
    sql: String,
    /// Maximum rows returned for each SELECT result. Defaults to 100 and is capped at 1,000.
    #[serde(default)]
    max_rows: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct TableInput {
    /// Table name, matched case-insensitively.
    table: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct SqlResult {
    /// Results in statement order.
    results: Vec<StatementOutput>,
    /// Committed database generation after the call.
    generation: u64,
    /// Whether a transaction remains open on this MCP session's connection.
    transaction_open: bool,
    /// Milliseconds spent executing and converting the result.
    duration_ms: u64,
    /// Whether one or more SELECT results were capped by max_rows.
    rows_truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StatementOutput {
    Select {
        columns: Vec<String>,
        rows: Vec<Vec<OutputValue>>,
        rows_total: usize,
        truncated: bool,
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
    Explain {
        plan: String,
    },
    Begin,
    Commit,
    Rollback,
    Checkpoint,
    Echo {
        value: String,
    },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum OutputValue {
    Null,
    Integer(i64),
    Real(String),
    Text(String),
    Boolean(bool),
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListTablesResult {
    tables: Vec<TableInfo>,
    generation: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct TableInfo {
    name: String,
    columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ColumnInfo {
    name: String,
    r#type: String,
    not_null: bool,
    unique: bool,
    primary_key: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct CheckpointResult {
    generation: u64,
}

#[derive(Clone)]
struct BasaltMcp {
    database: Database,
    connection: Arc<Mutex<Connection>>,
    tool_router: ToolRouter<Self>,
}

impl BasaltMcp {
    fn new(database: Database) -> Self {
        let connection = database.connect();
        Self {
            database,
            connection: Arc::new(Mutex::new(connection)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl BasaltMcp {
    /// Read rows without allowing a query to mutate the database.
    #[tool(
        name = "query",
        description = "Run read-only SQL. Accepts SELECT and EXPLAIN SELECT only. Use execute for writes, DDL, transactions, and CHECKPOINT. Results are typed, capped by max_rows, and limited to a 1 MiB response.",
        annotations(
            title = "Read from Basalt",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn query(
        &self,
        Parameters(input): Parameters<SqlInput>,
    ) -> Result<Json<SqlResult>, String> {
        execute_sql(self.connection.clone(), input, true)
            .await
            .map(Json)
    }

    /// Execute arbitrary SQL on the session connection.
    #[tool(
        name = "execute",
        description = "Execute SQL against the configured Basalt database. Use for INSERT, UPDATE, DELETE, CREATE/DROP, transactions, CHECKPOINT, and SELECT when needed. Statements run in order on one connection; use BEGIN and COMMIT for an explicit multi-call transaction. This tool can change or delete data.",
        annotations(
            title = "Execute SQL",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn execute(
        &self,
        Parameters(input): Parameters<SqlInput>,
    ) -> Result<Json<SqlResult>, String> {
        execute_sql(self.connection.clone(), input, false)
            .await
            .map(Json)
    }

    /// List table names and column metadata.
    #[tool(
        name = "list_tables",
        description = "List every table and its column metadata in deterministic order. This reads committed schema state without returning table rows.",
        annotations(
            title = "List Basalt tables",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_tables(&self) -> Result<Json<ListTablesResult>, String> {
        let connection = self.connection.clone();
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || with_connection(&connection, |_| table_info(&database)))
            .await
            .map_err(|error| format!("table listing task failed: {error}"))?
            .and_then(|response| {
                ensure_output_size(&response, "table metadata")?;
                Ok(response)
            })
            .map(Json)
    }

    /// Describe one table.
    #[tool(
        name = "describe_table",
        description = "Return the columns and constraints for one table. Table names are matched case-insensitively.",
        annotations(
            title = "Describe a Basalt table",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn describe_table(
        &self,
        Parameters(input): Parameters<TableInput>,
    ) -> Result<Json<TableInfo>, String> {
        let connection = self.connection.clone();
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |_| {
                let name = database
                    .table_names()
                    .map_err(|error| format!("could not describe table: {error}"))?
                    .into_iter()
                    .find(|name| name.eq_ignore_ascii_case(&input.table))
                    .ok_or_else(|| {
                        format!("could not describe table: no such table: {}", input.table)
                    })?;
                let columns = database
                    .columns(&name)
                    .map_err(|error| format!("could not describe table: {error}"))?;
                let response = TableInfo {
                    name,
                    columns: columns.into_iter().map(column_info).collect(),
                };
                ensure_output_size(&response, "table metadata")?;
                Ok(response)
            })
        })
        .await
        .map_err(|error| format!("table description task failed: {error}"))?
        .map(Json)
    }

    /// Flush durable state to the snapshot and clear the WAL.
    #[tool(
        name = "checkpoint",
        description = "Flush committed state to the durable snapshot and clear old WAL frames. It is safe to call repeatedly and is a no-op for :memory: databases. It fails while an explicit transaction is open.",
        annotations(
            title = "Checkpoint Basalt",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkpoint(&self) -> Result<Json<CheckpointResult>, String> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                connection
                    .execute_sql("CHECKPOINT")
                    .map_err(|error| format!("checkpoint failed: {error}"))?;
                let response = CheckpointResult {
                    generation: connection.generation(),
                };
                ensure_output_size(&response, "checkpoint result")?;
                Ok(response)
            })
        })
        .await
        .map_err(|error| format!("checkpoint task failed: {error}"))?
        .map(Json)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BasaltMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("basalt", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Basalt is a local embedded SQL database. Use query for read-only SELECT or EXPLAIN SELECT. Use execute for writes and transaction control. Results are bounded; narrow SELECT projections or lower max_rows when needed.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(SCHEMA_URI, "schema")
                .with_title("Basalt schema")
                .with_description("Current table and column metadata as JSON.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != SCHEMA_URI {
            return Err(ErrorData::resource_not_found(
                "unknown Basalt resource",
                Some(serde_json::json!({ "uri": request.uri })),
            ));
        }

        let schema = with_connection(&self.connection, |_| schema_json(&self.database)).map_err(
            |error| ErrorData::internal_error(format!("could not read schema: {error}"), None),
        )?;

        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(schema, SCHEMA_URI).with_mime_type("application/json"),
        ])
        .into())
    }
}

async fn execute_sql(
    connection: Arc<Mutex<Connection>>,
    input: SqlInput,
    read_only: bool,
) -> Result<SqlResult, String> {
    let max_rows = row_limit(input.max_rows)?;
    validate_sql(&input.sql, read_only)?;
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let mut connection = connection
            .lock()
            .map_err(|_| "database connection lock poisoned".to_string())?;
        let results = connection
            .execute_sql(&input.sql)
            .map_err(|error| format!("SQL execution failed: {error}"))?;
        let (results, rows_truncated) = convert_results(results, max_rows);
        let response = SqlResult {
            results,
            generation: connection.generation(),
            transaction_open: connection.in_transaction(),
            duration_ms: started.elapsed().as_millis() as u64,
            rows_truncated,
        };
        ensure_output_size(&response, "SQL result")?;
        Ok(response)
    })
    .await
    .map_err(|error| format!("SQL execution task failed: {error}"))?
}

fn validate_sql(sql: &str, read_only: bool) -> Result<(), String> {
    if sql.is_empty() {
        return Err("SQL must not be empty".into());
    }
    if sql.len() > MAX_SQL_BYTES {
        return Err(format!(
            "SQL is {} bytes; request limit is {MAX_SQL_BYTES} bytes",
            sql.len()
        ));
    }
    let statements = parse(sql).map_err(|error| {
        format!(
            "SQL parse failed: {} at byte {}",
            error.message, error.offset
        )
    })?;
    if statements.is_empty() {
        return Err("SQL must contain at least one statement".into());
    }
    if statements.len() > MAX_STATEMENTS {
        return Err(format!(
            "request contains {}; limit is {MAX_STATEMENTS} statements",
            statements.len()
        ));
    }
    if read_only && statements.iter().any(|statement| !is_read_only(statement)) {
        return Err(
            "query accepts SELECT and EXPLAIN SELECT only; use execute for database changes".into(),
        );
    }
    Ok(())
}

fn is_read_only(statement: &Statement) -> bool {
    match statement {
        Statement::Select { .. } => true,
        Statement::Explain(inner) => matches!(inner.as_ref(), Statement::Select { .. }),
        _ => false,
    }
}

fn row_limit(value: Option<u64>) -> Result<usize, String> {
    let value = value.unwrap_or(DEFAULT_MAX_ROWS as u64);
    if value == 0 || value > MAX_ROWS as u64 {
        return Err(format!("max_rows must be between 1 and {MAX_ROWS}"));
    }
    Ok(value as usize)
}

fn convert_results(results: Vec<StatementResult>, max_rows: usize) -> (Vec<StatementOutput>, bool) {
    let mut rows_truncated = false;
    let results = results
        .into_iter()
        .map(|result| match result {
            StatementResult::Select { columns, rows } => {
                let rows_total = rows.len();
                let truncated = rows_total > max_rows;
                rows_truncated |= truncated;
                StatementOutput::Select {
                    columns,
                    rows: rows
                        .into_iter()
                        .take(max_rows)
                        .map(|row| row.into_iter().map(output_value).collect())
                        .collect(),
                    rows_total,
                    truncated,
                }
            }
            StatementResult::Insert { rows_affected } => StatementOutput::Insert { rows_affected },
            StatementResult::Update { rows_affected } => StatementOutput::Update { rows_affected },
            StatementResult::Delete { rows_affected } => StatementOutput::Delete { rows_affected },
            StatementResult::CreateTable { name } => StatementOutput::CreateTable { name },
            StatementResult::DropTable { name } => StatementOutput::DropTable { name },
            StatementResult::CreateIndex {
                name,
                table,
                column,
            } => StatementOutput::CreateIndex {
                name,
                table,
                column,
            },
            StatementResult::DropIndex { name } => StatementOutput::DropIndex { name },
            StatementResult::Explain(plan) => StatementOutput::Explain { plan },
            StatementResult::Begin => StatementOutput::Begin,
            StatementResult::Commit => StatementOutput::Commit,
            StatementResult::Rollback => StatementOutput::Rollback,
            StatementResult::Checkpoint => StatementOutput::Checkpoint,
            StatementResult::Echo(value) => StatementOutput::Echo { value },
        })
        .collect();
    (results, rows_truncated)
}

fn output_value(value: Value) -> OutputValue {
    match value {
        Value::Null => OutputValue::Null,
        Value::Integer(value) => OutputValue::Integer(value),
        Value::Real(value) => OutputValue::Real(value.to_string()),
        Value::Text(value) => OutputValue::Text(value),
        Value::Boolean(value) => OutputValue::Boolean(value),
    }
}

fn with_connection<T>(
    connection: &Arc<Mutex<Connection>>,
    operation: impl FnOnce(&mut Connection) -> Result<T, String>,
) -> Result<T, String> {
    let mut connection = connection
        .lock()
        .map_err(|_| "database connection lock poisoned".to_string())?;
    operation(&mut connection)
}

fn table_info(database: &Database) -> Result<ListTablesResult, String> {
    let tables = database
        .table_names()
        .map_err(|error| format!("could not list tables: {error}"))?
        .into_iter()
        .map(|name| {
            let columns = database
                .columns(&name)
                .map_err(|error| format!("could not read table {name}: {error}"))?;
            Ok(TableInfo {
                name,
                columns: columns.into_iter().map(column_info).collect(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(ListTablesResult {
        tables,
        generation: database.generation(),
    })
}

fn schema_json(database: &Database) -> Result<String, String> {
    let schema =
        serde_json::to_string_pretty(&table_info(database)?).map_err(|error| error.to_string())?;
    if schema.len() > MAX_OUTPUT_BYTES {
        return Err(format!(
            "schema is {} bytes; response limit is {MAX_OUTPUT_BYTES} bytes",
            schema.len()
        ));
    }
    Ok(schema)
}

fn ensure_output_size<T: Serialize>(value: &T, label: &str) -> Result<(), String> {
    let output_size = serde_json::to_vec(value)
        .map_err(|error| format!("could not encode {label}: {error}"))?
        .len();
    if output_size > MAX_OUTPUT_BYTES {
        return Err(format!(
            "{label} is {output_size} bytes; response limit is {MAX_OUTPUT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn column_info(column: Column) -> ColumnInfo {
    ColumnInfo {
        name: column.name,
        r#type: column_type_name(&column.ty).into(),
        not_null: column.not_null,
        unique: column.unique,
        primary_key: column.primary_key,
    }
}

fn column_type_name(column_type: &ColumnType) -> &'static str {
    match column_type {
        ColumnType::Integer => "INTEGER",
        ColumnType::Real => "REAL",
        ColumnType::Text => "TEXT",
        ColumnType::Boolean => "BOOLEAN",
        ColumnType::Any => "ANY",
        ColumnType::Null => "NULL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn mcp_defaults_to_in_memory() {
        assert_eq!(parse_args(&[]).unwrap().database, ":memory:");
    }

    #[test]
    fn mcp_accepts_positional_and_flag_database_paths() {
        assert_eq!(
            parse_args(&args(&["data.basalt"])).unwrap().database,
            "data.basalt"
        );
        assert_eq!(
            parse_args(&args(&["--database", "data.basalt"]))
                .unwrap()
                .database,
            "data.basalt"
        );
    }

    #[test]
    fn mcp_rejects_mutating_query() {
        let error = validate_sql("DELETE FROM users", true).unwrap_err();
        assert!(error.contains("SELECT"));
    }

    #[test]
    fn mcp_limits_rows_and_preserves_statement_order() {
        let (outputs, truncated) = convert_results(
            vec![StatementResult::Select {
                columns: vec!["id".into()],
                rows: vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            }],
            1,
        );
        assert!(truncated);
        let StatementOutput::Select {
            rows,
            rows_total,
            truncated,
            ..
        } = &outputs[0]
        else {
            panic!("expected select output")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(*rows_total, 2);
        assert!(*truncated);
    }

    #[test]
    fn mcp_encodes_non_finite_reals_as_text() {
        let encoded = serde_json::to_value(output_value(Value::Real(f64::INFINITY))).unwrap();
        assert_eq!(encoded, serde_json::json!({"type": "real", "value": "inf"}));
    }
}
