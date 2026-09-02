# Basalt product brief

**Status:** Decision record
**Date:** 2026-09-01
**Scope:** CLI-first local product; no web frontend

## Decision

Basalt will not compete as a general-purpose replacement for SQLite,
PostgreSQL, or DuckDB. That would require compatibility, ecosystem, and trust
that a small new project cannot credibly provide.

Basalt will become a **local, reversible SQL workspace for coding agents and
automation**. It is for structured data that an agent needs to import, inspect,
transform, and explain without touching a production database or sending data
to a service.

The switching argument is not “Basalt has more SQL.” It is:

> Basalt gives an agent a private data workspace where every write can be
> previewed, bounded, audited, and undone.

This is a new-workspace product, not a production-database migration product.
Users can keep SQLite, PostgreSQL, DuckDB, or Turso for application data and
use Basalt for agent tasks that need relational state.

## Primary user

The initial ideal user is a developer using a coding agent such as Claude Code,
Codex, Cursor, or an equivalent MCP client. They need to work with CSV, JSON,
logs, issue exports, test fixtures, or generated tabular data while keeping the
work local and recoverable.

Their current alternatives are usually a mixture of shell scripts, temporary
SQLite files, ad-hoc Python, a generic database MCP server, or a full database
that is too risky to give an agent write access to.

The job Basalt must complete:

1. Create an isolated workspace beside a project.
2. Load structured input without writing a custom ingestion script.
3. Let a human or agent inspect schema and query the data.
4. Preview a proposed mutation before it becomes durable.
5. Apply the approved mutation with bounded impact.
6. Show what changed and undo it when necessary.
7. Reopen the workspace later or hand it to another local process.

Secondary users are Rust developers who want this workflow as a library. They
are not the first adoption target, but the public API must remain useful for
embedding the same workspace semantics.

## Explicit non-goals

These are deliberately out of scope for this product cycle:

- A web frontend, dashboard, hosted service, account system, or telemetry.
- A network database server or multi-tenant control plane.
- A complete SQLite, PostgreSQL, or MySQL compatibility project.
- A filesystem virtualization layer or general agent runtime. AgentFS already
  occupies that broader category.
- Embeddings, vector search, autonomous memory, or other speculative AI
  features that are not required for structured-data workflows.
- Production application-database migrations or a promise of multi-process
  write concurrency.
- A large SQL feature checklist without a demonstrated user need.

More SQL syntax is allowed only when it directly unblocks the chosen workflow
or a compatibility boundary that we explicitly commit to.

## What the research says

The following findings are evidence. The product decision built from them is an
inference and must be validated through a working workflow and external use.

| Existing option | What it already does well | Opening for Basalt |
| --- | --- | --- |
| SQLite | Zero-configuration local storage, a mature CLI, broad language support, a stable portable file format, and strong reliability. SQLite's own guidance recommends it for most device-local workloads with low write concurrency. | Do not ask users to replace their application SQLite file. Offer a safer workflow for disposable agent-owned data. |
| DuckDB | In-process analytical SQL, vectorized execution, broad client APIs, and direct data-analysis integrations. | Focus on controlled task state and reversible mutations, not analytical throughput. |
| Turso | A Rust in-process SQLite implementation pursuing compatibility, with a CLI, multiple language clients, cloud, sync, concurrent writes, CDC, and AI-oriented extensions. | “Rust database” and “agent database” are not differentiation by themselves. Avoid a compatibility arms race. |
| AgentFS | A SQLite-backed agent filesystem with CLI, SDKs, auditing, snapshots, portability, and a simple install path. | Stay SQL/data-specific. Do not build another filesystem or generic memory system. |
| Workspace-first MCP projects | MCP tools for workspaces, forks, checkpoints, search, and scoped access are emerging. | Make structured-data preview, impact reporting, and undo the central workflow. |
| SQLite MCP servers | Easy access to existing SQLite files through schema, query, and execute tools; some now offer read-only defaults, allowlists, and single-binary installs. | Raw SQL access is already available. Basalt must provide deterministic state controls, not just another `query` tool. |
| Rust embedded stores | `redb`, `fjall`, `sled`, and GlueSQL demonstrate demand for safe Rust storage, SQL layers, portable data, and configurable persistence. | Rust is an implementation advantage, not the user-facing reason to switch. |

### Competitive implications

SQLite's file-format stability and CLI availability create a very high bar for
general-purpose replacement. SQLite documents that its format has remained
backwards compatible since 2004 and that the CLI is distributed as a small
standalone program. Basalt's custom format and small dialect cannot win that
argument today.

