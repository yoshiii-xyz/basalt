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
Workspace mode:\n  --workspace PATH     Open a Basalt workspace (read-only by default)\n  --allow-writes       Enable workspace apply/undo and direct SQL writes\n\n\
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
    pub workspace: Option<String>,
    pub allow_writes: bool,
    pub help: bool,
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            database: ":memory:".into(),
            workspace: None,
            allow_writes: false,
            help: false,
        }
    }
}

/// Parse arguments after the `mcp` subcommand.
pub fn parse_args(args: &[String]) -> Result<McpOptions, McpCliError> {
    let mut options = McpOptions::default();
    let mut database = None;
    let mut workspace = None;
    let mut allow_writes = false;
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
                "--workspace" => {
                    index += 1;
                    let value = args.get(index).ok_or_else(|| {
                        McpCliError::new(format!("{argument} requires a value; try --help"))
                    })?;
                    set_workspace(&mut workspace, value)?;
                    index += 1;
                    continue;
                }
                "--allow-writes" | "--write" => {
                    allow_writes = true;
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
            if let Some(value) = argument.strip_prefix("--workspace=") {
                set_workspace(&mut workspace, value)?;
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

    if workspace.is_some() && database.is_some() {
        return Err(McpCliError::new(
            "choose --workspace or --database, not both",
        ));
    }
    if let Some(database) = database {
        options.database = database;
    }
    options.workspace = workspace;
    options.allow_writes = allow_writes;
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

fn set_workspace(workspace: &mut Option<String>, value: &str) -> Result<(), McpCliError> {
    if value.is_empty() {
        return Err(McpCliError::new("workspace path cannot be empty"));
    }
    if workspace.is_some() {
        return Err(McpCliError::new(
            "workspace path provided more than once; choose one path",
        ));
    }
    *workspace = Some(value.into());
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

#[derive(Clone)]
enum McpTarget {
    Database(Database),
    Workspace(crate::workspace::Workspace),
}

impl McpTarget {
    fn workspace(&self) -> Result<crate::workspace::Workspace, String> {
        match self {
            McpTarget::Workspace(workspace) => Ok(workspace.clone()),
            McpTarget::Database(_) => Err(
                "this tool requires `basalt mcp --workspace PATH`; direct database mode has no workspace lifecycle"
                    .to_string(),
            ),
        }
    }

    fn is_workspace(&self) -> bool {
        matches!(self, McpTarget::Workspace(_))
    }
}

/// Start the stdio MCP server for a direct database in read-only mode.
pub fn run(database: Database) -> Result<(), String> {
    run_database(database, false)
}

pub fn run_database(database: Database, allow_writes: bool) -> Result<(), String> {
    run_target(McpTarget::Database(database), allow_writes)
}

pub fn run_workspace(
    workspace: crate::workspace::Workspace,
    allow_writes: bool,
) -> Result<(), String> {
    run_target(McpTarget::Workspace(workspace), allow_writes)
}

fn run_target(target: McpTarget, allow_writes: bool) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("could not start async runtime: {error}"))?;

    runtime.block_on(async move {
        let server = BasaltMcp::new(target, allow_writes);
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct WorkspaceSqlInput {
    /// A write statement or statement sequence to preview.
    sql: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct WorkspaceImportInput {
    /// New table name for the imported rows. The table must not already exist.
    table: String,
    /// `csv`, `json`, or `jsonl`.
    format: String,
    /// UTF-8 input content. MCP imports are capped at 16 MiB and never read a filesystem path.
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct PlanInput {
    /// The exact plan identifier returned by workspace_preview.
    plan_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ChangeInput {
    /// A change identifier returned by workspace_apply or workspace_undo.
    change_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct DiffInput {
    /// Optional change identifier; defaults to the latest committed change.
    #[serde(default)]
    change_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ExportInput {
    /// Table name, matched case-insensitively.
    table: String,
    /// `csv`, `jsonl`, or `sql`.
    format: String,
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
    target: McpTarget,
    connection: Arc<Mutex<Connection>>,
    allow_writes: bool,
    tool_router: ToolRouter<Self>,
}

impl BasaltMcp {
    fn new(target: McpTarget, allow_writes: bool) -> Self {
        let connection_database = match &target {
            McpTarget::Database(database) => database.clone(),
            McpTarget::Workspace(_) => Database::in_memory(),
        };
        let connection = connection_database.connect();
        let mut tool_router = Self::tool_router();
        if target.is_workspace() {
            tool_router.remove_route("execute");
        } else {
            for tool in [
                "workspace_apply",
                "workspace_diff",
                "workspace_export",
                "workspace_history",
                "workspace_inspect",
                "workspace_import",
                "workspace_preview",
                "workspace_undo",
            ] {
                tool_router.remove_route(tool);
            }
        }
        Self {
            target,
            connection: Arc::new(Mutex::new(connection)),
            allow_writes,
            tool_router,
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
        if self.target.is_workspace() {
            execute_workspace_sql(self.target.workspace()?, input, true)
                .await
                .map(Json)
        } else {
            execute_sql(self.connection.clone(), input, true)
                .await
                .map(Json)
        }
    }

    /// Execute arbitrary SQL on the session connection.
    #[tool(
        name = "execute",
        description = "Execute SQL against a configured direct Basalt database. This tool is unavailable in workspace mode. Use for INSERT, UPDATE, DELETE, CREATE/DROP, transactions, CHECKPOINT, and SELECT when needed. Statements run in order on one connection; use BEGIN and COMMIT for an explicit multi-call transaction. This tool can change or delete data.",
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
        if self.target.is_workspace() {
            return Err(
                "direct SQL writes are disabled in workspace mode; use workspace_preview followed by workspace_apply"
                    .to_string(),
            );
        }
        if !self.allow_writes {
            return Err(
                "direct SQL writes are disabled; restart with --allow-writes after explicit operator approval"
                    .to_string(),
            );
        }
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
        let target = self.target.clone();
        tokio::task::spawn_blocking(move || match target {
            McpTarget::Database(database) => {
                with_connection(&connection, |_| table_info(&database))
            }
            McpTarget::Workspace(workspace) => {
                let database = workspace
                    .database()
                    .map_err(|error| format!("could not open workspace database: {error}"))?;
                table_info(&database)
            }
        })
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
        let target = self.target.clone();
        tokio::task::spawn_blocking(move || {
            let operation = |database: &Database| {
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
            };
            match target {
                McpTarget::Database(database) => {
                    with_connection(&connection, |_| operation(&database))
                }
                McpTarget::Workspace(workspace) => {
                    let database = workspace
                        .database()
                        .map_err(|error| format!("could not open workspace database: {error}"))?;
                    operation(&database)
                }
            }
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
        if !self.allow_writes {
            return Err(
                "checkpoint changes durable files; restart with --allow-writes after explicit operator approval"
                    .to_string(),
            );
        }
        let connection = self.connection.clone();
        let target = self.target.clone();
        tokio::task::spawn_blocking(move || match target {
            McpTarget::Database(_) => with_connection(&connection, |connection| {
                connection
                    .execute_sql("CHECKPOINT")
                    .map_err(|error| format!("checkpoint failed: {error}"))?;
                let response = CheckpointResult {
                    generation: connection.generation(),
                };
                ensure_output_size(&response, "checkpoint result")?;
                Ok(response)
            }),
            McpTarget::Workspace(workspace) => {
                let database = workspace
                    .database()
                    .map_err(|error| format!("could not open workspace database: {error}"))?;
                database
                    .checkpoint()
                    .map_err(|error| format!("checkpoint failed: {error}"))?;
                let response = CheckpointResult {
                    generation: database.generation(),
                };
                ensure_output_size(&response, "checkpoint result")?;
                Ok(response)
            }
        })
        .await
        .map_err(|error| format!("checkpoint task failed: {error}"))?
        .map(Json)
    }

    /// Inspect the configured workspace without returning table rows.
    #[tool(
        name = "workspace_inspect",
        description = "Inspect the configured Basalt workspace, including its format version, tables, columns, and row counts. This tool is available only in --workspace mode.",
        annotations(
            title = "Inspect Basalt workspace",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_inspect(&self) -> Result<Json<crate::workspace::InspectReport>, String> {
        let workspace = self.target.workspace()?;
        let response =
            tokio::task::spawn_blocking(move || crate::workspace::mcp_inspect(&workspace))
                .await
                .map_err(|error| format!("workspace inspection task failed: {error}"))?
                .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace inspection")?;
        Ok(Json(response))
    }

    /// Import bounded structured-data content into a workspace with recovery.
    #[tool(
        name = "workspace_import",
        description = "Import bounded UTF-8 CSV, JSON, or JSON Lines content into a new workspace table and create a recoverable change record. No filesystem path is accepted. Writes are disabled unless the MCP process was started with --allow-writes; use the CLI for SQL dump imports.",
        annotations(
            title = "Import workspace data",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn workspace_import(
        &self,
        Parameters(input): Parameters<WorkspaceImportInput>,
    ) -> Result<Json<crate::workspace::ImportReport>, String> {
        if !self.allow_writes {
            return Err(
                "workspace writes are disabled; restart with --allow-writes after explicit operator approval"
                    .to_string(),
            );
        }
        if input.content.len() > crate::workspace::MAX_MCP_IMPORT_BYTES {
            return Err(format!(
                "MCP import content exceeds the {} MiB limit",
                crate::workspace::MAX_MCP_IMPORT_BYTES / (1024 * 1024)
            ));
        }
        let workspace = self.target.workspace()?;
        let response = tokio::task::spawn_blocking(move || {
            crate::workspace::mcp_import(
                &workspace,
                Some(&input.table),
                &input.format,
                &input.content,
            )
        })
        .await
        .map_err(|error| format!("workspace import task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace import")?;
        Ok(Json(response))
    }

    /// Preview a workspace mutation and persist its exact plan.
    #[tool(
        name = "workspace_preview",
        description = "Preview a mutating SQL sequence in an isolated transaction and return an exact plan ID. The workspace data is not changed; apply the returned plan explicitly.",
        annotations(
            title = "Preview workspace write",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_preview(
        &self,
        Parameters(input): Parameters<WorkspaceSqlInput>,
    ) -> Result<Json<crate::workspace::PlanReport>, String> {
        let workspace = self.target.workspace()?;
        let response = tokio::task::spawn_blocking(move || {
            crate::workspace::mcp_preview(&workspace, &input.sql)
        })
        .await
        .map_err(|error| format!("workspace preview task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace preview")?;
        Ok(Json(response))
    }

    /// Apply one exact workspace plan when writes are enabled.
    #[tool(
        name = "workspace_apply",
        description = "Apply exactly one plan returned by workspace_preview. Writes are disabled unless the MCP process was started with --allow-writes; stale plans are rejected and a recovery point is created first.",
        annotations(
            title = "Apply workspace plan",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn workspace_apply(
        &self,
        Parameters(input): Parameters<PlanInput>,
    ) -> Result<Json<crate::workspace::ApplyReport>, String> {
        if !self.allow_writes {
            return Err(
                "workspace writes are disabled; restart with --allow-writes after explicit operator approval"
                    .to_string(),
            );
        }
        let workspace = self.target.workspace()?;
        let response = tokio::task::spawn_blocking(move || {
            crate::workspace::mcp_apply(&workspace, &input.plan_id)
        })
        .await
        .map_err(|error| format!("workspace apply task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace apply")?;
        Ok(Json(response))
    }

    /// Return the workspace change ledger.
    #[tool(
        name = "workspace_history",
        description = "List workspace apply and undo records, including recovery status. This tool is available only in --workspace mode.",
        annotations(
            title = "Read workspace history",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_history(&self) -> Result<Json<Vec<crate::workspace::HistoryEntry>>, String> {
        let workspace = self.target.workspace()?;
        let response =
            tokio::task::spawn_blocking(move || crate::workspace::mcp_history(&workspace))
                .await
                .map_err(|error| format!("workspace history task failed: {error}"))?
                .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace history")?;
        Ok(Json(response))
    }

    /// Compare a committed change recovery point with current workspace state.
    #[tool(
        name = "workspace_diff",
        description = "Compare a committed workspace change recovery point with the current state at table level. The result does not claim row-by-row patch precision.",
        annotations(
            title = "Diff workspace change",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_diff(
        &self,
        Parameters(input): Parameters<DiffInput>,
    ) -> Result<Json<crate::workspace::DiffReport>, String> {
        let workspace = self.target.workspace()?;
        tokio::task::spawn_blocking(move || {
            crate::workspace::mcp_diff(&workspace, input.change_id.as_deref())
        })
        .await
        .map_err(|error| format!("workspace diff task failed: {error}"))?
        .map_err(|error| error.to_string())
        .and_then(|response| {
            ensure_output_size(&response, "workspace diff")?;
            Ok(Json(response))
        })
    }

    /// Undo the latest committed workspace change when writes are enabled.
    #[tool(
        name = "workspace_undo",
        description = "Undo the latest committed workspace change by restoring its verified recovery point. Writes are disabled unless --allow-writes is enabled, and later work is never discarded implicitly.",
        annotations(
            title = "Undo workspace change",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn workspace_undo(
        &self,
        Parameters(input): Parameters<ChangeInput>,
    ) -> Result<Json<crate::workspace::UndoReport>, String> {
        if !self.allow_writes {
            return Err(
                "workspace writes are disabled; restart with --allow-writes after explicit operator approval"
                    .to_string(),
            );
        }
        let workspace = self.target.workspace()?;
        let response = tokio::task::spawn_blocking(move || {
            crate::workspace::mcp_undo(&workspace, &input.change_id)
        })
        .await
        .map_err(|error| format!("workspace undo task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace undo")?;
        Ok(Json(response))
    }

    /// Export one workspace table as bounded UTF-8 content.
    #[tool(
        name = "workspace_export",
        description = "Export one workspace table as bounded CSV, JSON Lines, or SQL content. The tool returns content instead of accepting an arbitrary filesystem path.",
        annotations(
            title = "Export workspace table",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_export(
        &self,
        Parameters(input): Parameters<ExportInput>,
    ) -> Result<Json<crate::workspace::ExportReport>, String> {
        let workspace = self.target.workspace()?;
        let response = tokio::task::spawn_blocking(move || {
            crate::workspace::mcp_export(&workspace, &input.table, &input.format)
        })
        .await
        .map_err(|error| format!("workspace export task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace export")?;
        Ok(Json(response))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BasaltMcp {
    fn get_info(&self) -> ServerInfo {
        let instructions = if self.target.is_workspace() {
            "Basalt workspace mode is local and read-only by default. Use workspace_import only for approved bounded CSV, JSON, or JSON Lines content, query or workspace_inspect to inspect data, workspace_preview to create an exact write plan, and workspace_apply only when writes are explicitly enabled. Use workspace_history, workspace_diff, and workspace_undo for recovery. Results are bounded."
        } else if self.allow_writes {
            "Basalt direct database mode has write access because --allow-writes was explicitly provided. Use query for read-only SELECT or EXPLAIN SELECT; use execute for writes and transaction control. Results are bounded."
        } else {
            "Basalt direct database mode is read-only. Use query for SELECT or EXPLAIN SELECT. Restart with --allow-writes only after explicit operator approval for direct SQL writes. Results are bounded."
        };
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("basalt", env!("CARGO_PKG_VERSION")))
        .with_instructions(instructions)
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

        let target = self.target.clone();
        let schema = match target {
            McpTarget::Database(database) => {
                with_connection(&self.connection, |_| schema_json(&database))
            }
            McpTarget::Workspace(workspace) => {
                let database = workspace.database().map_err(|error| {
                    ErrorData::internal_error(
                        format!("could not open workspace database: {error}"),
                        None,
                    )
                })?;
                schema_json(&database)
            }
        }
        .map_err(|error| {
            ErrorData::internal_error(format!("could not read schema: {error}"), None)
        })?;

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

async fn execute_workspace_sql(
    workspace: crate::workspace::Workspace,
    input: SqlInput,
    read_only: bool,
) -> Result<SqlResult, String> {
    let max_rows = row_limit(input.max_rows)?;
    validate_sql(&input.sql, read_only)?;
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let database = workspace
            .database()
            .map_err(|error| format!("workspace database open failed: {error}"))?;
        let results = database
            .execute_sql(&input.sql)
            .map_err(|error| format!("SQL execution failed: {error}"))?;
        let (results, rows_truncated) = convert_results(results, max_rows);
        let response = SqlResult {
            results,
            generation: database.generation(),
            transaction_open: false,
            duration_ms: started.elapsed().as_millis() as u64,
            rows_truncated,
        };
        ensure_output_size(&response, "SQL result")?;
        Ok(response)
    })
    .await
    .map_err(|error| format!("workspace SQL task failed: {error}"))?
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
        let options = parse_args(&[]).unwrap();
        assert_eq!(options.database, ":memory:");
        assert_eq!(options.workspace, None);
        assert!(!options.allow_writes);
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
    fn mcp_accepts_workspace_and_explicit_write_policy() {
        let options = parse_args(&args(&[
            "--workspace",
            ".basalt-workspace",
            "--allow-writes",
        ]))
        .unwrap();
        assert_eq!(options.workspace.as_deref(), Some(".basalt-workspace"));
        assert!(options.allow_writes);
        assert_eq!(options.database, ":memory:");
    }

    #[test]
    fn mcp_rejects_mixing_workspace_and_database() {
        let error = parse_args(&args(&[
            "--workspace",
            ".basalt-workspace",
            "--database",
            "app.basalt",
        ]))
        .unwrap_err();
        assert!(error.to_string().contains("workspace or --database"));
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
