//! MCP server frontend for agents that need to inspect and operate a Basalt database.
//!
//! The wire protocol is provided by the official `rmcp` implementation.  This
//! module only owns the database-facing tools, bounded result conversion, and
//! the small `basalt mcp` command-line parser.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::SinkExt;
use rmcp::handler::server::{
    router::tool::ToolRouter,
    tool::{InputResponses as ToolInputResponses, RequestState as ToolRequestState},
    wrapper::Parameters,
};
use rmcp::model::{
    CallToolResponse, CallToolResult, ElicitRequest, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationSchema, Implementation, InputRequest, InputRequiredResult,
    InputResponses, ListResourcesResult, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, RequestStateCodec, Resource, ResourceContents, SealOptions,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError};
use rmcp::{
    ErrorData, Json, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, FramedWrite};
use uuid::Uuid;

use crate::database::{Connection, Database};
use crate::db::{Column, StatementResult};
use crate::engine::MCP_EXECUTION_WORK_LIMIT;
use crate::sql::ast::Statement;
use crate::sql::parser::parse;
use crate::types::{ColumnType, Value};

pub const HELP: &str = "Basalt MCP server\n\n\
Usage:\n  basalt mcp [OPTIONS] [DATABASE_PATH | :memory:]\n\n\
Options:\n  -d, --database PATH  Database path (default: :memory:)\n  -h, --help           Print this help\n\n\
Workspace mode:\n  --workspace PATH     Open a Basalt workspace (read-only by default)\n  --init-workspace     Create --workspace PATH when it does not exist\n  --allow-writes       Enable workspace apply/undo and direct SQL writes\n\n\
The server speaks MCP over stdin/stdout. Diagnostics go to stderr.\n";

const DEFAULT_MAX_ROWS: usize = 100;
const MAX_ROWS: usize = 1_000;
const MAX_SQL_BYTES: usize = 1_048_576;
const MAX_STATEMENTS: usize = 100;
const MAX_MUTATING_STATEMENTS: usize = 32;
const MAX_OUTPUT_BYTES: usize = 1_048_576;
const MAX_MCP_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const SCHEMA_URI: &str = "basalt://schema";
const WRITE_APPROVAL_INPUT_KEY: &str = "basalt_write_approval";
const WRITE_APPROVAL_STATE_VERSION: u8 = 1;
const WRITE_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Parsed options for the `basalt mcp` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOptions {
    pub database: String,
    pub workspace: Option<String>,
    pub init_workspace: bool,
    pub allow_writes: bool,
    pub help: bool,
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            database: ":memory:".into(),
            workspace: None,
            init_workspace: false,
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
    let mut init_workspace = false;
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
                "--init-workspace" => {
                    init_workspace = true;
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
    if init_workspace && workspace.is_none() {
        return Err(McpCliError::new(
            "--init-workspace requires --workspace PATH",
        ));
    }
    if let Some(database) = database {
        options.database = database;
    }
    options.workspace = workspace;
    options.init_workspace = init_workspace;
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
            .serve(BoundedStdioTransport::new())
            .await
            .map_err(|error| format!("could not start MCP transport: {error:?}"))?;
        service
            .waiting()
            .await
            .map(|_| ())
            .map_err(|error| format!("MCP transport stopped with an error: {error:?}"))
    })
}

/// Stdio MCP transport with a bounded input frame.
///
/// rmcp's convenience `stdio()` transport uses an unbounded line reader. The
/// tool contracts below have smaller limits, but those limits are reached only
/// after JSON-RPC decoding. Keep malformed or hostile input from growing the
/// process buffer before that validation runs.
type StdioWriter =
    FramedWrite<tokio::io::Stdout, JsonRpcMessageCodec<TxJsonRpcMessage<RoleServer>>>;
type SharedStdioWriter = Arc<tokio::sync::Mutex<Option<StdioWriter>>>;

/// Keep syntax errors recoverable after rmcp's decoder has consumed their frame.
/// The transport handles decoder errors after each frame so syntax and incomplete-JSON
/// errors can be ignored without ending the input stream.
struct RecoveringJsonRpcMessageCodec<T> {
    inner: JsonRpcMessageCodec<T>,
}

impl<T> RecoveringJsonRpcMessageCodec<T> {
    fn new_with_max_length(max_length: usize) -> Self {
        Self {
            inner: JsonRpcMessageCodec::new_with_max_length(max_length),
        }
    }
}

