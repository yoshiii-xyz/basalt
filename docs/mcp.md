# Basalt MCP server

Basalt's MCP server is a local subprocess. An MCP host starts `basalt mcp`,
writes newline-delimited JSON-RPC messages to its stdin, and reads responses
from stdout. Basalt keeps stdout exclusively for MCP traffic; diagnostics go
to stderr.

The server uses the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)
Rust SDK for protocol framing and lifecycle handling. Its behavior follows the
MCP [stdio transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio)
and [tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)
specifications, including modern discovery requests and compatibility with the
legacy initialize handshake.

## Install

From a Basalt checkout:

```bash
cargo install --path .
```

Confirm the binary is available:

```bash
basalt mcp --help
```

The command requires Rust 1.88 or newer when building from source. If the MCP
host does not inherit the user's `PATH`, use the absolute path printed by
`command -v basalt` as the configuration's `command` value.

## Host configuration

Use the host's MCP server configuration file. The common local-server shape is:

```json
{
  "mcpServers": {
    "basalt": {
      "command": "/absolute/path/to/.cargo/bin/basalt",
      "args": ["mcp", "/absolute/path/to/app.basalt"]
    }
  }
}
```

Use `:memory:` instead of a file path when the database should disappear when
the MCP process exits:

```json
{
  "mcpServers": {
    "basalt": {
      "command": "basalt",
      "args": ["mcp", ":memory:"]
    }
  }
}
```

When a host must run from a checkout, use an absolute manifest path so its
working directory does not matter:

```json
{
  "mcpServers": {
    "basalt": {
      "command": "cargo",
      "args": [
        "run",
        "--quiet",
        "--manifest-path",
        "/absolute/path/to/basalt/Cargo.toml",
        "--",
        "mcp",
        "/absolute/path/to/app.basalt"
      ]
    }
  }
}
```

Prefer the installed binary: a host may restart an MCP server often, and
`cargo run` adds compile and dependency-resolution work to every startup.

## Tool contract

All SQL tools operate on one connection for the lifetime of the MCP process.
That makes transactions span separate calls:

1. Call `execute` with `BEGIN;`.
2. Call `execute` with the writes.
3. Call `execute` with `COMMIT;` or `ROLLBACK;`.

Available tools:

| Tool | Use | Mutates data |
| --- | --- | --- |
| `query` | `SELECT` and `EXPLAIN SELECT` only | No |
| `execute` | Writes, DDL, transactions, `CHECKPOINT`, and unrestricted SQL | Yes |
| `list_tables` | List tables and committed column metadata | No |
| `describe_table` | Inspect one table's committed column metadata | No |
| `checkpoint` | Flush a durable snapshot and clear old WAL frames | Filesystem state only |

`query` and `execute` accept:

```json
{
  "sql": "SELECT id, name FROM users ORDER BY id",
  "max_rows": 100
}
```

`max_rows` defaults to 100 and cannot exceed 1,000. The response includes
statement results in order, the committed generation, transaction state,
execution time, and whether rows were truncated. Each scalar is explicit, for
example `{ "type": "integer", "value": 1 }`, `{ "type": "real", "value": "1.5" }`,
or `{ "type": "null" }`. Real values are strings so non-finite results cannot
break JSON serialization and clients can choose their own numeric precision.
Responses larger than 1 MiB are rejected with an actionable error; narrow the
projection or lower `max_rows` when that happens. SQL input is limited to 1 MiB
and 100 statements per call.

The `basalt://schema` resource contains the current committed table and column
metadata as `application/json`. It is useful when an agent needs schema context
without spending a tool call on a query.

## Durability and safety

The MCP process has the same local filesystem access as the account that starts
it. A host should only launch Basalt for a database the user intends the agent
to access. `execute` can delete or change data; tool annotations identify that
risk, but the host remains responsible for approval policy.

Use a durable path for work that must survive process restarts. Basalt appends
committed writes to its WAL; call `checkpoint` when you want to fold the state
into the snapshot and remove old WAL frames. `checkpoint` is a no-op for
`:memory:`. A durable path is exclusively owned by one process; a second CLI
or MCP process receives an "already open" error instead of competing for the
same WAL.

## Troubleshooting

- Run `basalt mcp --help` directly to verify the installation.
- Use absolute paths for the database and, when necessary, the binary.
- Do not add CLI output flags such as `--json`; MCP owns stdout while the
  server is running.
- If a database is already open by another process, stop that process before
  diagnosing a file or WAL error.
