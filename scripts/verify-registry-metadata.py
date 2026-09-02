#!/usr/bin/env python3
"""Check the checked-in MCP Registry metadata against this package."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
EXPECTED_NAME = "io.github.joshiii-xyz/basalt"
EXPECTED_REPOSITORY = "https://github.com/joshiii-xyz/basalt"
EXPECTED_PACKAGE = "basalt-db"


class MetadataError(RuntimeError):
    pass


def fail(message: str) -> MetadataError:
    return MetadataError(f"registry metadata: {message}")


def require_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise fail(f"{field} must be a non-empty string")
    return value


def package_version() -> str:
    try:
        result = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise fail(f"could not read Cargo package metadata: {error}") from error
    try:
        metadata = json.loads(result.stdout)
        packages = metadata["packages"]
        package = next(item for item in packages if item["name"] == EXPECTED_PACKAGE)
        return require_string(package["version"], "Cargo package version")
    except (KeyError, StopIteration, TypeError, json.JSONDecodeError) as error:
        raise fail(f"Cargo metadata has no {EXPECTED_PACKAGE!r} package") from error


def main() -> int:
    try:
        metadata = json.loads((ROOT / "server.json").read_text(encoding="utf-8"))
        if not isinstance(metadata, dict):
            raise fail("document must be an object")
        version = package_version()
        if require_string(metadata.get("name"), "name") != EXPECTED_NAME:
            raise fail(f"name must be {EXPECTED_NAME!r}")
        if require_string(metadata.get("version"), "version") != version:
            raise fail(f"version must match Cargo ({version})")

        repository = metadata.get("repository")
        if not isinstance(repository, dict):
            raise fail("repository must be an object")
        if (
            require_string(repository.get("url"), "repository.url")
            != EXPECTED_REPOSITORY
        ):
            raise fail(f"repository.url must be {EXPECTED_REPOSITORY!r}")
        if require_string(repository.get("source"), "repository.source") != "github":
            raise fail("repository.source must be 'github'")

        packages = metadata.get("packages")
        if not isinstance(packages, list) or len(packages) != 1:
            raise fail("packages must contain exactly one Cargo package")
        package = packages[0]
        if not isinstance(package, dict):
            raise fail("package must be an object")
        if (
            require_string(package.get("registryType"), "packages[0].registryType")
            != "cargo"
        ):
            raise fail("packages[0].registryType must be 'cargo'")
        if (
            require_string(package.get("registryBaseUrl"), "packages[0].registryBaseUrl")
            != "https://crates.io"
        ):
            raise fail("packages[0].registryBaseUrl must be https://crates.io")
        if (
            require_string(package.get("identifier"), "packages[0].identifier")
            != EXPECTED_PACKAGE
        ):
            raise fail(f"packages[0].identifier must be {EXPECTED_PACKAGE!r}")
        if require_string(package.get("version"), "packages[0].version") != version:
            raise fail(f"packages[0].version must match Cargo ({version})")
        transport = package.get("transport")
        if not isinstance(transport, dict) or transport.get("type") != "stdio":
            raise fail("packages[0].transport.type must be 'stdio'")
        arguments = package.get("packageArguments")
        if not isinstance(arguments, list) or arguments != [
            {"type": "positional", "value": "mcp"}
        ]:
            raise fail("packages[0].packageArguments must launch the `mcp` subcommand")

        marker = f"mcp-name: {EXPECTED_NAME}"
        marker_lines = [line for line in (ROOT / "README.md").read_text(encoding="utf-8").splitlines() if marker in line]
        if not marker_lines or any("<!--" in line or "-->" in line for line in marker_lines):
            raise fail("README must contain the visible Cargo ownership marker")
    except (OSError, json.JSONDecodeError, MetadataError) as error:
        print(error, file=sys.stderr)
        return 1

    print(f"MCP Registry metadata passed ({EXPECTED_NAME} {version})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
