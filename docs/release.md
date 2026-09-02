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
bash scripts/smoke-test.sh target/release/basalt
python3 scripts/benchmark_workspace.py --basalt target/release/basalt \
  --rows 10000 --repeats 3 > benchmark.json
```

The test command includes the throughput benchmark target. Run the in-process
benchmark directly when a storage, planner, or execution change could affect
its result:

```bash
cargo bench --bench throughput
```

The workflow benchmark is the comparable structured-data protocol:

```bash
python3 scripts/benchmark_workspace.py --basalt target/release/basalt \
  --rows 10000 --repeats 3 > benchmark.json
```

Dependency audit tools are optional local/CI additions. If `cargo audit` or a
similar tool is available, run it and record the result in the release notes;
do not claim an audit was performed when the tool was unavailable.

## Distribution

`dist-workspace.toml` is the source of truth for the release target matrix and
installer types. `dist generate` refreshes the checked-in GitHub Actions
workflow; `dist plan` shows the artifacts without publishing anything. A
version tag such as `v0.1.0` builds the release archives, shell and PowerShell
installers, and SHA-256 files. The workflow keeps a reviewed least-privilege
permission override, so `allow-dirty = ["ci"]` is intentional; review that
override after regenerating the generated file.

The package is named `basalt-db` to avoid an existing crates.io name conflict,
but every archive contains a `basalt` executable. Do not describe a crates.io
install or a prebuilt download as available until that artifact has actually
been published.

The hosted CI gate checks the full suite on Ubuntu, checks the MSRV separately,
and compiles all targets on macOS and Windows runners. The release workflow's
artifact matrix remains the source of truth for the published architectures.

## Smoke tests

1. Install the binary from the checkout with `cargo install --path . --locked`
   or use a published release installer.
2. Run `bash scripts/smoke-test.sh /path/to/basalt` against the installed
   binary. It covers version output, workspace import/query, reversible
   mutation, export, and read-only MCP discovery.
3. Create a durable database with the CLI, query it in table/CSV/JSON-lines
   modes, close it, and query it again.
4. Create a workspace, import a representative CSV or JSON Lines file,
   inspect it, export it as CSV/JSON Lines/SQL, and reopen the workspace.
5. Exercise `BEGIN`, `COMMIT`, and `ROLLBACK` across repeated `--command`
   actions and through the interactive shell.
6. Start `basalt mcp :memory:` and verify `server/discover` (or legacy
   `initialize`), `tools/list`, `tools/call`, `resources/list`, and
   `resources/read` over newline-delimited JSON-RPC on stdout. Confirm direct
   writes are denied until `--allow-writes` is supplied.
7. Create an empty workspace and verify the same MCP process can import bounded
   structured content, inspect, preview, apply, diff, undo, and export without
   arbitrary filesystem paths. Confirm workspace mode does not list the
   unrestricted `execute` tool and denies the import until `--allow-writes` is
   supplied.
8. Verify a durable MCP database survives a server restart and that all
   diagnostics stay off stdout.

## Release review

- Update `CHANGELOG.md` with user-visible changes.
- Confirm the version in `Cargo.toml` is intentional and `Cargo.lock` is
  current.
- Confirm the package is named `basalt-db` and the installed binary remains
  `basalt` before publishing to crates.io.
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
