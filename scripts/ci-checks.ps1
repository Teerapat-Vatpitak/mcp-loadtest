# Source of truth for "did my change pass" on Windows. Same checks as ci-checks.sh.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

function Step($name) { Write-Host "`n→ $name" -ForegroundColor Cyan }

$testArtifactDir = Join-Path $PSScriptRoot "..\target\test-artifacts\ci"
$generatedJunit = Join-Path $PSScriptRoot "..\target\nextest\ci\junit.xml"
New-Item -ItemType Directory -Force -Path $testArtifactDir | Out-Null
@(
    $generatedJunit,
    (Join-Path $testArtifactDir "nextest.log"),
    (Join-Path $testArtifactDir "cargo-test.log"),
    (Join-Path $testArtifactDir "environment.txt"),
    (Join-Path $testArtifactDir "source-files.git-hash")
) | ForEach-Object {
    if (Test-Path -LiteralPath $_) {
        Remove-Item -LiteralPath $_ -Force
    }
}

$environment = @(
    "captured_at=$((Get-Date).ToUniversalTime().ToString('o'))",
    "head=$((& git rev-parse HEAD).Trim())",
    "profile=ci",
    "retries=0",
    "no_fail_fast=true",
    "os=$([System.Runtime.InteropServices.RuntimeInformation]::OSDescription)",
    "architecture=$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)",
    (& rustc --version),
    (& cargo --version),
    "git_status_begin",
    (& git status --short),
    "git_status_end"
)
if (Get-Command cargo-nextest -ErrorAction SilentlyContinue) {
    $environment += (& cargo nextest --version)
}
$environment |
    Set-Content -LiteralPath (Join-Path $testArtifactDir "environment.txt") -Encoding utf8

$sourceHashes = foreach ($sourceFile in (& git ls-files --cached --others --exclude-standard)) {
    if (Test-Path -LiteralPath $sourceFile -PathType Leaf) {
        "$((& git hash-object -- $sourceFile).Trim())`t$sourceFile"
    }
}
$sourceHashes |
    Set-Content -LiteralPath (Join-Path $testArtifactDir "source-files.git-hash") -Encoding utf8

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
    cargo nextest run --workspace --all-features --profile ci --no-fail-fast 2>&1 |
        Tee-Object -FilePath (Join-Path $testArtifactDir "nextest.log")
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if (-not (Test-Path -LiteralPath $generatedJunit -PathType Leaf)) {
        Write-Error "nextest passed but did not produce $generatedJunit"
        exit 74
    }
} else {
    cargo test --workspace --all-features --no-fail-fast 2>&1 |
        Tee-Object -FilePath (Join-Path $testArtifactDir "cargo-test.log")
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
