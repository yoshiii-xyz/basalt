#!/usr/bin/env python3
"""Compare the documented SQL subset with Python's SQLite implementation.

This is deliberately a small compatibility probe. It does not establish
SQLite compatibility; it catches regressions in ordinary SELECT, expression,
aggregate, and mutation behavior where both engines have an intentionally
shared contract.
"""

from __future__ import annotations

import argparse
import json
import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path
from typing import Any


CASES: dict[str, list[str]] = {
    "filter-order-and-aggregate": [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score INTEGER);",
        "INSERT INTO users VALUES (1, 'Ada', 11);",
        "INSERT INTO users VALUES (2, 'Grace', 9);",
        "INSERT INTO users VALUES (3, 'Lin', 14);",
        "INSERT INTO users VALUES (4, 'Null', NULL);",
        "SELECT id, name, score FROM users WHERE score >= 10 ORDER BY score DESC, id;",
        "SELECT COUNT(*) AS total, SUM(score) AS total_score FROM users;",
    ],
    "mutation-and-coalesce": [
        "CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT, score INTEGER);",
        "INSERT INTO items VALUES (1, 'one', 3);",
        "INSERT INTO items VALUES (2, 'two', NULL);",
        "UPDATE items SET score = COALESCE(score, 0) + 5 WHERE id = 2;",
        "DELETE FROM items WHERE id = 1;",
        "SELECT id, label, score FROM items ORDER BY id;",
    ],
    "join-and-group": [
        "CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT);",
        "CREATE TABLE members (id INTEGER PRIMARY KEY, team_id INTEGER, name TEXT);",
        "INSERT INTO teams VALUES (1, 'red');",
        "INSERT INTO teams VALUES (2, 'blue');",
        "INSERT INTO members VALUES (1, 1, 'Ada');",
        "INSERT INTO members VALUES (2, 1, 'Grace');",
        "INSERT INTO members VALUES (3, 2, 'Lin');",
        "SELECT teams.name AS team, COUNT(members.id) AS members FROM teams LEFT JOIN members ON teams.id = members.team_id GROUP BY teams.name ORDER BY teams.name;",
    ],
}


class DifferentialError(RuntimeError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Differential-test Basalt SQL against SQLite.")
    parser.add_argument(
        "--basalt",
        default="target/release/basalt",
        help="Basalt executable (default: target/release/basalt)",
    )
    return parser.parse_args()


def resolve_binary(requested: str) -> str:
    path = Path(requested)
    if path.parent != Path(".") or path.is_absolute():
        if not path.is_file():
            raise DifferentialError(f"Basalt executable was not found: {requested}")
        return str(path)
    resolved = shutil.which(requested)
    if resolved is None:
        raise DifferentialError(f"Basalt executable was not found: {requested}")
    return resolved


def basalt_selects(binary: str, statements: list[str]) -> list[dict[str, Any]]:
    command = [binary, "--json"]
    for statement in statements:
        command.extend(("--command", statement))
    command.append(":memory:")
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise DifferentialError(f"Basalt case failed: {detail}")
    results = []
    for line in completed.stdout.splitlines():
        value = json.loads(line)
        if value.get("type") == "select":
            results.append({"columns": value["columns"], "rows": value["rows"]})
    return results


def sqlite_selects(statements: list[str]) -> list[dict[str, Any]]:
    connection = sqlite3.connect(":memory:")
    results = []
    try:
        for statement in statements:
            cursor = connection.execute(statement)
            if cursor.description is None:
                continue
            results.append(
                {
                    "columns": [column[0] for column in cursor.description],
                    "rows": [list(row) for row in cursor.fetchall()],
                }
            )
    finally:
        connection.close()
    return results


def main() -> int:
    arguments = parse_args()
    binary = resolve_binary(arguments.basalt)
    checked = 0
    for name, statements in CASES.items():
        expected = sqlite_selects(statements)
        actual = basalt_selects(binary, statements)
        if actual != expected:
            raise DifferentialError(
                f"case {name!r} diverged\nSQLite: {json.dumps(expected)}\nBasalt: {json.dumps(actual)}"
            )
        checked += 1
        print(f"passed {name}")
    print(f"Differential SQL checks passed: {checked}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (DifferentialError, json.JSONDecodeError, sqlite3.Error) as error:
        print(f"differential test: {error}", file=sys.stderr)
        raise SystemExit(1)
