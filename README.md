<div align="center">
  <img src="assets/logo.png" alt="Basalt logo" width="128">
  <h1>Basalt</h1>
  <p>A small, durable embedded SQL database written in Rust.</p>
  <p>
    <a href="https://github.com/joshiii-xyz/basalt/actions/workflows/ci.yml">
      <img src="https://github.com/joshiii-xyz/basalt/actions/workflows/ci.yml/badge.svg" alt="CI">
    </a>
    <a href="LICENSE">
      <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license">
    </a>
  </p>
</div>

Basalt is a dependency-free embedded SQL database and command-line application
built from scratch in Rust. It provides a small library API, an interactive
shell, durable storage, snapshot-isolated transactions, and crash recovery.
It focuses on readable end-to-end implementation, durable behavior, and
practical command-line use.

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

## Installation

Rust 1.85 or newer is required.

```bash
cargo install --path .
```

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
into the snapshot and clear old WAL frames.

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
| tests/ | Integration and crash-recovery coverage |
| benches/ | Throughput benchmark |

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo doc --no-deps
cargo bench --bench throughput
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines and
[CHANGELOG.md](CHANGELOG.md) for project history.

## License

MIT. See [LICENSE](LICENSE).
