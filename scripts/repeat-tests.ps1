# Re-run the complete suite without retries hiding intermittent failures.
# Every attempt keeps an independent console log and JUnit report.
[CmdletBinding()]
param(
    [ValidateRange(1, 100)]
    [int]$Runs = 5,

    [string]$OutputDir = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $repoRoot

if (-not (Get-Command cargo-nextest -ErrorAction SilentlyContinue)) {
    throw "cargo-nextest is required so every attempt can emit JUnit. Install it with: cargo install cargo-nextest --locked"
}

if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss-fff"
    $OutputDir = Join-Path $repoRoot "target/test-artifacts/repeat-$stamp-$PID"
} elseif (-not [IO.Path]::IsPathRooted($OutputDir)) {
    $OutputDir = Join-Path $repoRoot $OutputDir
}

$OutputDir = [IO.Path]::GetFullPath($OutputDir)
if (Test-Path -LiteralPath $OutputDir) {
    if (-not (Test-Path -LiteralPath $OutputDir -PathType Container)) {
        throw "output path exists and is not a directory: $OutputDir"
    }
    if (Get-ChildItem -LiteralPath $OutputDir -Force | Select-Object -First 1) {
        throw "output directory must be empty: $OutputDir"
    }
} else {
    New-Item -ItemType Directory -Path $OutputDir | Out-Null
}

$artifactRoot = Join-Path $repoRoot "target/test-artifacts"
New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
$lockPath = Join-Path $artifactRoot ".repeat-tests.lock"
try {
    $lockStream = [IO.File]::Open(
        $lockPath,
        [IO.FileMode]::OpenOrCreate,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
} catch {
    throw "another repeat suite owns $lockPath; concurrent runs would mix JUnit evidence"
}

$oldCargoColor = $env:CARGO_TERM_COLOR
$oldNoColor = $env:NO_COLOR
$hadNativePreference = Test-Path Variable:\PSNativeCommandUseErrorActionPreference
if ($hadNativePreference) {
    $oldNativePreference = $PSNativeCommandUseErrorActionPreference
}
$env:CARGO_TERM_COLOR = "never"
$env:NO_COLOR = "1"
$PSNativeCommandUseErrorActionPreference = $false

$results = @()
try {
# Preserve enough host/tool/source context to distinguish a product flake from
# runner contention. Git object hashes cover tracked and untracked source
# files without copying potentially sensitive contents into the artifact.
$environment = @(
    "captured_at=$((Get-Date).ToUniversalTime().ToString('o'))",
    "head=$((& git rev-parse HEAD).Trim())",
    "profile=stress",
    "retries=0",
    "no_fail_fast=true",
    "os=$([System.Runtime.InteropServices.RuntimeInformation]::OSDescription)",
    "architecture=$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)",
    (& rustc --version),
    (& cargo --version),
    (& cargo nextest --version),
    "git_status_begin",
    (& git status --short),
    "git_status_end"
)
if (Get-Command python -ErrorAction SilentlyContinue) {
    $environment += (& python --version 2>&1)
} elseif (Get-Command python3 -ErrorAction SilentlyContinue) {
    $environment += (& python3 --version 2>&1)
}
$environment | Set-Content -LiteralPath (Join-Path $OutputDir "environment.txt") -Encoding utf8

$sourceHashes = foreach ($sourceFile in (& git ls-files --cached --others --exclude-standard)) {
    "$((& git hash-object -- $sourceFile).Trim())`t$sourceFile"
}
$sourceHashes |
    Set-Content -LiteralPath (Join-Path $OutputDir "source-files.git-hash") -Encoding utf8

    for ($attempt = 1; $attempt -le $Runs; $attempt++) {
        $label = "{0:D2}" -f $attempt
        $logPath = Join-Path $OutputDir "run-$label.log"
        $junitPath = Join-Path $OutputDir "run-$label.junit.xml"
        $generatedJunit = Join-Path $repoRoot "target/nextest/stress/junit.xml"
        $startedAt = (Get-Date).ToUniversalTime()

        # Never attach a previous attempt's report if this output directory is
        # reused or nextest exits before it can write a fresh JUnit file.
        if (Test-Path -LiteralPath $generatedJunit) {
            Remove-Item -LiteralPath $generatedJunit -Force
        }
        if (Test-Path -LiteralPath $junitPath) {
            Remove-Item -LiteralPath $junitPath -Force
        }

        Write-Host "`n=== repeat run $attempt/$Runs ===" -ForegroundColor Cyan
        try {
            & cargo nextest run --workspace --all-features --profile stress --no-fail-fast 2>&1 |
                Tee-Object -FilePath $logPath
            $status = $LASTEXITCODE
        } catch {
            Write-Warning "failed to execute or retain repeat run ${attempt}: $_"
            $status = 74
        }

        if (Test-Path -LiteralPath $generatedJunit) {
            try {
                Copy-Item -LiteralPath $generatedJunit -Destination $junitPath -Force
            } catch {
                Write-Warning "failed to retain JUnit as ${junitPath}: $_"
                if ($status -eq 0) {
                    $status = 74
                }
            }
        } else {
            Write-Warning "nextest did not produce JUnit for attempt $attempt"
            if ($status -eq 0) {
                $status = 74
            }
        }

        $finishedAt = (Get-Date).ToUniversalTime()
        $results += [pscustomobject]@{
            attempt = $attempt
            exit_code = $status
            started_at = $startedAt.ToString("o")
            finished_at = $finishedAt.ToString("o")
            duration_seconds = [Math]::Round(($finishedAt - $startedAt).TotalSeconds, 3)
            log = [IO.Path]::GetFileName($logPath)
            junit = if (Test-Path -LiteralPath $junitPath) {
                [IO.Path]::GetFileName($junitPath)
            } else {
                $null
            }
        }
    }
} finally {
    $env:CARGO_TERM_COLOR = $oldCargoColor
    $env:NO_COLOR = $oldNoColor
    if ($hadNativePreference) {
        $PSNativeCommandUseErrorActionPreference = $oldNativePreference
    } else {
        Remove-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue
    }
    $lockStream.Dispose()
    try {
        Remove-Item -LiteralPath $lockPath -Force -ErrorAction Stop
    } catch {
        # A new PowerShell invocation may have acquired the same file between
        # Dispose and Remove-Item. In that case Windows denies deletion and
        # the new owner will remove it when its own run ends.
        Write-Warning "could not remove repeat-suite lock ${lockPath}: $_"
    }
}

$summaryPath = Join-Path $OutputDir "summary.json"
$results | ConvertTo-Json | Set-Content -LiteralPath $summaryPath -Encoding utf8

$failedRuns = @($results | Where-Object { $_.exit_code -ne 0 })
Write-Host "`nArtifacts: $OutputDir"
Write-Host "Runs: $Runs; failed runs: $($failedRuns.Count)"

if ($failedRuns.Count -gt 0) {
    @(
        "status=unresolved-transient-failure",
        "failed_runs=$($failedRuns.Count)",
        "failed_attempts=$((@($failedRuns.attempt) -join ','))",
        "disposition=Release gate failed. Inspect each run log and JUnit, record a root cause, and fix it before re-running; do not discard this evidence set."
    ) | Set-Content -LiteralPath (Join-Path $OutputDir "DISPOSITION.txt") -Encoding utf8
    exit 1
}
@(
    "status=no-transient-failure-observed",
    "failed_runs=0",
    "disposition=All $Runs no-retry stress attempts passed; no transient failure in this evidence set requires root-cause attribution."
) | Set-Content -LiteralPath (Join-Path $OutputDir "DISPOSITION.txt") -Encoding utf8