impl<T: DeserializeOwned> Decoder for RecoveringJsonRpcMessageCodec<T> {
    type Item = T;
    type Error = JsonRpcMessageCodecError;

    fn decode(&mut self, buffer: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match self.inner.decode(buffer) {
            Err(JsonRpcMessageCodecError::Serde(error))
                if matches!(
                    error.classify(),
                    serde_json::error::Category::Syntax | serde_json::error::Category::Eof
                ) =>
            {
                tracing::debug!("ignoring unparsable MCP input: {error}");
                Ok(None)
            }
            result => result,
        }
    }
}

struct BoundedStdioTransport {
    read: BufReader<tokio::io::Stdin>,
    decoder: RecoveringJsonRpcMessageCodec<RxJsonRpcMessage<RoleServer>>,
    line_buf: Vec<u8>,
    write: SharedStdioWriter,
}

impl BoundedStdioTransport {
    fn new() -> Self {
        let write = FramedWrite::new(
            tokio::io::stdout(),
            JsonRpcMessageCodec::new_with_max_length(MAX_MCP_MESSAGE_BYTES),
        );
        Self {
            read: BufReader::new(tokio::io::stdin()),
            decoder: RecoveringJsonRpcMessageCodec::new_with_max_length(MAX_MCP_MESSAGE_BYTES),
            line_buf: Vec::new(),
            write: Arc::new(tokio::sync::Mutex::new(Some(write))),
        }
    }

    async fn read_frame(&mut self) -> Result<Option<BytesMut>, std::io::Error> {
        loop {
            let available = self.read.fill_buf().await?;
            if available.is_empty() {
                if self.line_buf.is_empty() {
                    return Ok(None);
                }
                let mut frame = BytesMut::from(self.line_buf.as_slice());
                frame.extend_from_slice(b"\n");
                self.line_buf.clear();
                return Ok(Some(frame));
            }

            let newline_offset = available.iter().position(|byte| *byte == b'\n');
            let bytes_to_consume = newline_offset.map_or(available.len(), |offset| offset + 1);
            let frame_length = self.line_buf.len() + newline_offset.unwrap_or(bytes_to_consume);
            if frame_length > MAX_MCP_MESSAGE_BYTES {
                self.read.consume(bytes_to_consume);
                self.line_buf.clear();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MCP input message exceeds the 32 MiB limit",
                ));
            }

            self.line_buf
                .extend_from_slice(&available[..bytes_to_consume]);
            self.read.consume(bytes_to_consume);

            if newline_offset.is_some() {
                let frame = BytesMut::from(self.line_buf.as_slice());
                self.line_buf.clear();
                return Ok(Some(frame));
            }
        }
    }
}

