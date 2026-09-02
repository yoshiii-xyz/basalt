use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct TempDir {
    path: PathBuf,
}

static TEMP_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "basalt-mcp-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&path).expect("temporary directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct McpProcess {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    pending: HashMap<u64, Value>,
}

impl McpProcess {
    fn start() -> Self {
        Self::start_with_options(":memory:", false)
    }

    fn start_writable() -> Self {
        Self::start_with_options(":memory:", true)
    }

    fn start_with_database(database: &str) -> Self {
        Self::start_with_options(database, false)
    }

    fn start_writable_with_database(database: &str) -> Self {
        Self::start_with_options(database, true)
    }

    fn start_with_workspace(workspace: &Path, allow_writes: bool) -> Self {
        Self::start_with_workspace_env(workspace, allow_writes, None)
    }

    fn start_with_workspace_init(workspace: &Path, allow_writes: bool) -> Self {
        let workspace = workspace.to_str().expect("workspace path should be UTF-8");
        let mut command = Command::new(env!("CARGO_BIN_EXE_basalt"));
        command.args(["mcp", "--workspace", workspace, "--init-workspace"]);
        if allow_writes {
            command.arg("--allow-writes");
        }
        Self::spawn(command)
    }

    fn start_with_workspace_env(
        workspace: &Path,
        allow_writes: bool,
        environment: Option<(&str, &str)>,
    ) -> Self {
        let workspace = workspace.to_str().expect("workspace path should be UTF-8");
        let mut command = Command::new(env!("CARGO_BIN_EXE_basalt"));
        command.args(["mcp", "--workspace", workspace]);
        if allow_writes {
            command.arg("--allow-writes");
        }
        if let Some((key, value)) = environment {
            command.env(key, value);
        }
        Self::spawn(command)
    }

    fn start_with_options(database: &str, allow_writes: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_basalt"));
        command.args(["mcp", database]);
        if allow_writes {
            command.arg("--allow-writes");
        }
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("MCP server should start");
        let input = child.stdin.take().expect("MCP stdin should be piped");
        let output = BufReader::new(child.stdout.take().expect("MCP stdout should be piped"));
        Self {
            child,
            input,
            output,
            pending: HashMap::new(),
        }
    }

    fn send(&mut self, message: Value) {
        serde_json::to_writer(&mut self.input, &message).expect("request should serialize");
        self.input
            .write_all(b"\n")
            .expect("request should be framed");
        self.input.flush().expect("request should flush");
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        self.response(id)
    }

    fn response(&mut self, id: u64) -> Value {
        if let Some(message) = self.pending.remove(&id) {
            return message;
        }
        loop {
            let mut line = String::new();
            let bytes = self
                .output
                .read_line(&mut line)
                .expect("MCP stdout should be readable");
            assert_ne!(
                bytes, 0,
                "MCP server exited before responding to request {id}"
            );
            let message: Value = serde_json::from_str(&line).expect("stdout must contain JSON-RPC");
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
            if let Some(response_id) = message.get("id").and_then(Value::as_u64) {
                self.pending.insert(response_id, message);
            }
        }
    }

    fn close(mut self) {
        drop(self.input);
        let status = self.child.wait().expect("MCP server should stop after EOF");
        assert!(
            status.success(),
            "MCP server exited unsuccessfully: {status}"
        );
    }

    fn wait_without_response(mut self) -> std::process::ExitStatus {
        drop(self.input);
        self.child.wait().expect("MCP server should stop")
    }
}

fn result(message: &Value) -> &Value {
    assert!(
        message.get("error").is_none(),
        "unexpected JSON-RPC error: {message}"
    );
    &message["result"]
}

fn initialize_legacy(server: &mut McpProcess) {
    let response = server.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "basalt-integration-test", "version": "1.0.0"}
        }),
    );
    let initialized = result(&response);
    assert_eq!(initialized["serverInfo"]["name"], "basalt");
    assert!(initialized["capabilities"]["tools"].is_object());
    assert!(initialized["capabilities"]["resources"].is_object());
    server.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));
}

