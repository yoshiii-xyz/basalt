# Contributing to Basalt

Basalt is intentionally small and dependency-free. Keep changes focused,
document user-visible behavior, and add a regression test for fixes.

## Local checks

Run the same checks used by CI before opening a pull request:

~~~text
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo doc --no-deps
~~~

For storage or query-planner changes, also run:

~~~text
cargo bench --bench throughput
~~~

Please avoid committing generated files such as target/, local database
files, editor settings, or benchmark output.
