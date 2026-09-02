#!/usr/bin/env python3
"""Measure Basalt's structured-data workspace workflow against local engines.

The output is intentionally a workflow report, not an engine-speed claim.
Basalt is invoked once per operation, while the Python baselines stay in one
process. The report records that difference and does not turn rollback or file
copying into a claim of equivalent plan/history/undo semantics.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import platform
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Callable


QUERY = (
    "SELECT bucket, COUNT(*), SUM(value) FROM events "
    "WHERE value >= 5000 GROUP BY bucket ORDER BY bucket"
)
UPDATE = "UPDATE events SET label = 'reviewed' WHERE id <= 1000"


class BenchmarkError(RuntimeError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark Basalt's local workspace workflow against SQLite and DuckDB."
    )
    parser.add_argument(
        "--basalt",
        default="target/release/basalt",
        help="Basalt executable (default: target/release/basalt)",
    )
    parser.add_argument(
        "--rows",
        type=int,
        default=10_000,
        help="Rows in the fixture (default: 10000)",
    )
    parser.add_argument(
        "--repeats",
        type=int,
        default=3,
        help="Fresh workspaces/databases per backend (default: 3)",
    )
    return parser.parse_args()


def resolve_binary(requested: str) -> str:
    path = Path(requested)
    if path.parent != Path(".") or path.is_absolute():
        if not path.is_file() or not os.access(path, os.X_OK):
            raise BenchmarkError(f"Basalt executable is not runnable: {requested}")
        return str(path)
    resolved = shutil.which(requested)
    if resolved is None:
        raise BenchmarkError(f"Basalt executable was not found: {requested}")
    return resolved


def fixture_rows(count: int) -> list[tuple[int, str, int, str]]:
    return [
        (row_id, f"bucket-{row_id % 10}", row_id * 3, f"row-{row_id}")
        for row_id in range(1, count + 1)
    ]


def write_fixture(path: Path, rows: list[tuple[int, str, int, str]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(("id", "bucket", "value", "label"))
        writer.writerows(rows)


def run_command(command: list[str]) -> str:
    completed = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        rendered = " ".join(command)
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise BenchmarkError(f"command failed ({completed.returncode}): {rendered}: {detail}")
    return completed.stdout


def measure(operation: Callable[[], Any]) -> tuple[float, Any]:
    started = time.perf_counter_ns()
    value = operation()
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    return elapsed_ms, value


def summarize(samples: list[float]) -> dict[str, Any]:
    return {
        "median_ms": round(statistics.median(samples), 3),
        "min_ms": round(min(samples), 3),
        "max_ms": round(max(samples), 3),
        "runs_ms": [round(sample, 3) for sample in samples],
    }


def basalt_json(output: str) -> dict[str, Any]:
    try:
        value = json.loads(output)
    except json.JSONDecodeError as error:
        raise BenchmarkError(f"Basalt did not emit JSON: {error}") from error
    if not isinstance(value, dict):
        raise BenchmarkError("Basalt JSON response was not an object")
    return value


def run_basalt_once(binary: str, rows: list[tuple[int, str, int, str]], root: Path) -> dict[str, float]:
    workspace = root / "basalt-workspace"
    source = root / "events.csv"
    export_path = root / "events.jsonl"
    write_fixture(source, rows)
    run_command([binary, "init", str(workspace)])

    timings: dict[str, float] = {}

    elapsed, _ = measure(
        lambda: run_command(
            [
                binary,
                "workspace",
                "import",
                "--table",
                "events",
                str(workspace),
                str(source),
            ]
        )
    )
    timings["import"] = elapsed

    def query() -> None:
        response = run_command([binary, "workspace", "query", "--json", str(workspace), QUERY])
        value = basalt_json(response)
        if value.get("type") != "select" or not value.get("rows"):
            raise BenchmarkError("Basalt query returned no aggregate rows")

    timings["query"], _ = measure(query)

    def preview() -> dict[str, Any]:
        response = run_command(
            [binary, "workspace", "preview", "--json", str(workspace), UPDATE]
        )
        value = basalt_json(response)
        if value.get("mutating_statements") != 1:
            raise BenchmarkError("Basalt preview did not report one mutation")
        return value

    timings["preview"], preview_report = measure(preview)
    plan_id = preview_report.get("plan_id")
    if not isinstance(plan_id, str) or not plan_id:
        raise BenchmarkError("Basalt preview did not return a plan ID")

    def apply() -> dict[str, Any]:
        return basalt_json(
            run_command([binary, "workspace", "apply", "--json", str(workspace), plan_id])
        )

    timings["apply"], apply_report = measure(apply)
    change_id = apply_report.get("change_id")
    if not isinstance(change_id, str) or not change_id:
        raise BenchmarkError("Basalt apply did not return a change ID")

    def diff() -> None:
        report = basalt_json(
            run_command(
                [binary, "workspace", "diff", "--json", str(workspace), change_id]
            )
        )
        if report.get("state_changed") is not True:
            raise BenchmarkError("Basalt diff did not report the applied change")

    timings["diff"], _ = measure(diff)

    def undo() -> None:
        report = basalt_json(
            run_command(
                [binary, "workspace", "undo", "--json", str(workspace), change_id]
            )
        )
        if report.get("undone_change_id") != change_id:
            raise BenchmarkError("Basalt undo did not restore the applied change")

    timings["undo"], _ = measure(undo)

    def export_data() -> None:
        run_command(
            [
                binary,
                "workspace",
                "export",
                "--format",
                "jsonl",
                str(workspace),
                "events",
                str(export_path),
            ]
        )
        exported_rows = [
            line for line in export_path.read_text(encoding="utf-8").splitlines() if line
        ]
        if len(exported_rows) != len(rows):
            raise BenchmarkError(
                f"Basalt exported {len(exported_rows)} rows; expected {len(rows)}"
            )

    timings["export"], _ = measure(export_data)
    return timings


def sqlite_connection(module: Any, path: Path) -> Any:
    connection = module.connect(str(path))
    if module is sqlite3:
        connection.execute("PRAGMA journal_mode=DELETE")
        connection.execute("PRAGMA synchronous=FULL")
    return connection


def run_embedded_sql_once(
    module: Any, rows: list[tuple[int, str, int, str]], root: Path, name: str
) -> dict[str, float]:
    database_path = root / f"{name}.db"
    recovery_path = root / f"{name}-before-apply.db"
    export_path = root / f"{name}.csv"
    connection = sqlite_connection(module, database_path)
    timings: dict[str, float] = {}

    def import_data() -> None:
        connection.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, bucket TEXT, value INTEGER, label TEXT)"
        )
        connection.execute("BEGIN")
        connection.executemany("INSERT INTO events VALUES (?, ?, ?, ?)", rows)
        connection.commit()

    timings["import"], _ = measure(import_data)

    def query() -> None:
        result = connection.execute(QUERY).fetchall()
        if not result:
            raise BenchmarkError(f"{name} query returned no aggregate rows")

    timings["query"], _ = measure(query)

    def preview() -> None:
        connection.execute("BEGIN")
        connection.execute(UPDATE)
        if name == "sqlite":
            connection.execute("SELECT changes()").fetchone()
        connection.rollback()

    timings["preview"], _ = measure(preview)

    def apply() -> None:
        nonlocal connection
        connection.commit()
        if name == "duckdb":
            connection.execute("CHECKPOINT")
        connection.close()
        shutil.copy2(database_path, recovery_path)
        connection = sqlite_connection(module, database_path)
        connection.execute("BEGIN")
        connection.execute(UPDATE)
        connection.commit()

    timings["apply"], _ = measure(apply)

    def diff() -> None:
        before = sqlite_connection(module, recovery_path)
        before_rows = before.execute(
            "SELECT id, bucket, value, label FROM events ORDER BY id"
        ).fetchall()
        current_rows = connection.execute(
            "SELECT id, bucket, value, label FROM events ORDER BY id"
        ).fetchall()
        before.close()
        if before_rows == current_rows:
            raise BenchmarkError(f"{name} diff did not report the applied change")

    timings["diff"], _ = measure(diff)

    def undo() -> None:
        nonlocal connection
        connection.close()
        shutil.copy2(recovery_path, database_path)
        connection = sqlite_connection(module, database_path)
        restored = connection.execute(
            "SELECT label FROM events WHERE id = 1"
        ).fetchone()
        if restored != ("row-1",):
            raise BenchmarkError(f"{name} recovery did not restore the original row")

    timings["undo"], _ = measure(undo)

    def export() -> None:
        with export_path.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, lineterminator="\n")
            writer.writerow(("id", "bucket", "value", "label"))
            writer.writerows(
                connection.execute(
                    "SELECT id, bucket, value, label FROM events ORDER BY id"
                ).fetchall()
            )
        with export_path.open(encoding="utf-8", newline="") as handle:
            exported_rows = sum(1 for _ in csv.reader(handle)) - 1
        if exported_rows != len(rows):
            raise BenchmarkError(f"{name} exported {exported_rows} rows; expected {len(rows)}")

    timings["export"], _ = measure(export)
    connection.close()
    return timings


def backend_report(
    name: str,
    repeats: int,
    operation: Callable[[Path], dict[str, float]],
    notes: list[str],
) -> dict[str, Any]:
    samples: dict[str, list[float]] = {}
    for _ in range(repeats):
        with tempfile.TemporaryDirectory(prefix=f"basalt-bench-{name}-") as directory:
            timings = operation(Path(directory))
        for label, elapsed in timings.items():
            samples.setdefault(label, []).append(elapsed)
    return {
        "status": "ok",
        "operations": {label: summarize(values) for label, values in sorted(samples.items())},
        "notes": notes,
    }


def unavailable_report(reason: str, notes: list[str]) -> dict[str, Any]:
    return {"status": "unavailable", "reason": reason, "notes": notes}


def package_version(binary: str) -> str:
    return run_command([binary, "--version"]).strip()


def git_revision() -> str | None:
    try:
        return run_command(["git", "rev-parse", "HEAD"]).strip()
    except (BenchmarkError, OSError):
        return None


def main() -> int:
    arguments = parse_args()
    if arguments.rows < 1 or arguments.rows > 100_000:
        raise BenchmarkError("--rows must be between 1 and 100000")
    if arguments.repeats < 1 or arguments.repeats > 20:
        raise BenchmarkError("--repeats must be between 1 and 20")

    binary = resolve_binary(arguments.basalt)
    rows = fixture_rows(arguments.rows)
    notes = [
        "Basalt timings include one CLI process, workspace open, and JSON/text conversion per operation.",
        "SQLite and DuckDB timings stay in one Python process; compare workflow shape, not raw engine throughput.",
        "SQLite/DuckDB preview uses transaction rollback and apply/undo uses a file copy; neither supplies Basalt's exact plan ledger.",
        "The fixture uses only the documented subset needed for this workflow; it is not a full dialect benchmark.",
    ]

    report: dict[str, Any] = {
        "schema_version": 1,
        "workload": {
            "rows": arguments.rows,
            "repeats": arguments.repeats,
            "query": QUERY,
            "mutation": UPDATE,
        },
        "environment": {
            "basalt": package_version(binary),
            "binary": str(Path(binary).resolve()),
            "python": platform.python_version(),
            "platform": platform.platform(),
            "machine": platform.machine(),
            "git_revision": git_revision(),
        },
        "backends": {},
    }

    report["backends"]["basalt"] = backend_report(
        "basalt",
        arguments.repeats,
        lambda root: run_basalt_once(binary, rows, root),
        notes,
    )
    report["backends"]["sqlite"] = backend_report(
        "sqlite",
        arguments.repeats,
        lambda root: run_embedded_sql_once(sqlite3, rows, root, "sqlite"),
        notes,
    )

    try:
        import duckdb  # type: ignore[import-not-found]
    except ImportError as error:
        report["backends"]["duckdb"] = unavailable_report(
            f"Python DuckDB client is unavailable: {error}",
            notes,
        )
    else:
        report["backends"]["duckdb"] = backend_report(
            "duckdb",
            arguments.repeats,
            lambda root: run_embedded_sql_once(duckdb, rows, root, "duckdb"),
            notes,
        )

    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as error:
        print(f"benchmark: {error}", file=sys.stderr)
        raise SystemExit(1)