#[test]
fn serves_legacy_stdio_clients_with_tools_resources_and_stateful_transactions() {
    let mut server = McpProcess::start_writable();
    initialize_legacy(&mut server);

    let tools_response = server.request(2, "tools/list", json!({}));
    let tools = result(&tools_response);
    let tool_names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools should be an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool should have a name"))
        .collect();
    assert_eq!(
        tool_names,
        [
            "checkpoint",
            "describe_table",
            "execute",
            "list_tables",
            "query"
        ]
    );
    assert_eq!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "query")
            .unwrap()["annotations"]["readOnlyHint"],
        true
    );
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "query")
            .unwrap()["outputSchema"]
            .is_object()
    );

    let create = server.request(
        3,
        "tools/call",
        json!({
            "name": "execute",
            "arguments": {"sql": "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);"}
        }),
    );
    assert_ne!(result(&create)["isError"], true);

    let begin = server.request(
        4,
        "tools/call",
        json!({"name": "execute", "arguments": {"sql": "BEGIN;"}}),
    );
    assert_eq!(
        result(&begin)["structuredContent"]["transaction_open"],
        true
    );

    let insert = server.request(
        5,
        "tools/call",
        json!({
            "name": "execute",
            "arguments": {"sql": "INSERT INTO users VALUES (1, 'Ada');"}
        }),
    );
    assert_ne!(result(&insert)["isError"], true);

    let query = server.request(
        6,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT id, name FROM users", "max_rows": 10}
        }),
    );
    let select = &result(&query)["structuredContent"]["results"][0];
    assert_eq!(select["type"], "select");
    assert_eq!(select["rows_total"], 1);
    assert_eq!(select["rows"][0][0], json!({"type": "integer", "value": 1}));
    assert_eq!(
        select["rows"][0][1],
        json!({"type": "text", "value": "Ada"})
    );

    let rollback = server.request(
        7,
        "tools/call",
        json!({"name": "execute", "arguments": {"sql": "ROLLBACK;"}}),
    );
    assert_eq!(
        result(&rollback)["structuredContent"]["transaction_open"],
        false
    );

    let rejected = server.request(
        8,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "DELETE FROM users"}
        }),
    );
    assert_eq!(result(&rejected)["isError"], true);

    let resources_response = server.request(9, "resources/list", json!({}));
    let resources = result(&resources_response);
    assert_eq!(resources["resources"][0]["uri"], "basalt://schema");
    let schema_response = server.request(10, "resources/read", json!({"uri": "basalt://schema"}));
    let schema = result(&schema_response);
    let schema_text = schema["contents"][0]["text"]
        .as_str()
        .expect("schema resource should be text");
    let schema: Value = serde_json::from_str(schema_text).expect("schema resource should be JSON");
    assert_eq!(schema["tables"][0]["name"], "users");

    let tables = server.request(
        11,
        "tools/call",
        json!({"name": "list_tables", "arguments": {}}),
    );
    assert_eq!(
        result(&tables)["structuredContent"]["tables"][0]["name"],
        "users"
    );
    let description = server.request(
        12,
        "tools/call",
        json!({"name": "describe_table", "arguments": {"table": "USERS"}}),
    );
    assert_eq!(
        result(&description)["structuredContent"]["columns"][0]["name"],
        "id"
    );
    assert_eq!(result(&description)["structuredContent"]["name"], "users");

    server.close();
}

