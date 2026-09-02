#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'usage: %s [PATH-TO-BASALT]\n' "$0" >&2
}

if [[ "$#" -gt 1 ]]; then
    usage
    exit 2
fi

requested_binary=${1:-basalt}
if [[ "$requested_binary" == */* ]]; then
    basalt_binary=$requested_binary
else
    basalt_binary=$(command -v "$requested_binary" || true)
fi

if [[ -z "$basalt_binary" || ! -x "$basalt_binary" ]]; then
    printf 'smoke test: executable not found: %s\n' "$requested_binary" >&2
    exit 2
fi

temp_root=$(mktemp -d "${TMPDIR:-/tmp}/basalt-smoke.XXXXXX")
trap 'rm -rf -- "$temp_root"' EXIT

workspace="$temp_root/workspace"
source="$temp_root/users.csv"
exported="$temp_root/users.jsonl"

version_output=$("$basalt_binary" --version)
[[ "$version_output" == basalt\ * ]]

"$basalt_binary" init "$workspace" >/dev/null
printf 'id,name\n1,Ada\n2,Grace\n' >"$source"
"$basalt_binary" workspace import --table users "$workspace" "$source" >/dev/null

query_output=$("$basalt_binary" workspace query --json "$workspace" \
    'SELECT id, name FROM users ORDER BY id')
grep -Fq '{"type":"select","columns":["id","name"],"rows":[[1,"Ada"],[2,"Grace"]]}' <<<"$query_output"

preview_output=$("$basalt_binary" workspace preview --json "$workspace" \
    "UPDATE users SET name = 'Updated' WHERE id = 1")
plan_id=$(sed -n 's/[[:space:]]*"plan_id": "\([^"]*\)".*/\1/p' <<<"$preview_output")
[[ -n "$plan_id" ]]

apply_output=$("$basalt_binary" workspace apply --json "$workspace" "$plan_id")
change_id=$(sed -n 's/[[:space:]]*"change_id": "\([^"]*\)".*/\1/p' <<<"$apply_output")
[[ -n "$change_id" ]]

diff_output=$("$basalt_binary" workspace diff --json "$workspace" "$change_id")
grep -Fq '"state_changed": true' <<<"$diff_output"

"$basalt_binary" workspace undo --json "$workspace" "$change_id" >/dev/null
"$basalt_binary" workspace export --format jsonl "$workspace" users "$exported" >/dev/null
grep -Fq '{"id":1,"name":"Ada"}' "$exported"

mcp_output=$(
    {
        printf '%s\n' \
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"basalt-smoke","version":"1.0.0"}}}' \
            '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
            '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
            '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"workspace_import","arguments":{"table":"other","format":"csv","content":"id\n1\n"}}}' \
            '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"workspace_inspect","arguments":{}}}'
    } | "$basalt_binary" mcp --workspace "$workspace"
)
grep -Fq '"serverInfo"' <<<"$mcp_output"
grep -Fq '"workspace_import"' <<<"$mcp_output"
grep -Fq 'writes are disabled' <<<"$mcp_output"
grep -Fq '"workspace_preview"' <<<"$mcp_output"
grep -Fq '"workspace_inspect"' <<<"$mcp_output"
grep -Fq '"users"' <<<"$mcp_output"

printf 'Basalt smoke test passed (%s)\n' "$version_output"
