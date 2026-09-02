# Basalt workspaces

Basalt workspaces are local directories for structured data that needs to be
queried or transformed without modifying an application database. The
workspace commands are a thin lifecycle around the existing SQL engine; they
do not create a service or a second database protocol.

## Create and inspect

```bash
basalt init .basalt-workspace
basalt workspace inspect .basalt-workspace
basalt workspace inspect --json .basalt-workspace
basalt workspace query --json .basalt-workspace "SELECT * FROM issues"
```

`basalt init PATH` is an alias for `basalt workspace init PATH`. Initialization
refuses to replace an existing manifest or reserved database file. A workspace
contains:

```text
.basalt-workspace/
├── workspace.json       # format_version and canonical database name
├── .workspace.lock      # workspace ownership lock
├── data.basalt          # Basalt snapshot
├── data.basalt.wal      # committed frames awaiting checkpointing
└── data.basalt.lock     # process-ownership lock
```

The manifest currently has format version `1` and always names
`data.basalt`. The database format is Basalt's own format; it is not a SQLite
file. `.workspace.lock` and `data.basalt.lock` are advisory ownership sidecars;
do not remove or replace them while a workspace or database is open. Stop the
owner before copying or deleting a workspace. A durable workspace is owned by
one process at a time, just like a direct durable database path.

Opening a workspace requires its canonical `data.basalt` snapshot. The only
exception is an interrupted recovery where `data.basalt.wal` is still present;
Basalt can rebuild the snapshot from that WAL. If both are missing, Basalt
fails instead of silently creating an empty database that could hide data
loss.

## Import

```bash
basalt workspace import --table issues .basalt-workspace issues.csv
basalt workspace import --table fixtures .basalt-workspace fixtures.json
basalt workspace import --table events .basalt-workspace events.jsonl
basalt workspace import .basalt-workspace backup.sql

# Read a stream. Both the table and format are explicit.
cat report.csv | basalt workspace import \
  --table report --format csv .basalt-workspace -
```

File extensions infer `csv`, `json`, `jsonl`/`ndjson`, and `sql`. Use
`--format` for stdin or another extension. Row-oriented imports use the file
stem as the table name when `--table` is omitted. SQL imports contain a dump
of one or more statements and must not include `BEGIN`, `COMMIT`, `ROLLBACK`,
or `CHECKPOINT`; Basalt wraps the import in one transaction.

CSV requires a header row. The importer uses the RFC 4180 parser, infers
`INTEGER`, `REAL`, `BOOLEAN`, or `TEXT`, and keeps incompatible mixed values as
text. Empty CSV fields become `NULL` for inferred numeric/boolean columns and
empty text for text columns. JSON accepts one object, an array of objects, or
JSON Lines objects. Missing and `null` fields become `NULL`; nested JSON is
stored as compact text. Inputs are limited to 64 MiB and row imports are
atomic. Every successful import also creates a recoverable history entry and
returns a `change_id`; use `workspace history`, `workspace diff`, and
`workspace undo` to inspect or reverse the latest import. This applies to both
row-oriented imports and SQL dumps. Malformed input rejected during parsing
leaves no history record; failures after the import operation begins are
recorded as failed operations for diagnostics. In both cases, durable workspace
data remains unchanged.

Imported tables have no inferred primary keys or other constraints. Add those
with SQL after inspecting the imported data.

## Query

```bash
basalt workspace query .basalt-workspace "SELECT * FROM issues ORDER BY id"
basalt workspace query --json .basalt-workspace "SELECT COUNT(*) FROM issues"
```

The workspace query command accepts only `SELECT` and `EXPLAIN SELECT` and
uses the existing table, CSV, or JSON Lines result renderers. Mutations stay
out of this command until the preview/apply lifecycle is available.

## Reversible writes

Preview a mutation before it changes the database:

```bash
basalt workspace preview --json .basalt-workspace \
  "UPDATE issues SET status = 'closed' WHERE id = 42"
```

The returned report includes the exact SQL, impact summary, and a `plan_id`
derived from that SQL and the current database state fingerprint. A preview
accepts at most 64 statements and 32 mutating statements. Review the SQL before
applying the plan; apply requires that plan ID and refuses a stale plan:

```bash
basalt workspace plan --json .basalt-workspace PLAN_ID
basalt workspace apply --json .basalt-workspace PLAN_ID
basalt workspace history --json .basalt-workspace
basalt workspace diff --json .basalt-workspace CHANGE_ID
basalt workspace undo --json .basalt-workspace CHANGE_ID
```

The plan command reloads the persisted exact SQL and impact summary when a
client loses the preview response or reconnects to the workspace.

Apply writes a recovery snapshot before executing the transaction. History
records are finalized after the database checkpoint; an interrupted finalize
is surfaced as `recovered` or `unresolved` rather than silently discarded.
Undo restores only the latest committed change and refuses to remove later
work. Diffs report schema changes and exact added/removed row counts from a
deterministic row-multiset comparison. They do not claim keyed row pairing or a
row-by-row patch; an update normally appears as one removed row and one added
row.

Imports, apply, and undo are safe to retry after a lost response. Their
identifiers and persisted request metadata make an exact retry return the
original receipt while the workspace is still at the recorded post-operation
state. A failed import can be retried when its exact pre-import state is still
present; unresolved records and moved work are rejected instead of replayed or
discarded.

Plans, change records, and recovery snapshots live below `history/` and use
the workspace format version. They are local implementation metadata; the
database itself remains the single `data.basalt` file.

## Export

```bash
basalt workspace export .basalt-workspace issues issues.csv
basalt workspace export --format jsonl .basalt-workspace issues -
basalt workspace export --format sql .basalt-workspace issues issues.sql
```

CSV, JSON Lines, and SQL output are deterministic for the same database state.
CSV uses an empty field for `NULL`; JSON Lines preserves typed JSON values; SQL
contains a portable `CREATE TABLE` and `INSERT` sequence. User-created indexes
are not included in SQL dumps. Export refuses to overwrite the workspace
manifest or database and uses a temporary file before installing a regular
output file.

Automation can add `--json` to import and export commands to receive one
machine-readable metadata object on stdout. Import reports the source, format,
table, byte count, durable change ID, and import summary. Export reports the
destination, format, row count, and byte count after the file is installed:

```bash
basalt workspace import --json --table issues .basalt-workspace issues.csv
basalt workspace export --json .basalt-workspace issues issues.jsonl
```

An export to `-` is the data stream itself, so `--json` is rejected with stdout
exports instead of mixing metadata into the file format.

## Current boundary

The workspace foundation provides `init`, `inspect`, read-only `query`,
`preview`, `plan`, `apply`, `history`, `diff`, `undo`, `import`, and `export`. The same
ingest-to-undo lifecycle is available through `basalt mcp --workspace PATH`;
MCP imports accept bounded CSV, JSON, or JSON Lines content and require
`--allow-writes`. Both CLI and MCP imports create a recovery point and return a
change ID; SQL dump imports remain CLI-only. MCP writes return bounded
structured results.
The reversible lifecycle is local and single-process. See
[the MCP contract](mcp.md) for the agent-facing sequence and approval boundary.

An MCP host can create a new workspace on first start by passing
`--init-workspace` with `--workspace PATH`. This initializes only a missing
path; existing directories are opened normally and invalid manifests are not
replaced.