#[test]
fn serves_modern_discovery_requests_with_per_request_metadata() {
    let mut server = McpProcess::start();
    let metadata = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "basalt-modern-integration-test",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    });

    let discovery_response = server.request(1, "server/discover", json!({"_meta": metadata}));
    let discovery = result(&discovery_response);
    assert!(
        discovery["supportedVersions"]
            .as_array()
            .expect("discovery should list protocol versions")
            .iter()
            .any(|version| version == "2026-07-28")
    );

    let tools_response = server.request(2, "tools/list", json!({"_meta": metadata}));
    let tools = result(&tools_response);
    assert_eq!(tools["tools"].as_array().unwrap().len(), 5);
    let resources_response = server.request(3, "resources/list", json!({"_meta": metadata}));
    assert_eq!(
        result(&resources_response)["resources"][0]["uri"],
        "basalt://schema"
    );
    server.close();
}

#[test]
fn workspace_mcp_requires_approval_and_completes_reversible_journey() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");

    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let mut read_only = McpProcess::start_with_workspace(&workspace, false);
    initialize_legacy(&mut read_only);

    let tools = read_only.request(2, "tools/list", json!({}));
    let tool_names: Vec<&str> = result(&tools)["tools"]
        .as_array()
        .expect("workspace tools should be an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool should have a name"))
        .collect();
    assert_eq!(
        tool_names,
        [
            "checkpoint",
            "describe_table",
            "list_tables",
            "query",
            "workspace_apply",
            "workspace_diff",
            "workspace_export",
            "workspace_history",
            "workspace_import",
            "workspace_inspect",
            "workspace_preview",
            "workspace_undo"
        ]
    );
    assert!(!tool_names.contains(&"execute"));

    let workspace_import_tool = result(&tools)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "workspace_import")
        .expect("workspace_import should be listed");
    assert_eq!(workspace_import_tool["annotations"]["readOnlyHint"], false);
    assert_eq!(
        workspace_import_tool["annotations"]["destructiveHint"],
        false
    );
    assert_eq!(workspace_import_tool["annotations"]["idempotentHint"], true);

    for name in ["workspace_apply", "workspace_undo"] {
        let tool = result(&tools)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} should be listed"));
        assert_eq!(tool["annotations"]["readOnlyHint"], false);
        assert_eq!(tool["annotations"]["destructiveHint"], true);
        assert_eq!(tool["annotations"]["idempotentHint"], true);
    }

    let inspect = read_only.request(
        3,
        "tools/call",
        json!({"name": "workspace_inspect", "arguments": {}}),
    );
    assert!(
        result(&inspect)["structuredContent"]["tables"]
            .as_array()
            .expect("workspace tables should be an array")
            .is_empty()
    );

    let denied_import = read_only.request(
        4,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "csv",
                "content": "id,name\n1,Ada\n"
            }
        }),
    );
    assert_eq!(result(&denied_import)["isError"], true);
    assert!(
        result(&denied_import)["content"][0]["text"]
            .as_str()
            .expect("denial should include text")
            .contains("writes are disabled")
    );
    read_only.close();

    let mut writable = McpProcess::start_with_workspace(&workspace, true);
    initialize_legacy(&mut writable);
    let imported = writable.request(
        2,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "csv",
                "content": "id,name\n1,Ada\n"
            }
        }),
    );
    let import_change_id = result(&imported)["structuredContent"]["change_id"]
        .as_str()
        .expect("import should return a change ID")
        .to_owned();
    assert_eq!(
        result(&imported)["structuredContent"]["summary"],
        "table users (1 rows, 2 columns)"
    );

    let retried_import = writable.request(
        13,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "csv",
                "content": "id,name\n1,Ada\n"
            }
        }),
    );
    assert_ne!(
        result(&retried_import)["isError"],
        true,
        "import retry failed: {retried_import}"
    );
    assert_eq!(
        result(&retried_import)["structuredContent"],
        result(&imported)["structuredContent"]
    );

    let inspect = writable.request(
        3,
        "tools/call",
        json!({"name": "workspace_inspect", "arguments": {}}),
    );
    assert_eq!(
        result(&inspect)["structuredContent"]["tables"][0]["name"],
        "users"
    );
    assert_eq!(
        result(&inspect)["structuredContent"]["tables"][0]["rows"],
        1
    );

    let query = writable.request(
        4,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT name FROM users WHERE id = 1"}
        }),
    );
    assert_eq!(
        result(&query)["structuredContent"]["results"][0]["rows"][0][0],
        json!({"type": "text", "value": "Ada"})
    );

    let preview = writable.request(
        5,
        "tools/call",
        json!({
            "name": "workspace_preview",
            "arguments": {"sql": "UPDATE users SET name = 'Grace' WHERE id = 1"}
        }),
    );
    let plan_id = result(&preview)["structuredContent"]["plan_id"]
        .as_str()
        .expect("preview should return a plan ID")
        .to_owned();
    assert_eq!(
        result(&preview)["structuredContent"]["sql"],
        "UPDATE users SET name = 'Grace' WHERE id = 1"
    );
    assert_eq!(
        result(&preview)["structuredContent"]["mutating_statements"],
        1
    );

    let apply = writable.request(
        6,
        "tools/call",
        json!({
            "name": "workspace_apply",
            "arguments": {"plan_id": plan_id}
        }),
    );
    let change_id = result(&apply)["structuredContent"]["change_id"]
        .as_str()
        .expect("apply should return a change ID")
        .to_owned();

    let retried_apply = writable.request(
        7,
        "tools/call",
        json!({
            "name": "workspace_apply",
            "arguments": {"plan_id": plan_id}
        }),
    );
    assert_ne!(result(&retried_apply)["isError"], true);
    assert_eq!(
        result(&retried_apply)["structuredContent"]["change_id"],
        change_id
    );

    let moved_import = writable.request(
        14,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "csv",
                "content": "id,name\n1,Ada\n"
            }
        }),
    );
    assert_eq!(result(&moved_import)["isError"], true);
    assert!(
        result(&moved_import)["content"][0]["text"]
            .as_str()
            .expect("moved import rejection should include text")
            .contains("state moved")
    );

    let changed = writable.request(
        8,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT name FROM users"}
        }),
    );
    assert_eq!(
        result(&changed)["structuredContent"]["results"][0]["rows"][0][0],
        json!({"type": "text", "value": "Grace"})
    );

    let history = writable.request(
        9,
        "tools/call",
        json!({"name": "workspace_history", "arguments": {}}),
    );
    assert_eq!(
        result(&history)["structuredContent"][0]["change_id"],
        import_change_id
    );
    assert_eq!(
        result(&history)["structuredContent"][0]["status"],
        "committed"
    );
    assert_eq!(
        result(&history)["structuredContent"][1]["change_id"],
        change_id
    );
    assert_eq!(
        result(&history)["structuredContent"][1]["status"],
        "committed"
    );

    let diff = writable.request(
        10,
        "tools/call",
        json!({
            "name": "workspace_diff",
            "arguments": {"change_id": change_id}
        }),
    );
    assert_eq!(
        result(&diff)["structuredContent"]["precision"],
        "table-level logical comparison"
    );
    assert_eq!(result(&diff)["structuredContent"]["state_changed"], true);

    let undo = writable.request(
        11,
        "tools/call",
        json!({
            "name": "workspace_undo",
            "arguments": {"change_id": change_id}
        }),
    );
    assert_eq!(
        result(&undo)["structuredContent"]["undone_change_id"],
        change_id
    );

    let retried_undo = writable.request(
        12,
        "tools/call",
        json!({
            "name": "workspace_undo",
            "arguments": {"change_id": change_id}
        }),
    );
    assert_ne!(result(&retried_undo)["isError"], true);
    assert_eq!(
        result(&retried_undo)["structuredContent"]["undone_change_id"],
        change_id
    );

    let restored = writable.request(
        13,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT name FROM users"}
        }),
    );
    assert_eq!(
        result(&restored)["structuredContent"]["results"][0]["rows"][0][0],
        json!({"type": "text", "value": "Ada"})
    );

    let exported = writable.request(
        14,
        "tools/call",
        json!({
            "name": "workspace_export",
            "arguments": {"table": "users", "format": "csv"}
        }),
    );
    assert_eq!(
        result(&exported)["structuredContent"]["content"],
        "id,name\n1,Ada\n"
    );
    writable.close();
}

