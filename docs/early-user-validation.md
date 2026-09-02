# Early-user validation

This is the validation protocol for Basalt's first switching wedge. It is not
an adoption claim. No external-user results have been collected until someone
completes this protocol and records the result.

## Who should try it

Use this if you are a developer using Claude Code, Codex, Cursor, or another
coding agent and you regularly give the agent CSV, JSON, logs, issue exports,
fixtures, or other structured data to inspect and change locally.

Basalt is not the right test if you need an existing SQLite file, full SQL
compatibility, concurrent writers, a hosted database, or a graphical editor.

## Five-minute test

Use a disposable copy of a real, non-sensitive input file. From an installed
Basalt binary or a checkout, run:

```bash
basalt init .basalt-workspace
basalt workspace import --table records .basalt-workspace records.csv
basalt workspace inspect --json .basalt-workspace
basalt workspace query --json .basalt-workspace \
  "SELECT * FROM records ORDER BY 1 LIMIT 10"
basalt workspace preview --json .basalt-workspace \
  "UPDATE records SET status = 'reviewed' WHERE status = 'open'"
```

Review the exact SQL, affected-row count, and returned `plan_id`. Apply that
plan, inspect the change, undo it, and verify that the original rows are back:

```bash
basalt workspace apply --json .basalt-workspace PLAN_ID
basalt workspace history --json .basalt-workspace
basalt workspace diff --json .basalt-workspace CHANGE_ID
basalt workspace undo --json .basalt-workspace CHANGE_ID
basalt workspace export --format jsonl .basalt-workspace records -
```

Replace `records.csv`, the table name, and the SQL with the smallest real task
you would otherwise perform with a shell script, temporary SQLite database,
Python, or a generic database MCP server. The data stays local. Stop any MCP
process before opening the workspace from the CLI.

To test the agent path, configure the same installed binary as a local stdio
MCP server using [the MCP guide](mcp.md), then repeat the sequence with
`workspace_import`, `workspace_inspect`, `query`, `workspace_preview`,
`workspace_plan`, `workspace_apply`, `workspace_history`, `workspace_diff`,
`workspace_undo`, and `workspace_export`.

## Record the result

Copy this template into a private note or a GitHub issue. Do not include
customer data, credentials, or proprietary files.

```text
Date:
Basalt version or commit:
Operating system:
Agent/host:
Input shape and approximate row count:
Current workflow replaced or compared:

Completed the CLI sequence: yes/no
Completed the MCP sequence: yes/no/not tested
Could you understand the proposed write before applying it: yes/no
Did diff/history/undo provide enough recovery confidence: yes/no
Did a documented limit block the task: yes/no (which one)
Would you use Basalt for the next task: yes/no/uncertain
What would make you switch from the current workflow:
Most important missing capability or confusing step:
```

The useful evidence is a completed or abandoned task, the current tool it was
compared with, and the concrete blocker. A positive reaction without a real
task is interest, not validation.

## What changes the roadmap

- A repeated blocker in the selected workflow can justify one bounded product
  change.
- Requests for a GUI, hosted service, broad SQL compatibility, or generic
  agent memory do not change this release's scope; they are separate products.
- Performance complaints are actionable only with the input shape, operation,
  row count, and comparison method recorded.
