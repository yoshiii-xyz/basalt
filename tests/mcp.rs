use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct McpProcess {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl McpProcess {
    fn start() -> Self {
        Self::start_with_database(":memory:")
    }

    fn start_with_database(database: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_basalt"))
            .args(["mcp", database])
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
    let mut server = McpProcess::start();
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
fn durable_database_survives_mcp_restart() {
    let path = unique_database_path();
    let path_string = path.to_str().expect("temporary path should be UTF-8");

    let mut server = McpProcess::start_with_database(path_string);
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
