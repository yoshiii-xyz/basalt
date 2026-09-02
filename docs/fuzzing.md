# Fuzzing

Basalt keeps fuzz targets outside the normal Cargo workspace so contributors
do not need libFuzzer to build, test, or package the CLI. The targets exercise
two untrusted-input boundaries:

- `sql_parser` feeds arbitrary UTF-8 strings to the SQL lexer/parser.
- `snapshot` feeds arbitrary bytes to the checksummed snapshot and state
  decoder without touching the filesystem.

The snapshot decoder rejects inputs larger than 256 MiB before parsing. The
workspace import path has its own 64 MiB input limit.

## Run locally

Install the runner once:

```bash
cargo install cargo-fuzz --locked
```

Run bounded smoke campaigns from the repository root. The default address
sanitizer build requires a nightly toolchain; `--sanitizer none` is the stable
toolchain fallback:

```bash
cargo fuzz run --sanitizer none sql_parser -- -max_total_time=60
cargo fuzz run --sanitizer none snapshot -- -max_total_time=60
```

Use longer runs and a saved corpus when investigating a failure. Keep reduced
regression inputs in the target's corpus directory only when they are small,
reproducible, and relevant to the boundary; do not commit generated crash
artifacts without a regression test explaining the failure.

Fuzzing is an additional signal. It does not replace deterministic parser,
storage, crash-recovery, compatibility, or integration tests.
