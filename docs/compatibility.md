# Compatibility policy

Basalt's compatibility promise is intentionally narrower than SQLite,
PostgreSQL, or DuckDB's. The supported SQL language is the contract in
[docs/sql.md](sql.md); a parser feature is not promised merely because another
engine accepts it.

## Database and workspace files

`data.basalt` and `data.basalt.wal` are Basalt's own checksummed snapshot/WAL
formats. They are not SQLite databases and must not be opened by SQLite,
DuckDB, or an application that assumes SQLite's file format. Basalt does not
promise that another engine can recover a partial or future Basalt file.

Snapshot format `1` is the current format. WAL version `1` remains readable for
existing workspaces; new frames use WAL version `2`, which adds a checksum for
the frame header as well as the existing payload checksum. A WAL may contain a
valid mixture during an upgrade. Basalt rejects unsupported versions, invalid
headers, invalid payloads, and non-monotonic generations rather than guessing.

Workspace directories carry a `workspace.json` format version. Basalt rejects
an unsupported version instead of guessing. The current format version is `1`.
CSV, JSON Lines, and Basalt SQL exports are the portable interchange boundary;
use those formats when moving data between tools or preserving a readable
backup.

Within one workspace format version, exports are deterministic and imports are
atomic. A workspace is exclusively owned by one Basalt process while open;
`.workspace.lock` coordinates workspace mode with direct opens of its canonical
`data.basalt` file. A future incompatible workspace layout must increment the
format version. A format increment is a migration decision, not an implicit
promise that old files can be upgraded in place.

If `data.basalt` is missing, workspace mode opens it only when the WAL contains
a recoverable committed frame. An empty, torn-only, corrupt, or unsupported WAL
does not authorize creating an empty replacement; the open fails so possible
data loss is visible. See the [backup and restore procedure](production-readiness.md#backup-and-restore)
for safe copies of durable workspaces.

## SQL compatibility checks

The repository includes `scripts/differential_sql.py`. It compares a small,
explicitly selected subset of documented SQL behavior against Python's bundled
SQLite library. CI runs it against the release binary. These checks cover
regression detection for shared behavior; they do not certify SQLite
compatibility and intentionally exclude engine-specific syntax and semantics.

Run the checks locally with:

```bash
cargo build --release --locked
python3 scripts/differential_sql.py --basalt target/release/basalt
```

When Basalt and SQLite disagree, first decide whether the statement belongs to
Basalt's documented contract. If it does, fix the regression or document the
intentional semantic boundary before changing the differential fixture.
