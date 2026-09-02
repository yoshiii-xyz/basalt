# Basalt release checklist

Basalt is released as a CLI-first embedded SQL library and command-line
application. A release is ready only when the repository, installed binary,
durable database path, and stdio MCP server pass the same checks.

## Required checks

Run these commands from a clean checkout:

```bash
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
cargo package --locked
cargo build --release --locked
```

The test command includes the throughput benchmark target. Run the benchmark
directly when a storage, planner, or execution change could affect its result:

```bash
cargo bench --bench throughput
```

Dependency audit tools are optional local/CI additions. If `cargo audit` or a
similar tool is available, run it and record the result in the release notes;
do not claim an audit was performed when the tool was unavailable.

## Smoke tests

1. Install the binary from the checkout with `cargo install --path . --locked`.
2. Create a durable database with the CLI, query it in table/CSV/JSON-lines
   modes, close it, and query it again.
3. Create a workspace, import a representative CSV or JSON Lines file,
   inspect it, export it as CSV/JSON Lines/SQL, and reopen the workspace.
4. Exercise `BEGIN`, `COMMIT`, and `ROLLBACK` across repeated `--command`
   actions and through the interactive shell.
5. Start `basalt mcp :memory:` and verify `server/discover` (or legacy
   `initialize`), `tools/list`, `tools/call`, `resources/list`, and
   `resources/read` over newline-delimited JSON-RPC on stdout.
6. Verify a durable MCP database survives a server restart and that all
   diagnostics stay off stdout.

## Release review

- Update `CHANGELOG.md` with user-visible changes.
- Confirm the version in `Cargo.toml` is intentional and `Cargo.lock` is
  current.
- Inspect `cargo package --list` for accidental files and verify the README
  logo and links render from the packaged crate/repository.
- Review `git diff --check`, `git status`, and the final commit history.
- Push the release commit only after all required checks and smoke tests pass.

## Product boundary

The release contract covers Basalt's documented SQL dialect, one-process
embedded database handles, the CLI, and the local stdio MCP server. Durable
database paths are exclusively owned by one process; use cloned handles for
threads in that process. It does not claim compatibility with a full external
SQL implementation or provide a network service/authentication layer.
