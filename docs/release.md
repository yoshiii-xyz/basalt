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

On Unix, `bash scripts/release-check.sh` runs the quality gates above, checks
the packaged crate, installs that exact package into a temporary prefix, and
runs the installed-binary CLI/MCP smoke journey. It also runs `cargo audit`
and `dist plan` when those tools are installed. The full 10,000-row benchmark
remains a separate measurement because its output is machine-dependent.

When `dist` is installed, the preflight also builds the local release archive
for the host target and runs `scripts/verify-release-artifacts.py` against that
exact archive and checksum. The release workflow applies the same structural
check to every target archive before hosting them, then extracts the native
Linux archive and runs the complete smoke journey from the extracted binary.

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

`dist-workspace.toml` is the source of truth for the release target matrix,
installer types, and immutable action pins. `dist generate` refreshes the
checked-in GitHub Actions workflow; `dist plan` shows the artifacts without
publishing anything. A version tag such as `vVERSION` builds the release
archives, shell and PowerShell
installers, and SHA-256 files. The workflow keeps a reviewed least-privilege
permission override, so `allow-dirty = ["ci"]` is intentional; review that
override and archive-verification step after regenerating the generated file.

The package is named `basalt-db` to avoid an existing crates.io name conflict,
but every archive contains a `basalt` executable. The crate install is
`cargo install basalt-db --locked`; the `v0.1.1` prebuilt download and installer
are published and have passed the public checksum and isolated consumer smoke
checks.

The [production-readiness contract](production-readiness.md) is the authority
for technical scope, fixed limits, backup/restore, and evidence. The public
`v0.1.1` artifacts do not automatically include hardening committed after that
release; the current hardened candidate is `v0.1.2`. Publish a new version only
after its package, archives, checksums,
installer, and registry metadata are verified together.

The hosted CI gate checks the full suite on Ubuntu, checks the MSRV separately,
and compiles and tests on macOS and Windows runners. The release workflow's
artifact matrix remains the source of truth for the published architectures.

The MCP Registry is a separate metadata publication. Its current preview
supports multiple package types, including Cargo and MCPB, but does not host
the artifacts themselves. The repository contains `server.json` and a visible
`mcp-name: io.github.joshiii-xyz/basalt` marker in the README. The `v0.1.1`
metadata is published at the [Basalt Registry listing](https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.joshiii-xyz%2Fbasalt).
The metadata verifier checks that both version fields match the Cargo package
and that the package launches the `mcp` subcommand. Publish future metadata
only after its corresponding crate and GitHub release are available.

## Publication steps for future releases

Publication is intentionally manual because it creates external releases. The
following sequence was completed for `v0.1.1`; replace `VERSION` with a new
version for a future release. Run the dry run and publish the package first,
then push the version tag to start the GitHub release workflow:

Make the release commit's README, changelog, and packaged documentation final
before publishing. Do not update those files after `cargo publish` to announce
that the release exists: crates.io versions are immutable, so a post-publish
documentation commit makes the repository's `main` branch differ from the
documentation shipped in the tagged package. Put release-status wording in
the release notes or use version-neutral wording that is true before and after
publication.

```bash
cargo publish --dry-run --locked
cargo publish --locked
VERSION=0.1.2
git tag -a "v${VERSION}" -m "Basalt v${VERSION}"
git push origin "v${VERSION}"
```

After the tag workflow succeeds, inspect the GitHub Release assets and their
checksums, then run the installer and `scripts/smoke-test.sh` from an isolated
consumer environment or clean machine. Do not describe an artifact as
available until those checks pass.

Once the package and binary release are verified, install the official
`mcp-publisher` CLI, create or update `server.json` with the same server name
and version, authenticate with `mcp-publisher login github`, and publish with
`mcp-publisher publish`. Verify the result through the Registry API. Registry
versions are immutable, so treat the metadata as another versioned release
artifact and rerun validation whenever its version changes.

## Smoke tests

1. Install the binary from the checkout with `cargo install --path . --locked`
   or use a published release installer.
2. Run `bash scripts/smoke-test.sh /path/to/basalt` against the installed
   binary. It covers version output, workspace import/query, reversible
   mutation, export, MCP bootstrap, read-only MCP discovery, and a process-level
   writable MCP import/preview/apply/diff/undo/export journey.
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
- Verify every generated archive and checksum with
  `python3 scripts/verify-release-artifacts.py target/distrib` before hosting
  release assets.
- Review `git diff --check`, `git status`, and the final commit history.
- Push the release commit only after all required checks and smoke tests pass.

## Product boundary

The release contract covers Basalt's documented SQL dialect, one-process
embedded database handles, the CLI, and the local stdio MCP server. Durable
database paths and workspaces are exclusively owned by one process; use cloned
handles for threads in that process. It does not claim compatibility with a
full external SQL implementation or provide a network service/authentication
layer.
