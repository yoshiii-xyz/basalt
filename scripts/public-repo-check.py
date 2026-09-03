#!/usr/bin/env python3
"""Reject accidental private artifacts and high-confidence credentials."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path


FORBIDDEN_DIRECTORIES = {
    ".pytest_cache",
    "__pycache__",
    "coverage",
    "node_modules",
    "qa",
    "target",
}
FORBIDDEN_FUZZ_DIRECTORIES = {"artifacts", "corpus"}
FORBIDDEN_NAMES = {
    ".DS_Store",
    ".env",
    ".env.local",
    ".env.production",
    ".env.test",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "credentials",
    "credentials.toml",
    "id_ed25519",
    "id_rsa",
}
FORBIDDEN_SUFFIXES = (
    ".basalt",
    ".basalt-wal",
    ".db",
    ".db-wal",
    ".db.tmp",
    ".log",
    ".profraw",
    ".sqlite",
    ".sqlite3",
    ".tmp",
)
SECRET_PATTERNS = (
    re.compile(r"AKIA[0-9A-Z]{16}"),
    re.compile(r"ASIA[0-9A-Z]{16}"),
    re.compile(r"gh[pousr]_[A-Za-z0-9_]{20,}"),
    re.compile(r"github_pat_[A-Za-z0-9_]{20,}"),
    re.compile(r"sk-[A-Za-z0-9]{20,}"),
    re.compile(r"xox[baprs]-[A-Za-z0-9-]{20,}"),
    re.compile(r"npm_[A-Za-z0-9]{20,}"),
    re.compile(r"cio[A-Za-z0-9]{30,}"),
    re.compile(r"-----BEGIN (?:RSA|OPENSSH|EC|DSA|PGP) PRIVATE KEY-----"),
)


def tracked_paths() -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "-z"],
        check=True,
        stdout=subprocess.PIPE,
    )
    return [Path(raw) for raw in result.stdout.decode().split("\0") if raw]


def private_path(path: Path) -> bool:
    if any(part in FORBIDDEN_DIRECTORIES for part in path.parts):
        return True
    if (
        len(path.parts) > 1
        and path.parts[0] == "fuzz"
        and path.parts[1] in FORBIDDEN_FUZZ_DIRECTORIES
    ):
        return True
    if path.name in FORBIDDEN_NAMES:
        return True
    if path.name.startswith(".env.") and path.name not in {".env.example", ".env.sample"}:
        return True
    return path.name.endswith(FORBIDDEN_SUFFIXES)


def secret_hits(path: Path) -> list[int]:
    try:
        text = path.read_bytes().decode("utf-8")
    except (OSError, UnicodeDecodeError):
        return []

    hits: list[int] = []
    for pattern in SECRET_PATTERNS:
        for match in pattern.finditer(text):
            hits.append(text.count("\n", 0, match.start()) + 1)
    return sorted(set(hits))


def main() -> int:
    paths = tracked_paths()
    bad_paths = sorted(str(path) for path in paths if private_path(path))
    bad_secrets = [(str(path), secret_hits(path)) for path in paths]
    bad_secrets = [(path, lines) for path, lines in bad_secrets if lines]

    if bad_paths or bad_secrets:
        print("public repository check failed", file=sys.stderr)
        for path in bad_paths:
            print(f"forbidden tracked path: {path}", file=sys.stderr)
        for path, lines in bad_secrets:
            line_list = ",".join(str(line) for line in lines)
            print(f"possible credential pattern in {path}:{line_list}", file=sys.stderr)
        return 1

    print(f"public repository check passed: {len(paths)} tracked paths")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
