# Benchmark snapshot

This is a reproducible evidence snapshot for the selected workspace workflow,
not a general-purpose database performance claim. It was run on 2026-09-02
against commit `1e9bc3f` with 10,000 fixture rows and three fresh repetitions:

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
| Import | 153.005 | 26.691 | 3879.845 |
| Aggregate query | 30.758 | 3.662 | 1.581 |
| Preview | 32.354 | 0.523 | 1.075 |
| Apply | 81.143 | 10.437 | 43.661 |
| Diff | 34.897 | 15.224 | 27.627 |
| Undo | 77.368 | 0.699 | 30.332 |
| Export | 30.832 | 12.851 | 15.038 |

The process models are intentionally different. Basalt starts a CLI process,
opens and validates a workspace, and converts output for every operation.
SQLite and DuckDB remain in one Python process. Their preview is a rolled-back
transaction and their recovery is a database-file copy, not Basalt's exact plan
ledger and verified recovery-point workflow. Basalt's diff also computes
deterministic added and removed row counts, while the baselines only compare
selected rows. These numbers identify the cost
of the complete workflow and its current bottlenecks; they do not establish
that Basalt is faster or slower as a general SQL engine.

The workload and measurement code live in
[scripts/benchmark_workspace.py](../scripts/benchmark_workspace.py), and the
protocol and compatibility limits are documented in
[docs/benchmarks.md](benchmarks.md) and
[docs/compatibility.md](compatibility.md).
