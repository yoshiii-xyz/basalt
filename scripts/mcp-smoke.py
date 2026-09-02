#!/usr/bin/env python3
"""Exercise the installed binary's writable workspace MCP contract."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any


MODERN_METADATA: dict[str, Any] = {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientInfo": {
        "name": "basalt-smoke",
        "version": "1.0.0",
    },
    "io.modelcontextprotocol/clientCapabilities": {},
}


class McpProcess:
    def __init__(self, binary: str, workspace: Path) -> None:
        self.process = subprocess.Popen(
            [binary, "mcp", "--workspace", str(workspace), "--allow-writes"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )

    def send(self, message: dict[str, Any]) -> None:
        if self.process.stdin is None:
            raise RuntimeError("MCP stdin is unavailable")
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def request(self, request_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        if self.process.stdout is None:
            raise RuntimeError("MCP stdout is unavailable")
        for line in self.process.stdout:
            message = json.loads(line)
            if message.get("id") != request_id:
                continue
            if "error" in message:
                raise RuntimeError(f"MCP request failed: {message['error']}")
            return message["result"]
        raise RuntimeError(f"MCP exited before responding to request {request_id}")

    def close(self) -> None:
        if self.process.stdin is not None:
            self.process.stdin.close()
        try:
            return_code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired as error:
            self.process.kill()
            self.process.wait()
            raise RuntimeError("MCP server did not stop after stdin closed") from error
        stderr = self.process.stderr.read() if self.process.stderr is not None else ""
        if return_code != 0:
            raise RuntimeError(f"MCP server exited with {return_code}: {stderr}")


def call(
    server: McpProcess,
    request_id: int,
    name: str,
    arguments: dict[str, Any],
    metadata: dict[str, Any],
) -> dict[str, Any]:
    result = server.request(
        request_id,
        "tools/call",
        {"name": name, "arguments": arguments, "_meta": metadata},
    )
    if result.get("isError") is True:
        raise RuntimeError(f"MCP tool {name} failed: {result}")
    structured = result.get("structuredContent")
    if not isinstance(structured, dict):
        raise RuntimeError(f"MCP tool {name} returned no structured content: {result}")
    return structured


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} PATH-TO-BASALT WORKSPACE")

    binary = sys.argv[1]
    workspace = Path(sys.argv[2])
    server = McpProcess(binary, workspace)
    try:
        discovery = server.request(
            1,
            "server/discover",
            {"_meta": MODERN_METADATA},
        )
        if "2026-07-28" not in discovery["supportedVersions"]:
            raise RuntimeError(f"modern MCP protocol was not advertised: {discovery}")

        tools = server.request(2, "tools/list", {"_meta": MODERN_METADATA})["tools"]
        names = {tool["name"] for tool in tools}
        required = {
            "list_tables",
            "query",
            "workspace_import",
            "workspace_preview",
            "workspace_apply",
            "workspace_diff",
            "workspace_undo",
            "workspace_export",
        }
        if not required <= names or "execute" in names:
            raise RuntimeError(f"unexpected workspace tools: {sorted(names)}")

        imported = call(
            server,
            3,
            "workspace_import",
            {
                "table": "mcp_users",
                "format": "csv",
                "content": "id,name\n1,Ada\n",
            },
            MODERN_METADATA,
        )
        import_change_id = imported["change_id"]
        if imported["summary"] != "table mcp_users (1 rows, 2 columns)":
            raise RuntimeError(f"unexpected import report: {imported}")

        queried = call(
            server,
            4,
            "query",
            {"sql": "SELECT name FROM mcp_users WHERE id = 1"},
            MODERN_METADATA,
        )
        rows = queried["results"][0]["rows"]
        if rows != [[{"type": "text", "value": "Ada"}]]:
            raise RuntimeError(f"unexpected query result: {queried}")

        preview = call(
            server,
            5,
            "workspace_preview",
            {"sql": "UPDATE mcp_users SET name = 'Grace' WHERE id = 1"},
            MODERN_METADATA,
        )
        plan_id = preview["plan_id"]

        applied = call(
            server,
            6,
            "workspace_apply",
            {"plan_id": plan_id},
            MODERN_METADATA,
        )
        applied_change_id = applied["change_id"]
        if applied_change_id == import_change_id:
            raise RuntimeError("apply reused the import change identifier")

        diff = call(
            server,
            7,
            "workspace_diff",
            {"change_id": applied_change_id},
            MODERN_METADATA,
        )
        if diff["state_changed"] is not True:
            raise RuntimeError(f"apply produced no diff: {diff}")

        undone = call(
            server,
            8,
            "workspace_undo",
            {"change_id": applied_change_id},
            MODERN_METADATA,
        )
        if undone["undone_change_id"] != applied_change_id:
            raise RuntimeError(f"unexpected undo report: {undone}")

        exported = call(
            server,
            9,
            "workspace_export",
            {"table": "mcp_users", "format": "csv"},
            MODERN_METADATA,
        )
        if exported["content"] != "id,name\n1,Ada\n":
            raise RuntimeError(f"unexpected export content: {exported}")
    finally:
        server.close()

    print("Basalt writable MCP smoke test passed")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, json.JSONDecodeError, KeyError, TypeError) as error:
        print(f"MCP smoke test failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