DuckDB already owns the “embedded SQL for analysis” message. Turso is a direct
Rust-and-agents competitor with a compatibility strategy. AgentFS and
workspace-first MCP tools show that agent state, audit history, snapshots, and
scoped workspaces are real product concerns, but they also mean “local state for
agents” is too broad as a position.

The narrow opening is a structured-data operation that needs all of these at
once:

- relational queries rather than only files or key-value records;
- local-only execution;
- a clear preview before mutation;
- a durable record of the change;
- a cheap undo path; and
- an MCP and CLI surface that expose the same semantics.

### Current upstream issue signal (rechecked 2026-09-01)

These are open issue reports, not defect rates, security audits, or adoption
metrics. They are useful evidence about the problems adjacent tools are being
asked to solve:

- AgentFS has an open request for whole-filesystem rollback, alongside current
  reports involving a first-command timeout on Apple silicon and filesystem
  corruption. That reinforces the boundary: Basalt should make structured-data
  changes reversible without expanding into a filesystem overlay.
- The official MCP servers repository has an open report that delete tools can
  return success when nothing matched, and another that a stateful tool's
  read-only and idempotent annotations are inaccurate. Basalt must report the
  actual outcome of each operation, keep additive and destructive hints
  accurate, and treat annotations as host hints rather than enforcement.
- Turso has open reports about a short WAL read being treated as an empty log
  and about `.dump` changing the stored type of a whole-number REAL. These do
  not establish a general quality ranking, but they show why SQLite
  compatibility and durability claims need their own test and release budget.

The product inference is narrow and practical: Basalt's best switching moment
is not “use a better database.” It is “give an agent a local data workspace
whose proposed writes have truthful status, explicit approval, durable
recovery, and a bounded undo path.” The current implementation covers that
shape; external users still need to validate whether it is valuable enough to
switch workflows.

## Basalt's current position

The current repository already provides useful foundations:

- A readable Rust SQL parser, executor, planner, B-tree index layer, and
  snapshot/WAL durability path.
- Snapshot-isolated transactions and optimistic conflict detection within a
  process.
- A tested CLI with interactive, script, CSV, and JSON-lines modes.
- A local stdio MCP server with bounded queries, schema discovery, typed
  results, and stateful transactions.
- Crash-recovery, concurrency, CLI, and MCP integration coverage.

The current product still has blocking gaps:

- It is installed from a checkout and has no published release artifact yet;
  the checked-in `dist` configuration and release workflow now define the
  normal-user path for the first tagged release.
- The package name is `basalt-db` because `basalt` is already present in the
  crates.io index for an unrelated project; the published library crate and
  package name are now unambiguous while the installed binary remains
  `basalt`.
- The file format is not SQLite-compatible; the CLI's common-format import and
  export path is intentionally a documented boundary rather than a promise of
  SQLite-file interoperability.
- The documented SQL dialect does not include prepared parameters, migrations,
  foreign keys, schema alteration, views, CTEs, or subqueries.
- Durable database paths are intentionally exclusive to one process.
- MCP `execute` is powerful and can mutate data; annotations alone are not a
  deterministic safety boundary.
- The benchmark is an internal workload, not a comparable result against an
  incumbent.
- No release has been published yet, so the normal-user install path is
  defined but not externally verified from a clean machine.

These gaps are not all equal. The first product release should fix the gaps
that block the selected workflow, not attempt to make Basalt feature-complete
SQL.

## Product contract for the first switching release

### Workspace

`basalt init PATH` creates a self-contained local workspace with an explicit
format/version marker. A workspace has one canonical database path and a small
metadata/history area. The layout is documented and safe to delete or copy.

### Import and export

The first release supports predictable import/export for CSV, JSON/JSONL, and
Basalt SQL dumps. Row imports infer columns, report row counts after a
successful atomic commit, and leave the workspace unchanged on failure.
Export is deterministic so a workspace can be inspected, backed up, or moved
without a Basalt-specific opaque dependency.

SQLite-file compatibility is not required for the first wedge, but importing a
SQLite dump or a clearly documented subset must remain a planned escape hatch.

### Query and mutation lifecycle

The CLI and MCP expose the same lifecycle:

```text
inspect -> preview -> apply -> diff/history -> undo or export
```

Preview executes against an isolated transaction and returns the statements,
affected-row estimates/counts, result metadata, and a stable plan identifier.
Apply accepts only the exact previewed operation, creates a recovery point, and
returns the committed change identifier. Undo restores the recovery point
without rewriting unrelated history.

The implementation must not claim a row-level diff when it only has a
database-level snapshot diff. The output should state its precision honestly.

### Safety

