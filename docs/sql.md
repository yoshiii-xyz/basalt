# Basalt SQL dialect

Basalt implements a deliberately small SQL dialect for embedded workloads.
The parser, executor, and tests are the source of truth; this page gives the
user-facing shape without implying compatibility with SQLite, PostgreSQL, or
another full SQL implementation.

## Statements

- `CREATE TABLE [IF NOT EXISTS]` with typed columns and inline `PRIMARY KEY`,
  `NOT NULL`, and `UNIQUE` constraints.
- `DROP TABLE [IF EXISTS]`.
- `CREATE [UNIQUE] INDEX [IF NOT EXISTS]` and `DROP INDEX [IF EXISTS]`.
- `INSERT ... VALUES` and `INSERT ... SELECT`.
- `SELECT`, `UPDATE`, and `DELETE`.
- `BEGIN`, `COMMIT`, `ROLLBACK`, `CHECKPOINT`, and `EXPLAIN SELECT`.

`SELECT` supports projections, aliases, `DISTINCT`, `WHERE`, inner/left/right/
full/cross joins, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, and `OFFSET`.
Scalar functions are `LOWER`, `UPPER`, `LENGTH`, `ABS`, `COALESCE`, and
`NULLIF`. Aggregate functions are `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX`.

## Values and rules

The value types are `NULL`, `INTEGER` (`i64`), `REAL` (`f64`), `TEXT`, and
`BOOLEAN`. Identifiers are case-insensitive for lookup, while their declared
spelling is preserved in schema metadata. Double-quoted and bracketed
identifiers support spaces and escaped delimiters. Single-quoted strings use
the SQL doubled-quote escape (`'it''s'`).

Arithmetic detects integer and real overflow. Division and remainder by zero
return `NULL`; comparisons involving `NULL` return `NULL`, and `WHERE` keeps
only predicates that evaluate to `TRUE`. `NULL` does not consume a `UNIQUE`
value, but a primary key is always non-null.

Numeric literals accept integers, decimals, leading-dot decimals, and `e`/`E`
exponents. The minimum `INTEGER` literal is written as
`-9223372036854775808`.

## Transactions and durability

`Database::in_memory()` is ephemeral. `Database::open(path)` stores committed
state in a checksummed snapshot and appends each committed generation to
`path.wal`. `checkpoint()` writes a fresh snapshot and clears old WAL frames.
Connections keep one private snapshot for explicit `BEGIN`/`COMMIT`/
`ROLLBACK`; concurrent commits are detected and rejected rather than silently
overwriting one another.

A durable file is owned by one process at a time. Clone a `Database` handle to
share it safely across threads in that process. The engine currently does not
provide a network protocol, prepared parameters, foreign keys, multi-column
indexes, schema alteration, views, CTEs, or subqueries. `INSERT ... SELECT`
is supported as a statement form.
