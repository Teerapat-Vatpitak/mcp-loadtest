#!/usr/bin/env bash
# Source of truth for "did my change pass". Same script CI runs.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n→ %s\n' "$1"; }

step "rustfmt"
cargo fmt --all -- --check

step "clippy"
cargo clippy --workspace --all-targets --all-features -- -D warnings

step "build"
cargo build --workspace --all-features --locked

step "test"
if command -v cargo-nextest >/dev/null 2>&1; then
    cargo nextest run --workspace --all-features --no-fail-fast
else
    cargo test --workspace --all-features --no-fail-fast
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
