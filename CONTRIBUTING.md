# Contributing to Basalt

Basalt is intentionally small. Keep changes focused, document user-visible
behavior, and add a regression test for fixes. Protocol changes must preserve
the MCP server's stdio boundary: stdout is reserved for JSON-RPC and
diagnostics belong on stderr.

## Local checks

Run the same checks used by CI before opening a pull request:

~~~text
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo doc --no-deps --locked
cargo package --locked
~~~

For storage or query-planner changes, also run:

~~~text
cargo bench --bench throughput
~~~

For all changes, run strict linting when the Rust Clippy component is
available:

~~~text
cargo clippy --all-targets --all-features --locked -- -D warnings
~~~

For MCP changes, run the wire-level integration coverage as well:

~~~text
cargo test --test mcp -- --nocapture
~~~

Keep tool input and output schemas typed, return database failures as tool
errors, and keep result sizes bounded so an agent cannot accidentally flood its
context window.

Please avoid committing generated files such as target/, local database
files, editor settings, or benchmark output.

See [docs/release.md](docs/release.md) for the full release and smoke-test
checklist.
