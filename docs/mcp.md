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

For the current `2026-07-28` protocol, a host can begin with
`server/discover`; subsequent requests carry the protocol version, client
identity, and client capabilities in their `_meta` object. Basalt also accepts
the older `initialize`/`initialized` exchange for legacy hosts. The release
smoke test exercises the modern path against the installed binary, while the
wire tests cover both paths.

## Install

From a Basalt checkout:

```bash
cargo install --path . --locked
```

Confirm the binary is available:

```bash
basalt mcp --help
```

The command requires Rust 1.88 or newer when building from source. If the MCP
host does not inherit the user's `PATH`, use the absolute path printed by
`command -v basalt` as the configuration's `command` value.

Claude Code can register a project-scoped server without hand-editing JSON:

```bash
claude mcp add basalt --scope project -- \
  basalt mcp --workspace "$PWD/.basalt-workspace"
```

This writes the project's `.mcp.json`; Claude Code may ask for approval before
using a project server. Use an absolute binary path when the host does not
inherit the shell's `PATH`. Keep `--allow-writes` out of shared configuration
unless the project has an explicit policy for agent imports, applies, and
undos; add it deliberately when that policy is approved. See the [Claude Code
MCP reference](https://docs.anthropic.com/en/docs/claude-code/mcp) for host
scope and registration details.

## Choose a mode

Use workspace mode for the product's agent workflow. It binds the server to a
versioned Basalt workspace and exposes only scoped workspace tools. The path is
the only data location the workspace tools can open:

```json
{
  "mcpServers": {
    "basalt": {
      "command": "/absolute/path/to/.cargo/bin/basalt",
      "args": ["mcp", "--workspace", "/absolute/path/to/project-data"]
    }
  }
}
```

Workspace mode is read-only with respect to data by default. `workspace_preview`
may create a local plan record, but it does not change database state. Add
`--allow-writes` only when the host configuration has an explicit approval
policy for applying plans and undoing changes:

```json
{
  "mcpServers": {
    "basalt": {
      "command": "basalt",
      "args": [
        "mcp",
        "--workspace",
        "/absolute/path/to/project-data",
        "--allow-writes"
      ]
    }
  }
}
```

Direct database mode remains available for compatibility with existing Basalt
users. It is also read-only by default; `execute` and `checkpoint` require
`--allow-writes`. Direct mode is not the scoped workspace workflow and exposes
stateful SQL transactions instead of preview/apply lifecycle tools.

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

For Cursor, save the same shape as a project `.cursor/mcp.json`. Cursor's CLI
also discovers project MCP configuration; its [MCP
documentation](https://docs.cursor.com/context/model-context-protocol) covers
the host's approval and auto-run settings. Basalt's `--allow-writes` flag is
still the server-side boundary and must be enabled separately.

Add `--allow-writes` to a direct database configuration only when unrestricted
SQL writes are intentionally approved:

```json
{
  "mcpServers": {
    "basalt": {
      "command": "basalt",
      "args": ["mcp", "/absolute/path/to/app.basalt", "--allow-writes"]
    }
  }
}
```

Use `:memory:` instead of a file path when the database should disappear when
the MCP process exits. This is useful for a read-only protocol smoke test or a
session whose data is supplied entirely through approved direct SQL:

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

Common tools in both modes are:

| Tool | Use | Mutates data |
| --- | --- | --- |
| `query` | `SELECT` and `EXPLAIN SELECT` only | No |
| `list_tables` | List tables and committed column metadata | No |
| `describe_table` | Inspect one table's committed column metadata | No |
| `checkpoint` | Flush a durable snapshot and clear old WAL frames | Filesystem state; requires approval |

Workspace mode also provides:

| Tool | Use | Mutates data |
| --- | --- | --- |
| `workspace_import` | Import bounded CSV, JSON, or JSON Lines content into a new table and create a recovery point | Yes; requires approval |
| `workspace_inspect` | Read workspace metadata, schema, and row counts | No |
| `workspace_preview` | Execute a bounded mutation in isolation and save its exact plan | Plan metadata only |
| `workspace_apply` | Apply one exact plan and create a recovery point | Yes; requires approval |
| `workspace_history` | Read the change ledger and recovery statuses | Recovery metadata may be reconciled |
| `workspace_diff` | Compare a change recovery point with current state | No |
| `workspace_undo` | Restore the latest committed change's recovery point | Yes; requires approval |
| `workspace_export` | Return one table as bounded CSV, JSON Lines, or SQL content | No |

`execute` is available only in direct database mode. It is absent from the
workspace tool list so agents do not see an operation that can bypass the
workspace lifecycle.

In workspace mode, use this sequence:

1. When data is not already present, call `workspace_import` with an explicit
   table, format, and content payload. The tool accepts only CSV, JSON, or
   JSON Lines, caps content at 16 MiB, never accepts a filesystem path, and
   returns a recoverable `change_id`.
2. Call `workspace_inspect` or `query` to understand the current data.
3. Call `workspace_preview` with the mutation. Keep the returned `plan_id`.
4. Have the operator approve the exact plan, then call `workspace_apply`.
5. Use `workspace_history` and `workspace_diff` to inspect the committed change.
6. Call `workspace_undo` with the latest change ID if the change should be
   reversed.

Workspace SQL calls open the workspace for one operation at a time. They do not
provide a multi-call SQL transaction; the durable plan and recovery lifecycle
is the transaction boundary. `workspace_apply` rejects stale plans and never
silently applies a mutation against a changed base state.

The import, apply, and undo calls are retry-safe for lost responses. Their
identifiers and persisted request metadata let an exact retry return the
original receipt when the workspace is still at the recorded post-operation
state. A failed import may also be retried when its exact base state is still
present; unresolved records and moved work are never replayed. If later work
moved the workspace, Basalt does not replay or discard that work. Tool
annotations remain hints; the state and identifier checks are the enforcement
boundary.

`workspace_import` is an explicit atomic ingress operation rather than a raw
SQL escape hatch. It requires `--allow-writes`, creates a new table, and stores
the pre-import recovery point in workspace history. An exact retry after a
lost response returns the original import receipt while the workspace remains
at the recorded result; it never imports the table twice. Use
`workspace_undo` to reverse it while it is the latest committed change. SQL
dump imports remain a CLI-only operation because they can contain arbitrary
DDL and DML; use the preview/apply lifecycle for SQL changes through MCP.

In direct database mode, all SQL tools operate on one connection for the
lifetime of the MCP process. When `--allow-writes` is enabled, transactions can
span separate calls:

1. Call `execute` with `BEGIN;`.
2. Call `execute` with the writes.
3. Call `execute` with `COMMIT;` or `ROLLBACK;`.

Available tools:

| Tool | Use | Mutates data |
| --- | --- | --- |
| `execute` | Writes, DDL, transactions, `CHECKPOINT`, and unrestricted SQL | Yes; requires approval |

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
it. A host should only launch Basalt for a database or workspace the user
intends the agent to access. Tool annotations are hints for host UIs; Basalt
also enforces the write flag itself. No host-specific approval behavior is
assumed.

Workspace import and export exchange content rather than accepting filesystem
paths. This keeps the workspace MCP surface from becoming an arbitrary
filesystem reader or writer. Imports are explicit, bounded, and recoverable;
the CLI commands remain available when a user intentionally chooses a source or
destination.

Use a durable path for work that must survive process restarts. Basalt appends
committed writes to its WAL; call `checkpoint` when you want to fold the state
into the snapshot and remove old WAL frames. `checkpoint` is a no-op for
`:memory:`, but still requires `--allow-writes` because it is a write-capable
operation in the protocol contract. A durable path is exclusively owned by one
process; a second CLI or MCP process receives an "already open" error instead
of competing for the same WAL. Workspace lifecycle operations likewise open
the underlying database one at a time.

## Troubleshooting

- Run `basalt mcp --help` directly to verify the installation.
- Use absolute paths for the database and, when necessary, the binary.
- Do not add CLI output flags such as `--json`; MCP owns stdout while the
  server is running.
- If a database is already open by another process, stop that process before
  diagnosing a file or WAL error.
