<div align="center">
  <img src="assets/logo.png" alt="Basalt logo" width="128">
  <h1>Basalt</h1>
  <p>A CLI-first local SQL workspace for structured data and coding agents.</p>
  <p>
    <a href="https://github.com/joshiii-xyz/basalt/actions/workflows/ci.yml">
      <img src="https://github.com/joshiii-xyz/basalt/actions/workflows/ci.yml/badge.svg" alt="CI">
    </a>
    <a href="LICENSE">
      <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license">
    </a>
  </p>
</div>

Basalt is an embedded SQL database and command-line application built from
scratch in Rust. It provides a small library API, an interactive shell,
durable storage, snapshot-isolated transactions, crash recovery, portable
structured-data workspaces, and a stdio MCP server for local AI agents. It is
not a SQLite-compatible replacement or a hosted database.

## Highlights

- SQL lexer and recursive-descent parser with expressions, joins, grouping,
  aggregates, aliases, and transaction statements.
- Atomic statement execution with primary-key, UNIQUE, and user-created
  indexes.
- Snapshot-isolated transactions with optimistic conflict detection.
- Checksummed page snapshots and a write-ahead log that recovers committed
  state after a process crash.
- Simple query planning with table scans, equality indexes, and range indexes.
- Interactive and scriptable CLI output in table, CSV, and JSON-lines formats.
- Portable workspaces with versioned metadata and atomic CSV, JSON/JSONL, and
  SQL dump import/export.
- Installable MCP server with typed SQL tools, bounded workspace imports and
  exports, engine-bounded SQL, schema resources, and recoverable agent changes.

## Installation

Rust 1.88 or newer is required.

```bash
cargo install --path . --locked
```

The published package name is `basalt-db`; the installed command remains
`basalt`.

Tagged releases also include checksummed installers and prebuilt binaries for
Linux, macOS, and Windows. After the first release is published, a normal
desktop install can use:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/joshiii-xyz/basalt/releases/latest/download/basalt-db-installer.sh | sh
```

The Rust package is not published yet. When it is, the package install path
will be `cargo install basalt-db --locked`.

To run directly from a checkout:

```bash
cargo run --release -- app.basalt
```

## Quick start

Open a database and run SQL interactively:

```console
$ basalt app.basalt
basalt> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
basalt> INSERT INTO users VALUES (1, 'Ada');
basalt> SELECT * FROM users;
id | name
---+-----
1  | Ada
1 row(s)
```

For a one-shot command:

```bash
basalt --json --command "SELECT * FROM users ORDER BY id;" app.basalt
```

Use `Database::in_memory()` for an ephemeral database. Durable writes are
appended to the WAL immediately; call `checkpoint()` to fold the current state
into the snapshot and clear old WAL frames. A durable path is owned by one
process at a time; cloned `Database` handles share that owner safely across
threads, while a second process receives an "already open" error.

## Workspaces

Use a workspace when an agent or script needs a disposable, local relational
area for CSV, JSON, logs, issue exports, or fixtures:

```bash
basalt init .basalt-workspace
basalt workspace import --table issues .basalt-workspace issues.csv
basalt workspace inspect --json .basalt-workspace
basalt workspace query --json .basalt-workspace "SELECT * FROM issues ORDER BY id"
basalt workspace export .basalt-workspace issues issues.jsonl
```

Imports are atomic and exports are deterministic. Add `--json` to workspace
import/export commands when an agent or script needs a machine-readable
operation report; raw exports to `-` remain clean data streams. Workspace MCP
imports create recoverable workspace changes; CLI imports remain the direct
local ingestion path. Later writes can be previewed, applied by exact plan ID,
inspected in history, diffed with schema and row-change counts, and undone when
they are the latest change. A workspace is owned by one Basalt process while
open, so stop a
workspace MCP server before using that workspace from the CLI or by opening its
`data.basalt` file directly. See [docs/workspaces.md](docs/workspaces.md) for
the format and boundaries.

The reason to use Basalt for agent-owned data is the write boundary: inspect a
proposed change before it is durable, apply only the exact reviewed plan, then
diff or undo the latest change if needed.

```bash
basalt workspace preview --json .basalt-workspace \
  "UPDATE issues SET status = 'closed' WHERE id = 42"