#[test]
fn workspace_mcp_owns_workspace_until_shutdown() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let mut server = McpProcess::start_with_workspace(&workspace, false);
    initialize_legacy(&mut server);

    let blocked = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "inspect", path_arg(&workspace)])
        .output()
        .expect("workspace inspect should run");
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("workspace is already open"),
        "unexpected concurrent workspace error: {blocked:?}"
    );

    let direct_database = workspace.join("data.basalt");
    let direct_blocked = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["--quiet", "-c", "SELECT 1", path_arg(&direct_database)])
        .output()
        .expect("direct database query should run");
    assert!(!direct_blocked.status.success());
    assert!(
        String::from_utf8_lossy(&direct_blocked.stderr).contains("workspace is already open"),
        "unexpected direct database error: {direct_blocked:?}"
    );

    server.close();
    let available = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "inspect", path_arg(&workspace)])
        .output()
        .expect("workspace inspect should run after MCP shutdown");
    assert!(
        available.status.success(),
        "workspace should reopen after MCP shutdown: {available:?}"
    );
}

#[test]
fn workspace_mcp_can_initialize_a_missing_workspace() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    assert!(!workspace.exists());

    let mut server = McpProcess::start_with_workspace_init(&workspace, false);
    initialize_legacy(&mut server);
    let inspect = server.request(
        2,
        "tools/call",
        json!({"name": "workspace_inspect", "arguments": {}}),
    );
    assert!(result(&inspect)["structuredContent"]["tables"].is_array());
    server.close();

    assert!(workspace.join("workspace.json").is_file());
    assert!(workspace.join("data.basalt").is_file());
}

