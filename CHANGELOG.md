# Changelog

## Unreleased

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
- Scoped MCP tool discovery by mode so direct database clients do not receive
  workspace-only tools and workspace clients cannot bypass the plan lifecycle
  with unrestricted SQL execution.
- Completed the embedded SQL engine, storage, indexes, transactions, WAL
  recovery, planner, and command-line frontend.
- Added script execution, CSV/JSON-lines output, schema discovery, crash
  recovery coverage, and reproducible project checks.
- Added an official `rmcp` stdio server with typed SQL tools, schema resources,
  bounded results, stateful MCP transactions, and wire-level protocol tests.
- Hardened numeric literal parsing, integer and real overflow handling, and
  recovery from incomplete WAL tails.
- Added cross-process ownership for durable database paths and documented the
  CLI-first SQL and release contracts.