- MCP starts read-only unless the operator explicitly enables writes.
- Write tools have separate names and accurate risk annotations.
- Workspace paths are explicit and cannot escape the configured workspace.
- Structured-data imports are explicit, content-based, bounded, and recoverable;
  MCP never accepts an arbitrary source path or SQL dump as an import payload.
- Requests have limits for SQL size, rows, statements, mutation count, and
  output size.
- A failed preview never changes durable state.
- An applied operation always has a recovery point or fails before mutation.
- Exact apply and undo retries return the original receipt only when the
  workspace is still at the recorded post-operation state; moved state is
  rejected rather than replayed.
- History and diagnostics never pollute MCP stdout.

MCP annotations are useful host hints, but deterministic controls must live in
Basalt. The MCP community guidance explicitly treats annotations as hints, not
guarantees.

### Distribution

The switching release must be usable without Rust:

- versioned binaries for the primary desktop platforms and architectures;
- a published, unambiguous package name if a Rust package is offered;
- a one-command installer or package-manager path;
- a copy-paste MCP configuration using the installed binary;
- an MCP Registry entry after the package and binary release exist and the
  registry contract is stable enough for the release;
- checksums and release notes;
- an install smoke test from a clean environment.

The official registry currently documents Cargo and MCPB package types, but it
is still in preview and stores installation metadata rather than artifacts.
Basalt will publish registry metadata only after `basalt-db` is on crates.io
and the checksummed binary release exists. The Cargo package will need a
visible `mcp-name:` marker in the rendered README for registry ownership
verification; adding that marker before publication would be misleading.

## Finite implementation plan

### Milestone 1 — Evidence and contract

This document records the research, target user, switching moment, non-goals,
and acceptance criteria. No implementation begins until a feature can be tied
to the contract below.

### Milestone 2 — Workspace foundation

- Add explicit workspace initialization and metadata.
- Add deterministic CSV, JSON/JSONL, and SQL dump import/export.
- Add format/version inspection and clear path handling.
- Add end-to-end CLI tests for create, import, query, export, and reopen.

Exit condition: a user can create and move a useful workspace without writing
an ingestion script or knowing the internal file layout.

### Delivery status

- Milestone 1 is complete: the evidence review and product contract are
  recorded in this document.
- Milestone 2 is implemented and covered by CLI integration tests: versioned
  initialization, inspection, CSV/JSON/JSON Lines/SQL imports, deterministic
  exports, reopen behavior, and failed-import rollback.
- Milestone 3 is complete: exact-state preview/apply plans, durable recovery
  points, table-level diffs, history, latest-change-only undo, and crash
  reconciliation are implemented and covered by failure-injection tests.
- Milestone 4 is complete: workspace-aware MCP tools are scoped by mode,
  read-only by default, bounded, typed, and covered by stdio wire tests for the
  full ingest-to-undo journey without shelling out. Workspace MCP imports are
  limited to structured row formats, require explicit writes, and create
  recovery points.
- Milestone 5 remains open only for publication and external-user validation.
  Hosted release evidence is complete: the incumbent benchmark harness,
  recorded SQLite/DuckDB snapshot, and first full comparison run are complete;
  supported-subset differential checks, compatibility policy, parser and
  snapshot fuzz targets, bounded campaigns, package contents, release
  generation, dependency auditing, MSRV checking, and clean-binary smoke tests
  are implemented and verified locally; hosted CI also checks macOS and Windows
  compilation. No public release or adoption claim is made until a release is
  actually published and exercised by external users.

### Milestone 3 — Reversible state

- Add durable recovery points with atomic creation and restart recovery.
- Add preview execution with a stable operation hash/identifier.
- Add apply verification, history, diff reporting, and undo.
- Add failure injection and crash tests around apply and restore.

Exit condition: a mutation can be inspected, applied, explained, and undone in
one complete CLI journey, with tests covering interruption at each durable
boundary.

### Milestone 4 — Agent surface

- Make MCP read-only by default; require an explicit write policy.
- Add bounded structured-data import, inspect, preview, apply, history/diff,
  undo, and export tools only when each has a complete deterministic
  implementation.
- Keep query results bounded and typed.
- Add workspace path/capability checks and exact MCP wire tests.
- Document the approval and recovery model for Claude Code, Codex, Cursor, and
  generic MCP hosts without assuming host-specific behavior.

Exit condition: an agent can complete the same ingest-to-undo workflow over
stdio MCP without shelling out or receiving ambiguous tool results.

### Milestone 5 — Trust and distribution

- Publish comparable benchmarks against SQLite and DuckDB for the selected
  workflow, including import, preview, apply, query, and recovery.
