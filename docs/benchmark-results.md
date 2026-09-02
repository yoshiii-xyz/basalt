# Benchmark snapshot

This is a reproducible evidence snapshot for the selected workspace workflow,
not a general-purpose database performance claim. It was run on 2026-09-01
against commit `0c66749` with 10,000 fixture rows and three fresh repetitions:

```bash
python3 scripts/benchmark_workspace.py \
  --basalt target/release/basalt \
  --rows 10000 \
  --repeats 3
```

Environment: Basalt 0.1.0, Linux x86_64, Python 3.12.3, SQLite 3.45.1, and
DuckDB 1.5.5. Values below are median milliseconds. The command emits the
individual run values as well, so the snapshot can be regenerated or audited.

| Operation | Basalt | SQLite | DuckDB |
| --- | ---: | ---: | ---: |
| Import | 156.967 | 39.117 | 3804.447 |
| Aggregate query | 26.895 | 3.505 | 4.104 |
| Preview | 31.768 | 0.515 | 1.108 |
| Apply | 79.528 | 11.695 | 40.295 |
| Table-level diff | 48.567 | 23.987 | 27.395 |
| Undo | 82.797 | 0.672 | 32.593 |
| Export | 26.035 | 14.202 | 17.301 |

The process models are intentionally different. Basalt starts a CLI process,
opens and validates a workspace, and converts output for every operation.
SQLite and DuckDB remain in one Python process. Their preview is a rolled-back
transaction and their recovery is a database-file copy, not Basalt's exact plan
ledger and verified recovery-point workflow. These numbers identify the cost
of the complete workflow and its current bottlenecks; they do not establish
that Basalt is faster or slower as a general SQL engine.

The workload and measurement code live in
[scripts/benchmark_workspace.py](../scripts/benchmark_workspace.py), and the
protocol and compatibility limits are documented in
[docs/benchmarks.md](benchmarks.md) and
[docs/compatibility.md](compatibility.md).