#[test]
fn workspace_mcp_serializes_concurrent_requests() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let mut server = McpProcess::start_with_workspace(&workspace, false);
    initialize_legacy(&mut server);
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "workspace_inspect", "arguments": {}}
    }));
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "workspace_inspect", "arguments": {}}
    }));

    let first = server.response(2);
    let second = server.response(3);
    for response in [&first, &second] {
        let tool_result = result(response);
        assert_ne!(
            tool_result["isError"], true,
            "concurrent request failed: {response}"
        );
        assert!(tool_result["structuredContent"]["tables"].is_array());
    }
    server.close();
}

#[test]
fn workspace_mcp_import_is_reversible_and_rejects_sql_dumps() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let mut server = McpProcess::start_with_workspace(&workspace, true);
    initialize_legacy(&mut server);

    let sql = server.request(
        2,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "unsafe",
                "format": "sql",
                "content": "CREATE TABLE unsafe (id INTEGER);"
            }
        }),
    );
    assert_eq!(result(&sql)["isError"], true);
    assert!(
        result(&sql)["content"][0]["text"]
            .as_str()
            .expect("SQL import rejection should include text")
            .contains("CLI")
    );

    let imported = server.request(
        3,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "jsonl",
                "content": "{\"id\":1,\"name\":\"Ada\"}\n"
            }
        }),
    );
    let change_id = result(&imported)["structuredContent"]["change_id"]
        .as_str()
        .expect("import should return a change ID")
        .to_owned();

    let undone = server.request(
        4,
        "tools/call",
        json!({
            "name": "workspace_undo",
            "arguments": {"change_id": change_id}
        }),
    );
    assert_eq!(
        result(&undone)["structuredContent"]["undone_change_id"],
        change_id
    );

    let tables = server.request(
        5,
        "tools/call",
        json!({"name": "list_tables", "arguments": {}}),
    );
    assert!(
        result(&tables)["structuredContent"]["tables"]
            .as_array()
            .expect("tables should be an array")
            .is_empty()
    );
    server.close();
}

