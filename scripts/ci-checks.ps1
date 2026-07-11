# Source of truth for "did my change pass" on Windows. Same checks as ci-checks.sh.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

function Step($name) { Write-Host "`n→ $name" -ForegroundColor Cyan }

Step "rustfmt"
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Step "clippy"
cargo clippy --workspace --all-targets --all-features -- -D warnings
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Step "build"
cargo build --workspace --all-features --locked
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Step "test"
if (Get-Command cargo-nextest -ErrorAction SilentlyContinue) {
    cargo nextest run --workspace --all-features --no-fail-fast
} else {
    cargo test --workspace --all-features --no-fail-fast
}
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Step "doc"
$env:RUSTDOCFLAGS = "-D warnings"
cargo doc --workspace --no-deps --all-features
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Supply-chain check: scans dependency tree for known CVEs.
# Uses the RustSec advisories DB; configured in ../deny.toml.
# Optional locally — required in CI.
if (Get-Command cargo-deny -ErrorAction SilentlyContinue) {
    Step "deny (advisories)"
    cargo deny check advisories
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} else {
    Write-Host "`n(skipped: cargo-deny not installed locally)"
}

Write-Host "`n✅ all checks passed" -ForegroundColor Green
