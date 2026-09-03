#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/release-check.sh

Run the release quality gates, install the exact packaged crate into a
temporary prefix, and exercise the installed CLI and MCP server.
EOF
}

if [[ "$#" -ne 0 ]]; then
    usage
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-target}
cd "$repo_root"

if [[ -n "$(git status --porcelain)" ]]; then
    printf 'release check requires a clean worktree\n' >&2
    exit 1
fi

run_check() {
    printf '\n==> %s\n' "$*"
    "$@"
}

run_check git diff --check
run_check python3 scripts/public-repo-check.py
run_check python3 scripts/verify-registry-metadata.py
run_check cargo fmt --all -- --check
run_check cargo check --all-targets --locked
run_check cargo clippy --all-targets --all-features --locked -- -D warnings
run_check cargo test --all-targets --locked
run_check env RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps --locked
run_check cargo package --locked
run_check cargo publish --dry-run --locked
run_check cargo build --release --locked
run_check bash scripts/smoke-test.sh "$target_dir/release/basalt"
run_check python3 scripts/differential_sql.py --basalt "$target_dir/release/basalt"

temp_root=$(mktemp -d "${TMPDIR:-/tmp}/basalt-release-check.XXXXXX")
trap 'rm -rf -- "$temp_root"' EXIT

package_metadata=$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json, sys; p = json.load(sys.stdin)["packages"][0]; print(p["name"] + "\t" + str(p["version"]))')
IFS=$'\t' read -r package_name package_version <<<"$package_metadata"
package_file="$target_dir/package/${package_name}-${package_version}.crate"
package_dir="$temp_root/${package_name}-${package_version}"
install_root="$temp_root/install"
package_target="$temp_root/package-target"

if [[ ! -f "$package_file" ]]; then
    printf 'packaged crate was not found: %s\n' "$package_file" >&2
    exit 1
fi

run_check tar -xzf "$package_file" -C "$temp_root"
if [[ ! -d "$package_dir" ]]; then
    printf 'packaged crate extracted to an unexpected path: %s\n' "$package_dir" >&2
    exit 1
fi

run_check env CARGO_TARGET_DIR="$package_target" cargo install --locked --root "$install_root" --path "$package_dir"
installed_binary="$install_root/bin/basalt"
if [[ ! -x "$installed_binary" ]]; then
    printf 'packaged install did not produce an executable: %s\n' "$installed_binary" >&2
    exit 1
fi

run_check bash scripts/smoke-test.sh "$installed_binary"
run_check python3 scripts/benchmark_workspace.py \
    --basalt "$installed_binary" --rows 1000 --repeats 1 >"$temp_root/benchmark.json"

if command -v cargo-audit >/dev/null 2>&1; then
    run_check cargo audit
else
    printf '\n==> cargo audit (skipped: cargo-audit is not installed)\n'
fi

if command -v dist >/dev/null 2>&1; then
    run_check dist plan --output-format=json --no-local-paths >"$temp_root/dist-plan.json"
    host_target=$(rustc -vV | awk '$1 == "host:" { print $2 }')
    if [[ -z "$host_target" ]]; then
        printf 'could not determine the local Rust host target\n' >&2
        exit 1
    fi
    if [[ "$host_target" == *windows* ]]; then
        archive_name="basalt-db-${host_target}.zip"
    else
        archive_name="basalt-db-${host_target}.tar.xz"
    fi
    run_check dist build --target="$host_target" --artifacts=local --output-format=json \
        >"$temp_root/dist-build.json"
    configured_archive="$target_dir/distrib/$archive_name"
    checkout_archive="$repo_root/target/distrib/$archive_name"
    if [[ -f "$configured_archive" ]]; then
        release_archive="$configured_archive"
    elif [[ -f "$checkout_archive" ]]; then
        release_archive="$checkout_archive"
    else
        printf 'dist build did not produce the expected release archive: %s or %s\n' \
            "$configured_archive" "$checkout_archive" >&2
        exit 1
    fi
    run_check python3 scripts/verify-release-artifacts.py "$release_archive"
else
    printf '\n==> dist plan (skipped: dist is not installed)\n'
fi

printf '\nRelease checks passed; the packaged crate and installed binary passed smoke tests.\n'