#[test]
fn workspace_mcp_export_rejects_large_tables_before_serializing_them() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let content = (1..=10_000).map(|id| format!("{id}\n")).collect::<String>();
    let mut server = McpProcess::start_with_workspace(&workspace, true);
    initialize_legacy(&mut server);
    let imported = server.request(
        2,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "events",
                "format": "csv",
                "content": format!("id\n{content}")
            }
        }),
    );
    assert_ne!(result(&imported)["isError"], true);
    let import_change_id = result(&imported)["structuredContent"]["change_id"]
        .as_str()
        .expect("import should return a change ID")
        .to_owned();
    server.close();

    let database = workspace.join("data.basalt");
    let appended = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args([
            "--command",
            "INSERT INTO events VALUES (10001)",
            path_arg(&database),
        ])
        .output()
        .expect("appending a row should run");
    assert!(
        appended.status.success(),
        "appending a row failed: {appended:?}"
    );

    let mut server = McpProcess::start_with_workspace(&workspace, true);
    initialize_legacy(&mut server);

    let exported = server.request(
        2,
        "tools/call",
        json!({
            "name": "workspace_export",
            "arguments": {"table": "events", "format": "csv"}
        }),
    );
    assert_eq!(result(&exported)["isError"], true);
    assert!(
        result(&exported)["content"][0]["text"]
            .as_str()
            .expect("export rejection should include text")
            .contains("limited to 10000 rows")
    );

    let diff = server.request(
        3,
        "tools/call",
        json!({
            "name": "workspace_diff",
            "arguments": {
                "change_id": import_change_id
            }
        }),
    );
    assert_eq!(result(&diff)["isError"], true);
    assert!(
        result(&diff)["content"][0]["text"]
            .as_str()
            .expect("diff rejection should include text")
            .contains("MCP diff is limited to 10000 rows")
    );
    server.close();
}

#[test]
fn workspace_mcp_import_rejects_an_oversized_row_payload_before_touching_state() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let content = (1..=10_001).map(|id| format!("{id}\n")).collect::<String>();
    let mut server = McpProcess::start_with_workspace(&workspace, true);
    initialize_legacy(&mut server);
    let imported = server.request(
        2,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "events",
                "format": "csv",
                "content": format!("id\n{content}")
            }
        }),
    );
    assert_eq!(result(&imported)["isError"], true);
    assert!(
        result(&imported)["content"][0]["text"]
            .as_str()
            .expect("import rejection should include text")
            .contains("limited to 10000 rows")
    );

    let inspect = server.request(
        3,
        "tools/call",
        json!({"name": "workspace_inspect", "arguments": {}}),
    );
    assert!(
        result(&inspect)["structuredContent"]["tables"]
            .as_array()
            .expect("workspace tables should be an array")
            .is_empty()
    );
    server.close();
}

#[test]
fn workspace_mcp_import_reconciles_an_interrupted_commit() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let init = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["workspace", "init", path_arg(&workspace)])
        .output()
        .expect("workspace init should run");
    assert!(init.status.success(), "workspace init failed: {init:?}");

    let mut crashing = McpProcess::start_with_workspace_env(
        &workspace,
        true,
        Some(("BASALT_CRASH_TEST_AFTER_IMPORT_CHECKPOINT", "1")),
    );
    initialize_legacy(&mut crashing);
    crashing.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "csv",
                "content": "id,name\n1,Ada\n"
            }
        }
    }));
    assert!(!crashing.wait_without_response().success());

    let mut recovered = McpProcess::start_with_workspace(&workspace, true);
    initialize_legacy(&mut recovered);
    let history = recovered.request(
        2,
        "tools/call",
        json!({"name": "workspace_history", "arguments": {}}),
    );
    assert_eq!(
        result(&history)["structuredContent"][0]["status"],
        "recovered"
    );
    let change_id = result(&history)["structuredContent"][0]["change_id"]
        .as_str()
        .expect("recovered import should have a change ID")
        .to_owned();

    let query = recovered.request(
        3,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT name FROM users"}
        }),
    );
    assert_eq!(
        result(&query)["structuredContent"]["results"][0]["rows"][0][0],
        json!({"type": "text", "value": "Ada"})
    );

    let retried_import = recovered.request(
        5,
        "tools/call",
        json!({
            "name": "workspace_import",
            "arguments": {
                "table": "users",
                "format": "csv",
                "content": "id,name\n1,Ada\n"
            }
        }),
    );
    assert_ne!(result(&retried_import)["isError"], true);
    assert_eq!(
        result(&retried_import)["structuredContent"]["change_id"],
        change_id
    );

    let undo = recovered.request(
        4,
        "tools/call",
        json!({
            "name": "workspace_undo",
            "arguments": {"change_id": change_id}
        }),
    );
    assert_eq!(
        result(&undo)["structuredContent"]["undone_change_id"],
        change_id
    );
    recovered.close();
}

