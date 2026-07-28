#!/usr/bin/env bash
# Source of truth for "did my change pass". Same script CI runs.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n→ %s\n' "$1"; }

test_artifact_dir="target/test-artifacts/ci"
generated_junit="target/nextest/ci/junit.xml"
mkdir -p "$test_artifact_dir"
rm -f -- \
    "$generated_junit" \
    "$test_artifact_dir/nextest.log" \
    "$test_artifact_dir/cargo-test.log" \
    "$test_artifact_dir/environment.txt" \
    "$test_artifact_dir/source-files.git-hash"

{
    printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'head=%s\n' "$(git rev-parse HEAD)"
    printf 'profile=ci\nretries=0\nno_fail_fast=true\n'
    printf 'os='
    uname -a
    rustc --version
    cargo --version
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest --version
    fi
    printf 'git_status_begin\n'
    git status --short
    printf 'git_status_end\n'
} >"$test_artifact_dir/environment.txt" 2>&1
while IFS= read -r -d '' source_file; do
    printf '%s\t%s\n' "$(git hash-object -- "$source_file")" "$source_file"
done < <(git ls-files --cached --others --exclude-standard -z) \
    >"$test_artifact_dir/source-files.git-hash"

step "rustfmt"
cargo fmt --all -- --check

step "clippy"
cargo clippy --workspace --all-targets --all-features -- -D warnings

step "build"
cargo build --workspace --all-features --locked

step "test"
if command -v cargo-nextest >/dev/null 2>&1; then
    cargo nextest run --workspace --all-features --profile ci --no-fail-fast \
        2>&1 | tee "$test_artifact_dir/nextest.log"
    if [ ! -f "$generated_junit" ]; then
        echo "nextest passed but did not produce $generated_junit" >&2
        exit 74
    fi
else
    cargo test --workspace --all-features --no-fail-fast \
        2>&1 | tee "$test_artifact_dir/cargo-test.log"
fi

step "doc"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Supply-chain check: scans dependency tree for known CVEs.
# Uses the RustSec advisories DB; configured in ../deny.toml.
# Optional locally — required in CI.
if command -v cargo-deny >/dev/null 2>&1; then
    step "deny (advisories)"
    cargo deny check advisories
else
    printf '\n(skipped: cargo-deny not installed locally)\n'
fi

printf '\n✅ all checks passed\n'
