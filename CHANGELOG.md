# Changelog

## Unreleased

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
