# Compatibility policy

Basalt's compatibility promise is intentionally narrower than SQLite,
PostgreSQL, or DuckDB's. The supported SQL language is the contract in
[docs/sql.md](sql.md); a parser feature is not promised merely because another
engine accepts it.

## Database and workspace files

`data.basalt` is Basalt's own checksummed snapshot/WAL format. It is not a
SQLite database and must not be opened by SQLite, DuckDB, or an application
that assumes SQLite's file format. Basalt does not promise that another engine
can recover a partial or future Basalt file.

Workspace directories carry a `workspace.json` format version. Basalt rejects
an unsupported version instead of guessing. The current format version is `1`.
CSV, JSON Lines, and Basalt SQL exports are the portable interchange boundary;
use those formats when moving data between tools or preserving a readable
backup.

Within one workspace format version, exports are deterministic and imports are
atomic. A future incompatible workspace layout must increment the format
version. A format increment is a migration decision, not an implicit promise
that old files can be upgraded in place.

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