impl Transport<RoleServer> for BoundedStdioTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = Arc::clone(&self.write);
        async move {
            let mut write = write.lock().await;
            let Some(write) = write.as_mut() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "MCP stdio transport is closed",
                ));
            };
            write.send(item).await.map_err(Into::into)
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            let mut frame = (match self.read_frame().await {
                Ok(frame) => frame,
                Err(error) => {
                    tracing::error!("MCP stdio transport read failed: {error}");
                    return None;
                }
            })?;

            match self.decoder.decode(&mut frame) {
                Ok(Some(message)) => return Some(message),
                Ok(None) => continue,
                Err(JsonRpcMessageCodecError::Serde(error)) => match error.classify() {
                    serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                        tracing::debug!("ignoring unparsable MCP input: {error}");
                    }
                    serde_json::error::Category::Data | serde_json::error::Category::Io => {
                        tracing::debug!("MCP protocol error on incoming message: {error}");
                        let mut write = self.write.lock().await;
                        let write = write.as_mut()?;
                        let response = TxJsonRpcMessage::<RoleServer>::error(
                            ErrorData::invalid_request("Invalid request", None),
                            None,
                        );
                        if write.send(response).await.is_err() {
                            return None;
                        }
                    }
                },
                Err(error) => {
                    tracing::error!("MCP stdio transport decode failed: {error}");
                    return None;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut write = self.write.lock().await;
        drop(write.take());
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SqlInput {
    /// SQL to execute. The server accepts up to 1 MiB, 100 statements, 32 mutating statements, and a bounded execution budget per call.
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
    /// UTF-8 input content. MCP imports are capped at 16 MiB, 10,000 rows, 256 columns, and 1,000,000 cells; they never read a filesystem path.
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

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteApproval {
    /// Set to true only when the user approves the described workspace change.
    approved: bool,
}

rmcp::elicit_safe!(WriteApproval);

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WriteOperation {
    Import,
    Apply,
    Undo,
}

impl WriteOperation {
    fn name(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Apply => "apply",
            Self::Undo => "undo",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct WriteApprovalState {
    version: u8,
    operation: WriteOperation,
    identity: String,
}

enum WorkspaceWriteApproval {
    Approved,
    InputRequired(InputRequiredResult),
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
    workspace_operation_lock: Arc<Mutex<()>>,
    request_state_codec: RequestStateCodec,
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
                "workspace_plan",
                "workspace_preview",
                "workspace_undo",
            ] {
                tool_router.remove_route(tool);
            }
        }
        let mut request_state_key = Vec::with_capacity(32);
        request_state_key.extend_from_slice(Uuid::new_v4().as_bytes());
        request_state_key.extend_from_slice(Uuid::new_v4().as_bytes());
        let request_state_codec = RequestStateCodec::try_new(request_state_key)
            .expect("two UUIDs always provide a sufficiently long request-state key");
        Self {
            target,
            connection: Arc::new(Mutex::new(connection)),
            workspace_operation_lock: Arc::new(Mutex::new(())),
            request_state_codec,
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
        description = "Run bounded read-only SQL. Accepts SELECT and EXPLAIN SELECT only. Use execute for writes, DDL, transactions, and CHECKPOINT. Results are typed, capped by max_rows, limited to a 1 MiB response, and protected by an execution work budget.",
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
            execute_workspace_sql(
                self.target.workspace()?,
                input,
                true,
                self.workspace_operation_lock.clone(),
            )
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
        description = "Execute bounded SQL against a configured direct Basalt database. This tool is unavailable in workspace mode. Use for INSERT, UPDATE, DELETE, CREATE/DROP, transactions, CHECKPOINT, and SELECT when needed. Statements run in order on one connection; use BEGIN and COMMIT for an explicit multi-call transaction. This tool can change or delete data.",
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        tokio::task::spawn_blocking(move || match target {
            McpTarget::Database(database) => {
                with_connection(&connection, |_| table_info(&database))
            }
            McpTarget::Workspace(workspace) => {
                let _operation = lock_workspace_operations(&workspace_operation_lock)?;
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
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
                    let _operation = lock_workspace_operations(&workspace_operation_lock)?;
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        tokio::task::spawn_blocking(move || match target {
            McpTarget::Database(_) => with_connection(&connection, |connection| {
                connection
                    .execute_sql_with_budget("CHECKPOINT", MCP_EXECUTION_WORK_LIMIT)
                    .map_err(|error| format!("checkpoint failed: {error}"))?;
                let response = CheckpointResult {
                    generation: connection.generation(),
                };
                ensure_output_size(&response, "checkpoint result")?;
                Ok(response)
            }),
            McpTarget::Workspace(workspace) => {
                let _operation = lock_workspace_operations(&workspace_operation_lock)?;
                let database = workspace
                    .database()
                    .map_err(|error| format!("could not open workspace database: {error}"))?;
                database
                    .execute_sql_with_budget("CHECKPOINT", MCP_EXECUTION_WORK_LIMIT)
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_inspect(&workspace)
        })
        .await
        .map_err(|error| format!("workspace inspection task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace inspection")?;
        Ok(Json(response))
    }

    /// Import bounded structured-data content into a workspace with recovery.
    #[tool(
        name = "workspace_import",
        description = "Import bounded UTF-8 CSV, JSON, or JSON Lines content into a new workspace table and create a recoverable change record. Content is limited to 16 MiB, 10,000 rows, 256 columns, and 1,000,000 cells. No filesystem path is accepted. Writes are disabled unless the MCP process was started with --allow-writes; modern clients advertising form elicitation receive an input_required approval request and retry with the response, while legacy initialized clients receive elicitation/create. Use the CLI for SQL dump imports or larger imports.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<crate::workspace::ImportReport>(),
        annotations(
            title = "Import workspace data",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_import(
        &self,
        Parameters(input): Parameters<WorkspaceImportInput>,
        ToolInputResponses(input_responses): ToolInputResponses,
        ToolRequestState(request_state): ToolRequestState,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, String> {
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
        let identity = write_operation_identity(
            WriteOperation::Import,
            &[&input.format, &input.table, &input.content],
        );
        if let WorkspaceWriteApproval::InputRequired(result) = self
            .request_workspace_write_approval(
                &context,
                WriteOperation::Import,
                &identity,
                format!(
                    "Approve importing {} bytes of {} content into workspace table {:?}?",
                    input.content.len(),
                    input.format,
                    input.table
                ),
                request_state,
                input_responses,
            )
            .await?
        {
            return Ok(result.into());
        }
        let workspace = self.target.workspace()?;
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
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
        complete_json(response)
    }

    /// Preview a workspace mutation and persist its exact plan.
    #[tool(
        name = "workspace_preview",
        description = "Preview a mutating SQL sequence in an isolated transaction and return the exact SQL, impact summary, and plan ID. A workspace MCP plan may affect at most 10,000 rows. The workspace data is not changed; apply the returned plan explicitly.",
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_preview(&workspace, &input.sql, MAX_OUTPUT_BYTES)
        })
        .await
        .map_err(|error| format!("workspace preview task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace preview")?;
        Ok(Json(response))
    }

    /// Reload a persisted workspace plan by its stable identifier.
    #[tool(
        name = "workspace_plan",
        description = "Load one persisted workspace plan by ID and return its exact SQL, base state, and impact summary. Use this to recover review context after a restart; it never changes workspace data.",
        annotations(
            title = "Load workspace plan",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_plan(
        &self,
        Parameters(input): Parameters<PlanInput>,
    ) -> Result<Json<crate::workspace::PlanReport>, String> {
        let workspace = self.target.workspace()?;
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_plan(&workspace, &input.plan_id, MAX_OUTPUT_BYTES)
        })
        .await
        .map_err(|error| format!("workspace plan task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace plan")?;
        Ok(Json(response))
    }

    /// Apply one exact workspace plan when writes are enabled.
    #[tool(
        name = "workspace_apply",
        description = "Apply exactly one plan returned by workspace_preview. A workspace MCP plan may affect at most 10,000 rows. Writes are disabled unless the MCP process was started with --allow-writes; modern clients advertising form elicitation receive an input_required approval request and retry with the response, while legacy initialized clients receive elicitation/create. Stale plans are rejected and a recovery point is created first.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<crate::workspace::ApplyReport>(),
        annotations(
            title = "Apply workspace plan",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_apply(
        &self,
        Parameters(input): Parameters<PlanInput>,
        ToolInputResponses(input_responses): ToolInputResponses,
        ToolRequestState(request_state): ToolRequestState,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, String> {
        if !self.allow_writes {
            return Err(
                "workspace writes are disabled; restart with --allow-writes after explicit operator approval"
                .to_string(),
            );
        }
        let identity = write_operation_identity(WriteOperation::Apply, &[&input.plan_id]);
        if let WorkspaceWriteApproval::InputRequired(result) = self
            .request_workspace_write_approval(
                &context,
                WriteOperation::Apply,
                &identity,
                format!(
                    "Approve applying Basalt workspace plan {}? Review its exact SQL and impact with workspace_plan first.",
                    input.plan_id
                ),
                request_state,
                input_responses,
            )
            .await?
        {
            return Ok(result.into());
        }
        let workspace = self.target.workspace()?;
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_apply(&workspace, &input.plan_id)
        })
        .await
        .map_err(|error| format!("workspace apply task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace apply")?;
        complete_json(response)
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_history(&workspace)
        })
        .await
        .map_err(|error| format!("workspace history task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace history")?;
        Ok(Json(response))
    }

    /// Compare a committed change recovery point with current workspace state.
    #[tool(
        name = "workspace_diff",
        description = "Compare a committed workspace change recovery point with the current state at table level. The result does not claim row-by-row patch precision and refuses comparisons larger than 10,000 rows; use the CLI diff for larger workspaces.",
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
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
        description = "Undo the latest committed workspace change by restoring its verified recovery point. Writes are disabled unless --allow-writes is enabled; modern clients advertising form elicitation receive an input_required approval request and retry with the response, while legacy initialized clients receive elicitation/create. Later work is never discarded implicitly.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<crate::workspace::UndoReport>(),
        annotations(
            title = "Undo workspace change",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspace_undo(
        &self,
        Parameters(input): Parameters<ChangeInput>,
        ToolInputResponses(input_responses): ToolInputResponses,
        ToolRequestState(request_state): ToolRequestState,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, String> {
        if !self.allow_writes {
            return Err(
                "workspace writes are disabled; restart with --allow-writes after explicit operator approval"
                .to_string(),
            );
        }
        let identity = write_operation_identity(WriteOperation::Undo, &[&input.change_id]);
        if let WorkspaceWriteApproval::InputRequired(result) = self
            .request_workspace_write_approval(
                &context,
                WriteOperation::Undo,
                &identity,
                format!(
                    "Approve undoing the latest Basalt workspace change {}? Later work is never discarded implicitly.",
                    input.change_id
                ),
                request_state,
                input_responses,
            )
            .await?
        {
            return Ok(result.into());
        }
        let workspace = self.target.workspace()?;
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_undo(&workspace, &input.change_id)
        })
        .await
        .map_err(|error| format!("workspace undo task failed: {error}"))?
        .map_err(|error| error.to_string())?;
        ensure_output_size(&response, "workspace undo")?;
        complete_json(response)
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
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _operation = lock_workspace_operations(&workspace_operation_lock)
                .map_err(crate::workspace::WorkspaceError::Invalid)?;
            crate::workspace::mcp_export(&workspace, &input.table, &input.format, MAX_OUTPUT_BYTES)
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
            "Basalt workspace mode is local and read-only by default. Use workspace_import only for approved bounded CSV, JSON, or JSON Lines content, query or workspace_inspect to inspect data, workspace_preview to create an exact write plan, workspace_plan to reload a saved plan after a lost response or restart, and workspace_apply only when writes are explicitly enabled. Modern clients advertising form elicitation receive an input_required approval request before workspace imports, applies, and undos; legacy initialized clients receive elicitation/create. Use workspace_history, workspace_diff, and workspace_undo for recovery. Results are bounded."
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
        let connection = self.connection.clone();
        let workspace_operation_lock = self.workspace_operation_lock.clone();
        let schema = tokio::task::spawn_blocking(move || match target {
            McpTarget::Database(database) => {
                with_connection(&connection, |_| schema_json(&database))
            }
            McpTarget::Workspace(workspace) => {
                let _operation = lock_workspace_operations(&workspace_operation_lock)?;
                let database = workspace
                    .database()
                    .map_err(|error| format!("could not open workspace database: {error}"))?;
                schema_json(&database)
            }
        })
        .await
        .map_err(|error| ErrorData::internal_error(format!("schema task failed: {error}"), None))?
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
            .execute_sql_with_budget(&input.sql, MCP_EXECUTION_WORK_LIMIT)
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

impl BasaltMcp {
    async fn request_workspace_write_approval(
        &self,
        context: &RequestContext<RoleServer>,
        operation: WriteOperation,
        identity: &str,
        message: String,
        request_state: Option<String>,
        input_responses: Option<InputResponses>,
    ) -> Result<WorkspaceWriteApproval, String> {
        if !supports_form_elicitation(context) {
            return Ok(WorkspaceWriteApproval::Approved);
        }
        if uses_modern_mcp(context) {
            return self.request_modern_write_approval(
                operation,
                identity,
                message,
                request_state,
                input_responses,
            );
        }

        request_legacy_write_approval(context, message).await?;
        Ok(WorkspaceWriteApproval::Approved)
    }

    fn request_modern_write_approval(
        &self,
        operation: WriteOperation,
        identity: &str,
        message: String,
        request_state: Option<String>,
        input_responses: Option<InputResponses>,
    ) -> Result<WorkspaceWriteApproval, String> {
        match (request_state, input_responses) {
            (None, None) => Ok(WorkspaceWriteApproval::InputRequired(
                self.modern_write_approval_request(operation, identity, message)?,
            )),
            (None, Some(_)) => Err(
                "workspace write approval response is missing its request state; repeat the original tool call"
                    .to_string(),
            ),
            (Some(_), None) => Err(
                "workspace write approval response is missing input responses; repeat the original tool call"
                    .to_string(),
            ),
            (Some(request_state), Some(input_responses)) => {
                validate_modern_write_approval(
                    &self.request_state_codec,
                    operation,
                    identity,
                    &request_state,
                    &input_responses,
                )?;
                Ok(WorkspaceWriteApproval::Approved)
            }
        }
    }

    fn modern_write_approval_request(
        &self,
        operation: WriteOperation,
        identity: &str,
        message: String,
    ) -> Result<InputRequiredResult, String> {
        let schema = ElicitationSchema::from_type::<WriteApproval>()
            .map_err(|error| format!("could not build workspace write approval schema: {error}"))?;
        let mut input_requests = BTreeMap::new();
        input_requests.insert(
            WRITE_APPROVAL_INPUT_KEY.to_string(),
            InputRequest::Elicitation(ElicitRequest::new(
                ElicitRequestParams::FormElicitationParams {
                    meta: None,
                    message,
                    requested_schema: schema,
                },
            )),
        );
        let state = WriteApprovalState {
            version: WRITE_APPROVAL_STATE_VERSION,
            operation,
            identity: identity.to_string(),
        };
        let request_state = self
            .request_state_codec
            .seal_json_with(
                &state,
                &SealOptions::new()
                    .associated_data(identity.as_bytes())
                    .ttl(WRITE_APPROVAL_TIMEOUT),
            )
            .map_err(|error| format!("could not create workspace write approval state: {error}"))?;
        Ok(InputRequiredResult::new(
            Some(input_requests),
            Some(request_state),
        ))
    }
}

async fn request_legacy_write_approval(
    context: &RequestContext<RoleServer>,
    message: String,
) -> Result<(), String> {
    let schema = ElicitationSchema::from_type::<WriteApproval>()
        .map_err(|error| format!("could not build workspace write approval schema: {error}"))?;
    let response = context
        .peer
        .create_elicitation_with_timeout(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema: schema,
            },
            Some(WRITE_APPROVAL_TIMEOUT),
        )
        .await
        .map_err(|error| format!("workspace write approval request failed: {error}"))?;
    validate_write_approval_response(response)
}

fn validate_modern_write_approval(
    codec: &RequestStateCodec,
    operation: WriteOperation,
    identity: &str,
    request_state: &str,
    input_responses: &InputResponses,
) -> Result<(), String> {
    let state = codec
        .open_json_with::<WriteApprovalState>(request_state, identity.as_bytes())
        .map_err(|_| {
            "workspace write approval state is invalid or expired; repeat the original tool call"
                .to_string()
        })?;
    if state.version != WRITE_APPROVAL_STATE_VERSION
        || state.operation != operation
        || state.identity != identity
    {
        return Err(
            "workspace write approval does not match this operation; repeat the original tool call"
                .to_string(),
        );
    }
    let response = input_responses
        .get(WRITE_APPROVAL_INPUT_KEY)
        .ok_or_else(|| "workspace write approval response was not provided".to_string())?;
    let response: ElicitResult = serde_json::from_value(response.clone())
        .map_err(|error| format!("workspace write approval response was invalid: {error}"))?;
    validate_write_approval_response(response)
}

fn validate_write_approval_response(response: ElicitResult) -> Result<(), String> {
    match response.action {
        ElicitationAction::Accept => {
            let content = response.content.ok_or_else(|| {
                "workspace write approval was accepted without a response".to_string()
            })?;
            let approval: WriteApproval = serde_json::from_value(content)
                .map_err(|error| format!("workspace write approval was invalid: {error}"))?;
            if approval.approved {
                Ok(())
            } else {
                Err("workspace write was not approved by the user".to_string())
            }
        }
        ElicitationAction::Decline => Err("workspace write was declined by the user".to_string()),
        ElicitationAction::Cancel => Err("workspace write approval was cancelled".to_string()),
        _ => Err("workspace write approval returned an unknown action".to_string()),
    }
}

fn supports_form_elicitation(context: &RequestContext<RoleServer>) -> bool {
    context
        .client_capabilities()
        .and_then(|capabilities| capabilities.elicitation)
        .is_some_and(|capability| capability.form.is_some() || capability.url.is_none())
}

fn uses_modern_mcp(context: &RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|version| version.as_str() >= "2026-07-28")
}

fn write_operation_identity(operation: WriteOperation, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"basalt-mcp-write-approval-v1\0");
    hasher.update(operation.name().as_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn complete_json<T: Serialize>(value: T) -> Result<CallToolResponse, String> {
    let value = serde_json::to_value(value)
        .map_err(|error| format!("could not serialize structured tool output: {error}"))?;
    Ok(CallToolResult::structured(value).into())
}

async fn execute_workspace_sql(
    workspace: crate::workspace::Workspace,
    input: SqlInput,
    read_only: bool,
    workspace_operation_lock: Arc<Mutex<()>>,
) -> Result<SqlResult, String> {
    let max_rows = row_limit(input.max_rows)?;
    validate_sql(&input.sql, read_only)?;
    tokio::task::spawn_blocking(move || {
        let _operation = lock_workspace_operations(&workspace_operation_lock)?;
        let started = Instant::now();
        let database = workspace
            .database()
            .map_err(|error| format!("workspace database open failed: {error}"))?;
        let results = database
            .execute_sql_with_budget(&input.sql, MCP_EXECUTION_WORK_LIMIT)
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
    let mutating_statements = statements
        .iter()
        .filter(|statement| is_mutating_statement(statement))
        .count();
    if mutating_statements > MAX_MUTATING_STATEMENTS {
        return Err(format!(
            "request contains {mutating_statements} mutating statements; limit is {MAX_MUTATING_STATEMENTS}"
        ));
    }
    Ok(())
}

fn is_mutating_statement(statement: &Statement) -> bool {
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

fn lock_workspace_operations(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>, String> {
    lock.lock()
        .map_err(|_| "workspace operation lock poisoned".to_string())
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
        assert!(!options.init_workspace);
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
        assert!(!options.init_workspace);
        assert!(options.allow_writes);
        assert_eq!(options.database, ":memory:");
    }

    #[test]
    fn mcp_accepts_explicit_workspace_initialization() {
        let options = parse_args(&args(&[
            "--workspace",
            ".basalt-workspace",
            "--init-workspace",
        ]))
        .unwrap();
        assert_eq!(options.workspace.as_deref(), Some(".basalt-workspace"));
        assert!(options.init_workspace);
    }

    #[test]
    fn mcp_rejects_workspace_initialization_without_workspace_mode() {
        let error = parse_args(&args(&["--init-workspace"])).unwrap_err();
        assert!(error.to_string().contains("requires --workspace"));
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
    fn mcp_write_operation_identity_is_stable() {
        assert_eq!(
            write_operation_identity(WriteOperation::Apply, &["plan-123"]),
            "3c2c8d418c345803f69373bd26f1a8d76e892fd8b204d77dbbaafcbb2e4bb517"
        );
    }

    #[test]
    fn mcp_limits_mutating_statements_per_request() {
        let sql = (0..=MAX_MUTATING_STATEMENTS)
            .map(|index| format!("CREATE TABLE table_{index} (id INTEGER)"))
            .collect::<Vec<_>>()
            .join("; ");
        let error = validate_sql(&sql, false).unwrap_err();
        assert!(error.contains("mutating statements"));
        assert!(error.contains("limit is 32"));
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

    #[test]
    fn mcp_stdio_codec_rejects_oversized_frames() {
        let mut codec =
            JsonRpcMessageCodec::<serde_json::Value>::new_with_max_length(MAX_MCP_MESSAGE_BYTES);
        let mut input = tokio_util::bytes::BytesMut::with_capacity(MAX_MCP_MESSAGE_BYTES + 1);
        input.resize(MAX_MCP_MESSAGE_BYTES + 1, b'x');

        let error = tokio_util::codec::Decoder::decode(&mut codec, &mut input).unwrap_err();

        assert!(matches!(
            error,
            JsonRpcMessageCodecError::MaxLineLengthExceeded
        ));
    }

    #[test]
    fn mcp_stdio_codec_recovers_after_malformed_json() {
        let mut codec = RecoveringJsonRpcMessageCodec::<serde_json::Value>::new_with_max_length(
            MAX_MCP_MESSAGE_BYTES,
        );
        let mut input = BytesMut::from(&b"{not-json}\n{\"ok\":true}\n"[..]);

        assert!(Decoder::decode(&mut codec, &mut input).unwrap().is_none());
        assert_eq!(
            Decoder::decode(&mut codec, &mut input).unwrap(),
            Some(serde_json::json!({"ok": true}))
        );
    }

    #[test]
    fn mcp_stdio_codec_recovers_after_invalid_message_shape() {
        let mut codec =
            RecoveringJsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
                MAX_MCP_MESSAGE_BYTES,
            );
        let mut input = BytesMut::from(
            &br#"[]
{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
"#[..],
        );

        let error = Decoder::decode(&mut codec, &mut input).unwrap_err();
        assert!(matches!(
            error,
            JsonRpcMessageCodecError::Serde(error)
                if matches!(error.classify(), serde_json::error::Category::Data)
        ));
        assert!(Decoder::decode(&mut codec, &mut input).unwrap().is_some());
    }
}