# Review the returned plan_id, then:
basalt workspace apply --json .basalt-workspace PLAN_ID
# Review the returned change_id, then:
basalt workspace diff --json .basalt-workspace CHANGE_ID
basalt workspace undo --json .basalt-workspace CHANGE_ID
```

Use SQLite or DuckDB when you need their compatibility or analytical
performance. Basalt is for local structured-data work where a bounded,
recoverable write matters more than replacing an existing database.

If that describes your workflow, use the [early-user validation
guide](docs/early-user-validation.md) with a disposable, non-sensitive input
and record the concrete task and blocker. Basalt does not claim adoption until
developers complete this workflow against the tools they already use.

## MCP server

Basalt can run as a local [Model Context Protocol](https://modelcontextprotocol.io/)
server over stdio. Install the binary from this checkout:

```bash
cargo install --path . --locked
```

Then configure an MCP host with an absolute workspace path. Workspace mode is
the recommended agent integration: it scopes data access and requires an
explicit preview/apply lifecycle for writes.

```json
{
  "mcpServers": {
    "basalt": {
      "command": "basalt",
      "args": [
        "mcp",
        "--workspace",
        "/absolute/path/to/project-data",
        "--init-workspace"
      ]
    }
  }
}
```

`--init-workspace` creates the configured workspace only when its path does not
exist; it never replaces an existing directory or manifest. Omit it when the
workspace must be provisioned separately. Add `"--allow-writes"` only when the
host has an explicit operator approval policy for applying workspace plans and
undoing changes. Direct database mode is still available with `"args": ["mcp",
"/absolute/path/to/app.basalt"]`, but it is read-only by default; `execute` and
`checkpoint` require the same flag. Use `"args": ["mcp", ":memory:"]` for an
ephemeral direct-mode session. The installed binary is preferred for host
configuration; running from a checkout is also possible with `cargo run --quiet
-- mcp --workspace /absolute/path/to/project-data`.

When a modern MCP host advertises form elicitation, Basalt returns an
`input_required` approval request before each workspace import, apply, or undo
and executes only after the host retries with an explicit approval. Legacy
initialized hosts receive `elicitation/create`; hosts that do not advertise
elicitation use the explicit `--allow-writes` startup policy.

Workspace mode exposes `workspace_import`, `workspace_inspect`,
`workspace_preview`, `workspace_plan`, `workspace_apply`,
`workspace_history`, `workspace_diff`, `workspace_undo`, and
`workspace_export`, alongside bounded `query`, `list_tables`, and
`describe_table` tools. It also exposes the current schema at
`basalt://schema`. See [docs/mcp.md](docs/mcp.md) for the complete tool
contract, configuration details, approval boundary, and troubleshooting.

## CLI

Execute a SQL file:

```bash
basalt --file schema-and-seed.sql app.basalt
```

Run commands in order on one connection, including a transaction spanning
multiple commands:

```bash
basalt --command "BEGIN;" --command "INSERT INTO users VALUES (2, 'Grace');" --command "COMMIT;" app.basalt
```

Use `--file -` to read SQL from stdin. Repeat `--command` and `--file` as
needed; they execute in the order they appear. Table output is human-readable,
CSV emits query rows, and `--json` emits one JSON object per statement. Run
`.help` inside the shell for `.tables`, `.schema`, `.mode`, `.headers`,
`.checkpoint`, `.show`, and `.clear`.

## Library usage

```rust
use basalt::{Database, db::StatementResult};

let database = Database::open("example.basalt")?;
database.execute_sql(
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL); INSERT INTO users VALUES (1, 'Ada');",
)?;
let result = database.execute_sql("SELECT * FROM users WHERE id = 1")?;
assert!(matches!(result[0], StatementResult::Select { .. }));
database.checkpoint()?;
# Ok::<(), basalt::db::DbError>(())
```

Use `database.connect()` when SQL transaction statements need to span multiple
calls.

## Project layout

| Path | Purpose |
| --- | --- |
| src/sql/ | Lexer, parser, AST, and SQL dialect |
| src/engine.rs | Statement execution and query semantics |
| src/planner.rs | Access-path selection |
| src/db.rs, src/database.rs | Tables, constraints, transactions, and API |
| src/storage.rs, src/wal.rs | Snapshots, checksums, and recovery |
| src/cli.rs | Interactive and scripted command-line frontend |
| src/workspace.rs | Local workspace lifecycle and data interchange |
| src/mcp.rs | Stdio MCP server, agent tools, and schema resource |
| docs/sql.md | Supported SQL dialect and transaction semantics |
| docs/benchmark-results.md | Recorded workflow benchmark snapshot |
| docs/compatibility.md | File-format boundary and differential-test policy |
| docs/early-user-validation.md | Five-minute switching test and feedback template |
| docs/fuzzing.md | Parser and persistence fuzzing instructions |
| docs/mcp.md | MCP installation, configuration, and tool contract |
| docs/workspaces.md | Workspace layout and import/export contract |
| tests/ | Integration and crash-recovery coverage |
| benches/ | In-process engine throughput benchmark |
| scripts/benchmark_workspace.py | Reproducible workflow comparison harness |
| scripts/differential_sql.py | Supported-subset SQLite differential checks |
| scripts/mcp-smoke.py | Installed-binary writable MCP smoke test |
| scripts/verify-release-artifacts.py | Release archive checksum and contents check |
| scripts/release-check.sh | Packaged-crate and release preflight |
| scripts/smoke-test.sh | Installed-binary CLI and read-only MCP smoke test |
| fuzz/ | Optional libFuzzer parser and snapshot targets |

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
cargo bench --bench throughput
cargo package --locked
cargo build --release --locked
cargo audit
python3 scripts/benchmark_workspace.py --basalt target/release/basalt
```

For the complete Unix release preflight, use:

```bash
bash scripts/release-check.sh
```

It installs the exact packaged crate into a temporary prefix and runs the
installed-binary smoke journey. It also runs `cargo audit` and `dist plan`
when those tools are available.

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines and
[CHANGELOG.md](CHANGELOG.md) for project history. The release checklist is in
[docs/release.md](docs/release.md), including the generated release workflow
and clean-binary smoke test.

## License

MIT. See [LICENSE](LICENSE).