#[test]
fn direct_mcp_writes_require_explicit_approval() {
    let mut server = McpProcess::start();
    initialize_legacy(&mut server);
    let denied = server.request(
        2,
        "tools/call",
        json!({
            "name": "execute",
            "arguments": {"sql": "CREATE TABLE denied (id INTEGER)"}
        }),
    );
    assert_eq!(result(&denied)["isError"], true);
    assert!(
        result(&denied)["content"][0]["text"]
            .as_str()
            .expect("denial should include text")
            .contains("--allow-writes")
    );
    server.close();
}

#[test]
fn durable_database_survives_mcp_restart() {
    let path = unique_database_path();
    let path_string = path.to_str().expect("temporary path should be UTF-8");

    let mut server = McpProcess::start_writable_with_database(path_string);
    initialize_legacy(&mut server);
    let setup = server.request(
        2,
        "tools/call",
        json!({
            "name": "execute",
            "arguments": {
                "sql": "CREATE TABLE durable (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO durable VALUES (1, 'saved');"
            }
        }),
    );
    assert_ne!(result(&setup)["isError"], true);
    let checkpoint = server.request(
        3,
        "tools/call",
        json!({"name": "checkpoint", "arguments": {}}),
    );
    assert_ne!(result(&checkpoint)["isError"], true);
    server.close();

    let mut server = McpProcess::start_with_database(path_string);
    initialize_legacy(&mut server);
    let query = server.request(
        2,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT note FROM durable"}
        }),
    );
    assert_eq!(
        result(&query)["structuredContent"]["results"][0]["rows"][0][0],
        json!({"type": "text", "value": "saved"})
    );
    server.close();
    remove_database_files(&path);
}

#[test]
fn tool_errors_are_recoverable_json_results() {
    let mut server = McpProcess::start_writable();
    initialize_legacy(&mut server);

    let missing_table = server.request(
        2,
        "tools/call",
        json!({
            "name": "execute",
            "arguments": {"sql": "SELECT * FROM missing_table"}
        }),
    );
    assert_eq!(result(&missing_table)["isError"], true);
    assert!(
        result(&missing_table)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such table")
    );

    let invalid_limit = server.request(
        3,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {"sql": "SELECT 1", "max_rows": 0}
        }),
    );
    assert_eq!(result(&invalid_limit)["isError"], true);

    let tables = server.request(
        4,
        "tools/call",
        json!({"name": "list_tables", "arguments": {}}),
    );
    assert!(result(&tables)["structuredContent"]["tables"].is_array());
    server.close();
}

fn unique_database_path() -> std::path::PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("basalt-mcp-{timestamp}.basalt"))
}

fn remove_database_files(path: &Path) {
    let _ = fs::remove_file(path);
    let wal_path = format!("{}.wal", path.display());
    let _ = fs::remove_file(wal_path);
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("test paths should be UTF-8")
}

fn unique_suffix() -> u128 {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u128;
    timestamp * 1_000_000 + sequence
}