- Add SQL differential tests for the supported contract, fuzzing for parser and
  persistence boundaries, and a documented format compatibility policy.
- Add release binaries, package/install documentation, checksums, and a clean
  machine smoke test.
- Update README, MCP/operator/developer docs, changelog, CI, and release notes.

Exit condition: a new user can install Basalt, complete the workflow in five
minutes, understand its limits, and recover from an intentionally interrupted
operation.

## Success criteria

The release is complete only when all of these are true:

1. The primary user can finish the full workspace workflow using only the CLI.
2. The same ingest-to-undo workflow works through MCP with no raw filesystem
   escape hatch.
3. Previewed writes are exact enough to explain and cannot silently change the
   workspace before apply.
4. Every applied write has a tested recovery path.
5. Common structured data can enter and leave deterministically.
6. The install path requires no Rust toolchain for normal users.
7. Benchmark results identify where Basalt is slower, equal, or better; no
   blanket performance claim is made.
8. The supported SQL, durability model, security boundary, file format, and
   non-goals are documented.
9. All required local and hosted quality checks pass on the pushed release
   candidate.

External adoption is not something the repository can claim on its own. After
the implementation is complete, the README should invite targeted early users
and record feedback as evidence rather than presenting the product brief as
market validation.

## Research sources

Primary and current sources reviewed for this decision:

- [SQLite: Appropriate Uses](https://www.sqlite.org/whentouse.html)
- [SQLite: serverless architecture](https://www.sqlite.org/serverless.html)
- [SQLite: stable file format](https://www.sqlite.org/onefile.html)
- [SQLite: command-line shell](https://www.sqlite.org/cli.html)
- [SQLite: prepared statement parameters](https://www.sqlite.org/c3ref/bind_blob.html)
- [SQLite: WAL concurrency](https://www2.sqlite.org/wal.html)
- [DuckDB: why DuckDB](https://duckdb.org/why_duckdb)
- [DuckDB: clients](https://duckdb.org/docs/current/clients/overview)
- [DuckDB: concurrency](https://duckdb.org/docs/stable/connect/concurrency.html)
- [Turso: current rewrite and roadmap](https://turso.tech/blog/we-are-a-year-into-rewriting-sqlite)
- [Turso: SQLite compatibility contract](https://github.com/tursodatabase/turso/blob/main/COMPAT.md)
- [Turso: local database manual and installation](https://github.com/tursodatabase/turso/blob/main/docs/manual.md)
- [AgentFS](https://github.com/tursodatabase/agentfs)
- [AgentFS open issues](https://github.com/tursodatabase/agentfs/issues)
- [Redis Agent Filesystem](https://github.com/redis/agent-filesystem)
- [MCP official reference servers](https://github.com/modelcontextprotocol/servers)
- [MCP Registry](https://modelcontextprotocol.io/registry/about)
- [MCP Registry package types](https://modelcontextprotocol.io/registry/package-types)
- [MCP Registry publishing quickstart](https://modelcontextprotocol.io/registry/quickstart)
- [MCP 2026-07-28 specification release](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
- [MCP tool annotation guidance](https://blog.modelcontextprotocol.io/posts/2026-03-16-tool-annotations/)
- [AgentFS rollback request](https://github.com/tursodatabase/agentfs/issues/313)
- [AgentFS macOS first-exec timeout report](https://github.com/tursodatabase/agentfs/issues/342)
- [AgentFS filesystem corruption report](https://github.com/tursodatabase/agentfs/issues/332)
- [MCP servers inaccurate delete outcome report](https://github.com/modelcontextprotocol/servers/issues/4740)
- [MCP servers inaccurate statefulness annotations report](https://github.com/modelcontextprotocol/servers/issues/4721)
- [Turso WAL recovery report](https://github.com/tursodatabase/turso/issues/8593)
- [Turso dump type round-trip report](https://github.com/tursodatabase/turso/issues/8577)
- [MCP roots and filesystem boundaries](https://modelcontextprotocol.io/specification/2025-03-26/client/roots)
- [redb](https://github.com/cberner/redb)
- [fjall](https://github.com/fjall-rs/fjall)
- [sled](https://github.com/spacejam/sled)
- [GlueSQL](https://github.com/gluesql/gluesql)
- [SQLite MCP server example](https://github.com/rvarun11/sqlite-mcp)
- [single-binary SQLite MCP with access control](https://github.com/0xOmarA/mcp-server-sqlite)
- [read-only SQLite MCP example](https://github.com/helmi75/mcp-sqlite-server)

Community repositories are treated as market signals and implementation
examples, not as audited security or adoption claims.
