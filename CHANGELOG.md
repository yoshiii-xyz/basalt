# Changelog

## Unreleased

- Added release-archive verification for checksums, required files, executable
  contents, and unsafe archive members; the release host job also runs the
  extracted Linux artifact through the installed-binary smoke journey.
- Pinned CI and release GitHub Actions to reviewed commit SHAs, with the
  generated release workflow pins kept in `dist-workspace.toml`.
- Bounded MCP workspace diffs to 10,000 inspected rows per compared database
  and removed unnecessary checkpoint writes from history and diff reads.
- Serialized workspace MCP operations within one server process so concurrent
  valid requests cannot race the workspace database lock.
- MCP export and diff row limits now reject oversized tables before row data is
  materialized, while the CLI keeps its complete-table behavior.
- Keep a workspace lock for the full lifetime of CLI and MCP ownership, closing
  the undo window where another process could write while a recovery snapshot is
  being installed.
- Bounded MCP and workspace preview requests to 32 mutating statements per
  call, in addition to their existing SQL and statement limits.
- Bounded workspace MCP exports by row count and content size before building a
  response, directing larger exports to the CLI.
- Hardened workspace export protection so path aliases cannot overwrite the
  workspace database or manifest.
- Made exact workspace apply and undo retries return the original receipt after
  a lost response, while rejecting retries against moved workspace state; MCP
  metadata now advertises those retries as idempotent.
- Made bounded workspace MCP imports return their original receipt on an exact
  retry after a lost response by persisting an import request fingerprint.
- Allowed an exact retry of a failed workspace MCP import when its recorded
  base state is still unchanged; unresolved or moved imports remain blocked.
- Included the exact SQL in CLI and MCP workspace preview reports so an
  operator can review the operation before approving its plan ID.
- Validated MCP preview response size before persisting plan metadata, so an
  oversized response cannot leave an orphaned plan behind.
- Added concise Claude Code and Cursor project configuration paths for the
  installed workspace MCP server, while keeping server-side writes explicit.
- Corrected the MCP workspace import annotation to identify it as additive
  rather than destructive, with a wire-level regression check.
- Added versioned local workspaces with `basalt init`, read-only workspace
  queries, and JSON inspection.
- Added atomic CSV, JSON/object, JSON Lines, and Basalt SQL dump imports plus
  deterministic CSV, JSON Lines, and SQL exports, with a 64 MiB input limit.
- Added end-to-end workspace coverage for initialization, type inference,
  reopen, format round trips, and failed-import rollback.
- Added exact-state write plans, durable recovery points, table-level diffs,
  change history, and latest-change-only undo for workspaces.
- Added workspace-aware MCP tools for inspect, preview, apply, history, diff,
  undo, and bounded export. MCP is read-only by default; direct SQL writes,
  workspace applies/undos, and checkpointing require explicit `--allow-writes`.
- Added bounded CSV, JSON, and JSON Lines workspace MCP imports. Each approved
  import is atomic, path-free, and recorded with a recovery point and change ID;
  SQL dump imports remain CLI-only.
- Scoped MCP tool discovery by mode so direct database clients do not receive
  workspace-only tools and workspace clients cannot bypass the plan lifecycle
  with unrestricted SQL execution.
- Added a `basalt-db` package boundary while preserving the `basalt` binary,
  reproducible `dist` release configuration, checksummed installer workflow,
  clean-binary smoke test, and scheduled RustSec dependency audit.
- Added a reproducible workspace workflow benchmark with SQLite and optional
  DuckDB baselines, explicit process-model notes, and no blanket performance
  claim.
- Added a release preflight that validates the packaged crate and runs the
  installed-binary CLI/MCP smoke journey from an isolated temporary prefix.
- Added machine-readable `--json` reports for workspace imports and exports,
  while keeping stdout exports as clean data streams.
- Isolated packaged-crate installation build artifacts so release preflight
  cannot replace the checkout's development binary with stale packaged code.
- Added a recorded benchmark snapshot with host and client versions, median
  operation timings, and the complete process-model caveat.
- Added a documented file-format compatibility policy and differential checks
  for a deliberately small shared SQL subset.
- Added optional libFuzzer targets for arbitrary SQL parsing and snapshot/state
  decoding, plus a bounded in-memory snapshot validation API.
- Completed the embedded SQL engine, storage, indexes, transactions, WAL
  recovery, planner, and command-line frontend.
- Added script execution, CSV/JSON-lines output, schema discovery, crash
  recovery coverage, and reproducible project checks.
- Added an official `rmcp` stdio server with typed SQL tools, schema resources,
  bounded results, stateful MCP transactions, and wire-level protocol tests.
- Verified the installed-binary MCP smoke path against modern `2026-07-28`
  discovery and per-request metadata while retaining legacy handshake coverage.
- Hardened numeric literal parsing, integer and real overflow handling, and
  recovery from incomplete WAL tails.
- Added cross-process ownership for durable database paths and documented the
  CLI-first SQL and release contracts.
