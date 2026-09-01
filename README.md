# Basalt

<p align="center">
  <img src="assets/logo.png" alt="Basalt logo" width="280">
</p>

Basalt is a dependency-free embedded SQL database and command-line application
written from scratch in Rust. It has a small public API, an interactive shell,
scriptable output formats, snapshot-isolated transactions, checksummed pages,
and a write-ahead log that recovers complete commits after a process crash.

[![CI](https://github.com/joshiii-xyz/basalt/actions/workflows/ci.yml/badge.svg)](https://github.com/joshiii-xyz/basalt/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Basalt is a small, focused project rather than a database compatibility
replacement. Its goal is a readable end-to-end implementation with durable
behavior, useful SQL, and a practical command-line interface.

## Quick start

~~~text
cargo run --release -- --help
cargo run --release -- app.basalt
~~~

Or install the binary locally:

~~~text
cargo install --path .
basalt --file schema.sql app.basalt
~~~

## What is included

1. **SQL frontend** — UTF-8 lexer, comments, quoted identifiers, recursive
   descent parser, precedence-aware expressions, DDL/DML, joins, grouping,
   aggregates, aliases, and transaction statements.
2. **Storage** — deterministic catalog/row serialization in a fixed-size page
   container with per-page CRC-32 validation and corruption errors.
3. **Executor** — scans, filters, projections, scalar functions,
   `INSERT ... SELECT`, `UPDATE`, `DELETE`, ordering, `DISTINCT`,
   `LIMIT/OFFSET`, and outer joins.
4. **Indexes** — hand-written B+tree indexes, primary keys, column-level and
   user-created UNIQUE indexes, and index maintenance across writes/reloads.
5. **Transactions** — private snapshot transactions, atomic statements,
   optimistic conflict detection, concurrent readers, and connection-scoped
   `BEGIN`/`COMMIT`/`ROLLBACK`.
6. **Durability** — append-only checksummed WAL frames, torn-tail handling,
   atomic checkpoint installation, and recovery of the newest committed state.
7. **Planner** — equality and range access-path selection with a simple cost
   comparison, plus nested-loop inner/left/right/full/cross joins and grouped
   aggregate execution.
8. **CLI frontend** — interactive and non-interactive execution, SQL script
   files, repeated commands, multiline buffering, table/CSV/JSON-lines output,
   schema discovery, and shell state controls.
9. **Proof** — unit and integration coverage for parser semantics, B+tree
   mutation, constraints, atomicity, joins, aggregates, persistence, WAL
   recovery, corruption boundaries, concurrent access, and the executable
   frontend.

## Library usage

```rust
use basalt::{Database, db::StatementResult};

let database = Database::open("example.basalt")?;
database.execute_sql(
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\
     INSERT INTO users VALUES (1, 'Ada');",
)?;
let result = database.execute_sql("SELECT * FROM users WHERE id = 1")?;
assert!(matches!(result[0], StatementResult::Select { .. }));
database.checkpoint()?;
# Ok::<(), basalt::db::DbError>(())
```

Use `Database::in_memory()` for an ephemeral database. Use
`database.connect()` when SQL transaction statements need to span multiple
calls. Every autocommit write is durable in the WAL immediately; call
`checkpoint()` to fold it into the page file and truncate old WAL frames.

## Command-line frontend

Start an in-memory interactive session:

```text
cargo run --release -- example.basalt
basalt> CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT);
basalt> SELECT * FROM t;
```

The shell accepts multiline SQL when a statement contains an open quoted
literal or parenthesized expression, and top-level semicolons can separate
multiple statements. SQL without a semicolon is also executed when it is
complete. Run .help for all commands; useful commands include .tables,
.schema [TABLE], .mode table|csv|json, .headers on|off, .checkpoint,
.show, .clear, and .quit.

The same frontend works in automation:

```text
# Execute a script against a durable database.
basalt --file schema-and-seed.sql app.basalt

# Run commands in order on one connection (transactions can span commands).
basalt --command "BEGIN;" --command "INSERT INTO t VALUES (1, 'one');" --command "COMMIT;" app.basalt

# Produce machine-readable output.
basalt --json --command "SELECT * FROM t ORDER BY id;" app.basalt
basalt --csv --quiet --command "SELECT * FROM t;" app.basalt
```

The option --file - reads SQL from stdin. The options --command and --file may
be repeated and execute in the order they appear. Table output is
human-readable, CSV emits only query rows, and JSON emits one object per
statement (JSON Lines). Batch errors are written to stderr and return a
non-zero exit status; --quiet suppresses successful non-query messages.

EXPLAIN SELECT ... reports the chosen table or index access path in every
output mode.

## Repository layout

- src/sql/ - lexer, parser, AST, and SQL dialect
- src/engine.rs - execution and query semantics
- src/planner.rs - access-path selection
- src/db.rs, src/database.rs - tables, constraints, transactions, and API
- src/storage.rs, src/wal.rs - snapshots, checksums, and recovery
- src/cli.rs - interactive and scripted command-line frontend
- tests/ - integration, crash-recovery, and executable frontend coverage
- benches/ - repeatable throughput benchmark

## Verification

The project intentionally has no external dependencies:

```text
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo bench --bench throughput
```

## License

MIT
