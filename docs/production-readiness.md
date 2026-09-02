# Production-readiness contract

This is Basalt's technical release contract for the narrow product it actually
ships: a local, single-owner, reversible SQL workspace for coding agents and
structured-data tasks. “Ready” means every in-scope guarantee below has a
passing implementation check and a documented operating procedure. It does
not mean that humans have validated the product or that Basalt has adoption.

## Scope

The contract covers:

- the Rust library and embedded SQL engine;
- durable snapshots, WAL recovery, locking, and workspace metadata;
- the `basalt` CLI and its table, CSV, and JSON Lines interfaces; and
- the local stdio MCP server, including its workspace approval boundary.

The following are explicit non-goals, not release blockers: SQLite or Postgres
compatibility, multi-process writes, a network service, cloud storage,
filesystem virtualization, a frontend, generic AI memory, and speculative
features outside the documented CLI/MCP workflow.

## Readiness matrix

| Area | Contract | Evidence |
| --- | --- | --- |
| Atomic durability | A committed durable generation is either in the synced WAL or in the synced snapshot; snapshot installation is atomic and parent-directory durability is requested where the platform supports it. | `src/storage.rs`, `src/wal.rs`, `src/database.rs`; crash-recovery and storage tests. |
| Corruption handling | Snapshot pages, snapshot headers, WAL payloads, and current WAL headers are checksummed. Torn WAL tails are repaired; complete corruption, unsupported formats, and ambiguous recovery sources fail closed. | `src/storage.rs`, `src/wal.rs`; corruption and recovery tests. |
| Reversible workspace writes | Imports and reviewed plans persist a recovery point before mutation. Undo restores through a new WAL generation, keeps generations monotonic, and never discards later committed work. | `src/workspace.rs`, `src/database.rs`; workspace crash and lifecycle tests. |
| Format compatibility | Workspace format `1` and snapshot format `1` are explicit. WAL version `1` remains readable while new frames use version `2` with a header checksum. Unsupported versions are rejected. | `docs/compatibility.md`; WAL legacy/upgrade tests. |
| Resource bounds | CLI SQL is capped at 16 MiB; workspace previews at 1 MiB; MCP SQL and responses at 1 MiB; MCP work, rows, imports, history, snapshots, and WAL have finite limits; parser nesting is capped. | Constants and checks in `src/cli.rs`, `src/mcp.rs`, `src/workspace.rs`, `src/sql/parser.rs`, `src/engine.rs`; limit tests. |
| Path safety | Workspace-managed database, WAL, lock, snapshot, history, manifest, and temporary paths reject symbolic links and non-regular files where they are used. Export cannot alias workspace state or metadata. | `src/storage.rs`, `src/wal.rs`, `src/workspace.rs`, `src/database.rs`; symlink and alias tests. |
| Ownership | One process owns a durable database and workspace at a time. Workspace operations are serialized within an MCP server, and ownership locks live for the full handle lifetime. | `fs4` locks in `src/database.rs` and `src/workspace.rs`; cross-process and concurrent MCP tests. |
| MCP boundary | Stdio carries only newline-delimited JSON-RPC on stdout. Workspace mode does not expose unrestricted `execute`; writes require startup permission and protocol-appropriate approval. Tool results are typed and bounded. | `src/mcp.rs`, `scripts/mcp-smoke.py`; legacy and modern wire tests. |
| Deterministic interfaces | CLI JSON output is JSON Lines per statement; workspace reports have stable identifiers and explicit errors; exports are deterministic; MCP exposes typed result schemas and bounded row responses. | CLI/workspace/MCP integration tests and smoke scripts. |
| Distribution | The package, binary, registry metadata, release archives, checksums, installers, MSRV, portability checks, and clean-consumer smoke path are validated by the release workflow. | `.github/workflows/`, `dist-workspace.toml`, `scripts/release-check.sh`, `docs/release.md`. |

## Fixed resource limits

These are safety boundaries, not performance promises. The CLI intentionally
offers the less restrictive local path where the operation is explicit.

| Boundary | Limit |
| --- | ---: |
| Durable snapshot | 256 MiB on disk |
| WAL | 1 GiB total; checkpoint before continuing |
| Direct CLI SQL action/input buffer | 16 MiB |
| Workspace import | 64 MiB |
| Workspace preview SQL | 1 MiB, 64 statements, 32 mutating statements, 10,000 preview rows |
| MCP SQL | 1 MiB, 100 statements, 32 mutating statements, 1,000,000 work units |
| MCP SQL response | 1 MiB, 100 default rows and 1,000 maximum rows per SELECT |
| MCP stdio frame | 32 MiB per newline-delimited JSON-RPC message |
| MCP row import | 16 MiB, 10,000 rows, 256 columns, 1,000,000 cells |
| MCP reviewed mutation | 10,000 affected rows per plan |
| MCP diff/export | 10,000 rows per compared/exported database/table and 1 MiB response |
| MCP history | 10,000 change-directory entries and 1 MiB of record metadata |
| State strings and collections | 64 MiB per encoded string and 1,000,000 collection items |
| SQL parser nesting | 128 expression levels and 64 nested statements |

The MCP row cap limits returned rows, while the engine work budget also limits
scans, joins, aggregation, mutations, snapshot cloning, and checkpoint work.
Some query plans materialize intermediate rows before conversion; the work
budget and snapshot bounds are the safety boundary, not a claim that every
intermediate is streamed.

## Backup and restore

For an exact durable workspace backup:

1. Stop the owning CLI or MCP process and confirm no process holds the
   workspace lock.
2. Run `basalt workspace inspect PATH` and, if the workspace has pending WAL
   frames, run `basalt --command CHECKPOINT PATH` while it is owned.
3. Copy the complete workspace directory, including `workspace.json`,
   `data.basalt`, and `history/`, to the backup destination.
4. Restore only while no Basalt process owns either source or destination.
   Open the restored directory normally and inspect it before use.

For a portable data backup, export each required table as CSV, JSON Lines, or
Basalt SQL and import it into a fresh workspace. Portable exports do not carry
workspace history or user-created indexes; SQL exports preserve table
constraints supported by the exporter but are not a general SQLite dump.
Never copy live snapshot/WAL files while a process is writing them.

## Required evidence

From a clean checkout, run:

```bash
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
cargo package --locked
cargo publish --dry-run --locked
cargo build --release --locked
bash scripts/smoke-test.sh target/release/basalt
python3 scripts/differential_sql.py --basalt target/release/basalt
cargo audit
```

`bash scripts/release-check.sh` runs the applicable local release checks,
installs the exact packaged crate into an isolated prefix, and repeats the
installed-binary smoke journey. Hosted CI is required for the MSRV, macOS,
Windows, release-archive, and hosted-security portions.

Fuzzing, automated integration tests, and AI-agent testing are technical
evidence only. They are not human-user validation, usability research, or an
adoption claim. Record external-user results separately in
[`early-user-validation.md`](early-user-validation.md).

## Release status

The public crate, binary release, and MCP Registry entry may remain one
version behind the current hardening branch. The current candidate is `0.1.2`;
the public artifacts remain `0.1.1` until the release procedure completes.
Do not describe a candidate version as publicly available until its crate, tag, archives,
checksums, installer, and registry metadata have each been verified. A fresh
crates.io publish requires fresh operator authorization; no repository file
stores registry credentials.
