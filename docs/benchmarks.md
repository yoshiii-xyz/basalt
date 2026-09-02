# Workspace benchmark

Basalt's differentiator is a controlled workspace lifecycle, not a claim of
general-purpose database throughput. The benchmark therefore measures one
representative structured-data workflow:

```text
import -> aggregate query -> preview -> apply -> table-level diff -> undo -> export
```

The fixture has four columns and a configurable number of rows. Each operation
runs against a fresh database/workspace for every repetition. Basalt is invoked
as a CLI for each operation, so its measurement includes process startup,
workspace validation, durable file opening, and output conversion. SQLite uses
Python's standard-library `sqlite3` module and DuckDB uses its optional Python
client; those baselines stay in one Python process. The result is useful for
workflow cost and bottleneck discovery, not a fair claim that one SQL engine is
faster than another.

SQLite and DuckDB do not expose Basalt's exact preview plan, durable change
ledger, or verified latest-change undo in this harness. Their preview proxy is a
transaction that is rolled back; their recovery proxy copies the database file
before apply and restores it during undo. Those limitations are included in the
JSON output and must remain in any published comparison.

## Run it

Build the optimized binary first:

```bash
cargo build --release --locked
python3 scripts/benchmark_workspace.py \
  --basalt target/release/basalt \
  --rows 10000 \
  --repeats 3 \
  > benchmark.json
```

The script has no required third-party Python dependency. DuckDB is reported as
`unavailable` unless its Python client is installed separately:

```bash
python3 -m pip install duckdb
```

Do not commit `benchmark.json`: timings depend on the machine, filesystem,
Python build, and current system load. Store the command, environment, commit,
and complete output with any performance report. Report medians and the full
run values; do not quote a single fastest run.

## How to read the result

The report is versioned JSON with one entry per backend and one summary per
operation. A backend can be `ok` or `unavailable`. Before comparing numbers,
check the notes and confirm that the same fixture, row count, repetitions, and
Basalt commit were used.

The benchmark intentionally makes no claim about:

- SQLite file-format compatibility;
- DuckDB analytical performance outside this small workflow;
- multi-process write concurrency;
- durability equivalence between different storage engines; or
- the correctness or safety of a baseline's rollback/file-copy proxy.

The supported SQL boundary is documented in [docs/sql.md](sql.md). The
workspace contract and recovery precision are documented in
[docs/workspaces.md](workspaces.md).

See [the recorded benchmark snapshot](benchmark-results.md) for one complete
10,000-row run with pinned host and client versions. It is evidence for the
current bottlenecks, not a promise about another machine or workload.
