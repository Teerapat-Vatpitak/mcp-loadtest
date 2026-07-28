$ErrorActionPreference = "Stop"

# Pin the exact official final specification revision and the latest reviewed
# conformance harness revision. The harness still labels this wire version as
# draft, so the claim remains limited to the explicitly executed scenarios.
$ConformanceRef = "49103de6ed70804e940637bf3e9e29e4a3f54e64"
$ConformanceSpecSource = "71e306956a4959c9655e5036be215d41986596e6"
$SpecRef = "5f5440bb26a62e2cf3440b92da5a667efa03b267"
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../../..")).Path
$SpecRepo = "https://github.com/modelcontextprotocol/modelcontextprotocol.git"
$ConformanceRepo = "https://github.com/modelcontextprotocol/conformance.git"
$TargetRoot = Join-Path $RepoRoot "target"
$Results = Join-Path $TargetRoot "official-conformance-results"
$LockDir = Join-Path $TargetRoot "official-conformance.lock"
$QuarantineRoot = Join-Path $TargetRoot "official-conformance-quarantine"
$ScopeManifest = Join-Path $RepoRoot "crates/protocol/tests/conformance-scope-2026-07-28.tsv"
$script:CurrentPhase = "setup"
$script:RunStartedAt = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
$script:ResultsReady = $false
$LockAcquired = $false
$LocationPushed = $false

function ConvertTo-TsvField {
    param([AllowNull()][string]$Value)

    if ($null -eq $Value) { return "" }
    return $Value.Replace("\", "\\").Replace("`t", "\t").Replace("`r", "\r").Replace("`n", "\n")
}

function Write-RunStatus {
    param(
        [Parameter(Mandatory = $true)][string]$Status,
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [string]$ErrorMessage = ""
    )

    if (-not $script:ResultsReady) { return }
    $Lines = @(
        "status=$Status",
        "started_at=$($script:RunStartedAt)"
    )
    if ($Status -ne "running") {
        $Lines += "finished_at=$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss.fffZ'))"
    }
    $Lines += "exit_code=$ExitCode"
    $Lines += "junit=not provided by upstream harness"
    if ($Status -eq "failed") {
        $Lines += "failed_phase=$($script:CurrentPhase)"
        $Lines += "error=$(ConvertTo-TsvField $ErrorMessage)"
    }
    $Lines | Set-Content -Encoding utf8 (Join-Path $Results "RUN_STATUS.txt")
}

function Invoke-NativePhase {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$LogStem,
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$ArgumentList,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [switch]$Quiet
    )

    $script:CurrentPhase = $Name
    $SafeStem = $LogStem -replace "[^A-Za-z0-9_.-]", "_"
    $StdoutPath = Join-Path $Results "$SafeStem.stdout.log"
    $StderrPath = Join-Path $Results "$SafeStem.stderr.log"
    $CombinedPath = Join-Path $Results "$SafeStem.log"
    $ArgvPath = Join-Path $Results "$SafeStem.argv.json"
    $StartedAt = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
    $Stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $ExitCode = 127
    $ResolvedExecutable = $FilePath
    $InvocationError = $null

    try {
        $Command = Get-Command $FilePath -ErrorAction Stop
        if ($Command.Path) {
            $ResolvedExecutable = $Command.Path
        }
        elseif ($Command.Source) {
            $ResolvedExecutable = $Command.Source
        }

        [ordered]@{
            working_directory = $WorkingDirectory
            executable = $ResolvedExecutable
            argv = @($ArgumentList)
        } |
            ConvertTo-Json -Depth 4 |
            Set-Content -Encoding utf8 $ArgvPath

        Push-Location $WorkingDirectory
        try {
            $global:LASTEXITCODE = 0
            & $ResolvedExecutable @ArgumentList 1> $StdoutPath 2> $StderrPath
            $ExitCode = [int]$LASTEXITCODE
        }
        finally {
            Pop-Location
        }
    }
    catch {
        $InvocationError = $_
        if (-not (Test-Path -LiteralPath $StdoutPath)) {
            New-Item -ItemType File -Path $StdoutPath | Out-Null
        }
        if (-not (Test-Path -LiteralPath $StderrPath)) {
            New-Item -ItemType File -Path $StderrPath | Out-Null
        }
        ($_ | Out-String) | Add-Content -Encoding utf8 $StderrPath
    }
    finally {
        $Stopwatch.Stop()
    }

    if (-not (Test-Path -LiteralPath $ArgvPath)) {
        [ordered]@{
            working_directory = $WorkingDirectory
            executable = $ResolvedExecutable
            argv = @($ArgumentList)
            resolution_error = if ($InvocationError) { $InvocationError.Exception.Message } else { $null }
        } |
            ConvertTo-Json -Depth 4 |
            Set-Content -Encoding utf8 $ArgvPath
    }

    $StdoutText = if (Test-Path -LiteralPath $StdoutPath) {
        [System.IO.File]::ReadAllText($StdoutPath)
    }
    else { "" }
    $StderrText = if (Test-Path -LiteralPath $StderrPath) {
        [System.IO.File]::ReadAllText($StderrPath)
    }
    else { "" }
    @(
        "=== stdout ===",
        $StdoutText,
        "=== stderr ===",
        $StderrText
    ) | Set-Content -Encoding utf8 $CombinedPath

    if (-not $Quiet) {
        if ($StdoutText) { [Console]::Out.Write($StdoutText) }
        if ($StderrText) { [Console]::Error.Write($StderrText) }
    }

    $FinishedAt = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
    $ArgvJson = [System.IO.File]::ReadAllText($ArgvPath).Trim()
    @(
        (ConvertTo-TsvField $Name),
        (ConvertTo-TsvField $StartedAt),
        (ConvertTo-TsvField $FinishedAt),
        [math]::Round($Stopwatch.Elapsed.TotalMilliseconds),
        $ExitCode,
        (ConvertTo-TsvField $ArgvJson)
    ) -join "`t" | Add-Content -Encoding utf8 (Join-Path $Results "PHASES.tsv")

    if ($ExitCode -ne 0) {
        throw "phase '$Name' failed with exit code $ExitCode; see $CombinedPath"
    }

    return [pscustomobject]@{
        ExitCode = $ExitCode
        Stdout = $StdoutText
        Stderr = $StderrText
        StdoutPath = $StdoutPath
        StderrPath = $StderrPath
        CombinedPath = $CombinedPath
        ArgvPath = $ArgvPath
    }
}

try {
    $script:CurrentPhase = "target-root-validation"
    $TargetItem = Get-Item -LiteralPath $TargetRoot -Force -ErrorAction SilentlyContinue
    if (-not $TargetItem) {
        New-Item -ItemType Directory -Path $TargetRoot -ErrorAction Stop | Out-Null
        $TargetItem = Get-Item -LiteralPath $TargetRoot -Force -ErrorAction Stop
    }
    if (
        -not $TargetItem.PSIsContainer -or
        ($TargetItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
    ) {
        throw "target must be a real directory, not a file, symlink, junction, or other reparse point: $TargetRoot"
    }

    # A directory creation is atomic and shared by the Bash and PowerShell
    # runners. Never auto-remove an existing lock: it may belong to a live run.
    try {
        New-Item -ItemType Directory -Path $LockDir -ErrorAction Stop | Out-Null
        $LockAcquired = $true
    }
    catch {
        throw "official conformance lock already exists; inspect and remove only after proving no runner is active: $LockDir"
    }
    @(
        "pid=$PID",
        "host=$([System.Environment]::MachineName)",
        "started_at=$($script:RunStartedAt)",
        "runner=powershell"
    ) | Set-Content -Encoding utf8 (Join-Path $LockDir "OWNER.txt")

    # Prevent a failed invocation from publishing artifacts left by an earlier
    # run. Preserve them recoverably under target with a collision-safe name.
    if (Test-Path -LiteralPath $Results) {
        New-Item -ItemType Directory -Force -Path $QuarantineRoot | Out-Null
        $QuarantineName = "{0}-{1}-{2}" -f `
            (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssfffZ"), `
            $PID, `
            ([guid]::NewGuid().ToString("N"))
        Move-Item -LiteralPath $Results -Destination (Join-Path $QuarantineRoot $QuarantineName)
    }
    New-Item -ItemType Directory -Path $Results | Out-Null
    $script:ResultsReady = $true
    "phase`tstarted_at`tfinished_at`tduration_ms`texit_code`targv_json" |
        Set-Content -Encoding utf8 (Join-Path $Results "PHASES.tsv")
    Write-RunStatus -Status "running" -ExitCode 0

    Push-Location $RepoRoot
    $LocationPushed = $true

    Invoke-NativePhase `
        -Name "adapter-build" `
        -LogStem "adapter-build" `
        -FilePath "cargo" `
        -ArgumentList @(
            "build",
            "--locked",
            "-p",
            "mcp-loadtest-protocol",
            "--example",
            "conformance_client"
        ) `
        -WorkingDirectory $RepoRoot | Out-Null

    $ExeSuffix = if ($IsWindows) { ".exe" } else { "" }
    $Adapter = (Resolve-Path (Join-Path $RepoRoot "target/debug/examples/conformance_client$ExeSuffix")).Path

    # Fetch and verify the separately reviewed specification revision. The
    # conformance package is pinned below; this preserves real spec-commit
    # evidence rather than recording an unchecked SHA.
    $SpecCheckout = Join-Path $RepoRoot "target/official-spec-ref"
    if (-not (Test-Path -LiteralPath (Join-Path $SpecCheckout ".git"))) {
        New-Item -ItemType Directory -Force -Path $SpecCheckout | Out-Null
        Invoke-NativePhase `
            -Name "spec-checkout-init" `
            -LogStem "spec-checkout-init" `
            -FilePath "git" `
            -ArgumentList @("-C", $SpecCheckout, "init", "--quiet") `
            -WorkingDirectory $RepoRoot | Out-Null
    }
    Invoke-NativePhase `
        -Name "spec-fetch-final" `
        -LogStem "spec-fetch-final" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $SpecCheckout,
            "fetch",
            "--quiet",
            "--depth=1",
            $SpecRepo,
            $SpecRef
        ) `
        -WorkingDirectory $RepoRoot | Out-Null
    $ResolvedSpecPhase = Invoke-NativePhase `
        -Name "spec-resolve-final" `
        -LogStem "spec-resolve-final" `
        -FilePath "git" `
        -ArgumentList @("-C", $SpecCheckout, "rev-parse", "FETCH_HEAD") `
        -WorkingDirectory $RepoRoot `
        -Quiet
    $ResolvedSpec = $ResolvedSpecPhase.Stdout.Trim()
    if ($ResolvedSpec -ne $SpecRef) {
        throw "pinned specification revision resolved to unexpected commit: $ResolvedSpec"
    }
    $SpecCommitPhase = Invoke-NativePhase `
        -Name "spec-commit-evidence" `
        -LogStem "spec-commit-evidence" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $SpecCheckout,
            "show",
            "-s",
            "--format=commit=%H%ncommitted_at=%cI%nsubject=%s",
            "FETCH_HEAD"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    Copy-Item -LiteralPath $SpecCommitPhase.StdoutPath -Destination (Join-Path $Results "SPEC_COMMIT.txt")

    # The final tag is now the contract: require exactly one immutable tag ref
    # and require it to resolve to the separately fetched final commit.
    $FinalTagPhase = Invoke-NativePhase `
        -Name "spec-resolve-final-tag" `
        -LogStem "spec-resolve-final-tag" `
        -FilePath "git" `
        -ArgumentList @("ls-remote", "--refs", $SpecRepo, "refs/tags/2026-07-28") `
        -WorkingDirectory $RepoRoot `
        -Quiet
    $ResolvedFinal = @(
        $FinalTagPhase.Stdout -split "`r?`n" |
            Where-Object { $_ -match "\srefs/tags/2026-07-28$" } |
            ForEach-Object { ($_ -split "\s+")[0] }
    )
    if ($ResolvedFinal.Count -ne 1 -or $ResolvedFinal[0] -ne $SpecRef) {
        throw "2026-07-28 final tag resolved unexpectedly: $($ResolvedFinal -join ',')"
    }

    # Reconcile the provisional harness's vendored draft schema against the
    # final dated schema. Fail closed unless the complete JSON-object delta is
    # confined to the reviewed subscriptions/listen change outside our scope.
    Invoke-NativePhase `
        -Name "spec-fetch-conformance-source" `
        -LogStem "spec-fetch-conformance-source" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $SpecCheckout,
            "fetch",
            "--quiet",
            "--depth=1",
            $SpecRepo,
            $ConformanceSpecSource
        ) `
        -WorkingDirectory $RepoRoot | Out-Null
    $ConformanceSourceCommitPhase = Invoke-NativePhase `
        -Name "spec-resolve-conformance-source" `
        -LogStem "spec-resolve-conformance-source" `
        -FilePath "git" `
        -ArgumentList @("-C", $SpecCheckout, "rev-parse", "FETCH_HEAD") `
        -WorkingDirectory $RepoRoot `
        -Quiet
    $ResolvedConformanceSourceCommit = $ConformanceSourceCommitPhase.Stdout.Trim()
    if ($ResolvedConformanceSourceCommit -ne $ConformanceSpecSource) {
        throw "conformance schema source resolved unexpectedly: $ResolvedConformanceSourceCommit"
    }

    $SourceSchema = Join-Path $Results "CONFORMANCE_VENDORED_DRAFT_SCHEMA.json"
    $FinalSchema = Join-Path $Results "FINAL_2026-07-28_SCHEMA.json"
    $SourceSchemaPhase = Invoke-NativePhase `
        -Name "retain-conformance-source-schema" `
        -LogStem "retain-conformance-source-schema" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $SpecCheckout,
            "show",
            "${ConformanceSpecSource}:schema/draft/schema.json"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    Copy-Item -LiteralPath $SourceSchemaPhase.StdoutPath -Destination $SourceSchema
    $FinalSchemaPhase = Invoke-NativePhase `
        -Name "retain-final-schema" `
        -LogStem "retain-final-schema" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $SpecCheckout,
            "show",
            "${SpecRef}:schema/2026-07-28/schema.json"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    Copy-Item -LiteralPath $FinalSchemaPhase.StdoutPath -Destination $FinalSchema

    $PythonCommand = Get-Command python3 -ErrorAction SilentlyContinue
    if (-not $PythonCommand) {
        $PythonCommand = Get-Command python -ErrorAction SilentlyContinue
    }
    if (-not $PythonCommand) {
        throw "Python is required for final schema reconciliation"
    }
    $ReconcilePython = @'
import copy
import json
import pathlib
import sys


source_path, final_path, report_path = map(pathlib.Path, sys.argv[1:])
source = json.loads(source_path.read_text(encoding="utf-8-sig"))
final = json.loads(final_path.read_text(encoding="utf-8-sig"))
source_defs = source.get("$defs")
final_defs = final.get("$defs")
errors = []
if not isinstance(source_defs, dict) or not isinstance(final_defs, dict):
    errors.append("both schemas must contain object-valued $defs")
    source_defs = source_defs if isinstance(source_defs, dict) else {}
    final_defs = final_defs if isinstance(final_defs, dict) else {}

removed = sorted(set(source_defs) - set(final_defs))
added = sorted(set(final_defs) - set(source_defs))
changed = sorted(
    key
    for key in set(source_defs) & set(final_defs)
    if source_defs[key] != final_defs[key]
)
expected_removed = ["SubscriptionsListenResultMeta"]
expected_added = [
    "SubscriptionsListenResultMetaObject",
    "SubscriptionsListenResultResponse",
]
expected_changed = ["SubscriptionsListenResult"]
if removed != expected_removed:
    errors.append(f"unexpected removed definitions: {removed!r}")
if added != expected_added:
    errors.append(f"unexpected added definitions: {added!r}")
if changed != expected_changed:
    errors.append(f"unexpected changed definitions: {changed!r}")

source_top = {key: value for key, value in source.items() if key != "$defs"}
final_top = {key: value for key, value in final.items() if key != "$defs"}
if source_top != final_top:
    errors.append("top-level schema fields outside $defs changed")

source_result = source_defs.get("SubscriptionsListenResult")
final_result = final_defs.get("SubscriptionsListenResult")
if not isinstance(source_result, dict) or not isinstance(final_result, dict):
    errors.append("SubscriptionsListenResult is missing or not an object")
else:
    old_ref = (
        source_result.get("properties", {})
        .get("_meta", {})
        .get("$ref")
    )
    new_ref = (
        final_result.get("properties", {})
        .get("_meta", {})
        .get("$ref")
    )
    if old_ref != "#/$defs/SubscriptionsListenResultMeta":
        errors.append(f"unexpected provisional subscriptions ref: {old_ref!r}")
    if new_ref != "#/$defs/SubscriptionsListenResultMetaObject":
        errors.append(f"unexpected final subscriptions ref: {new_ref!r}")
    normalized = copy.deepcopy(source_result)
    normalized["properties"]["_meta"]["$ref"] = (
        "#/$defs/SubscriptionsListenResultMetaObject"
    )
    if normalized != final_result:
        errors.append(
            "SubscriptionsListenResult changed beyond the reviewed $ref rename"
        )

status = "FAIL" if errors else "PASS"
lines = [
    f"status={status}",
    "source=modelcontextprotocol@71e306956a4959c9655e5036be215d41986596e6:schema/draft/schema.json",
    "final=modelcontextprotocol@5f5440bb26a62e2cf3440b92da5a667efa03b267:schema/2026-07-28/schema.json",
    f"removed_defs={','.join(removed)}",
    f"added_defs={','.join(added)}",
    f"changed_defs={','.join(changed)}",
    "top_level_outside_defs=unchanged" if source_top == final_top else "top_level_outside_defs=CHANGED",
    "implemented_surface_delta=none (subscriptions/listen is excluded from scope)",
]
lines.extend(f"error={error}" for error in errors)
report_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
if errors:
    raise SystemExit("final schema reconciliation failed: " + "; ".join(errors))
'@
    $ReconcileScript = Join-Path $Results "RECONCILE_SCHEMA.py"
    $ReconcilePython | Set-Content -Encoding utf8 $ReconcileScript
    Invoke-NativePhase `
        -Name "final-schema-reconciliation" `
        -LogStem "final-schema-reconciliation" `
        -FilePath $PythonCommand.Source `
        -ArgumentList @(
            $ReconcileScript,
            $SourceSchema,
            $FinalSchema,
            (Join-Path $Results "FINAL_SCHEMA_RECONCILIATION.txt")
        ) `
        -WorkingDirectory $RepoRoot | Out-Null

    # Verify and then execute the immutable conformance checkout. Running the
    # exact local dist entry point avoids npx reinstall/build churn per phase.
    $ConformanceCheckout = Join-Path $RepoRoot "target/official-conformance-ref"
    if (-not (Test-Path -LiteralPath (Join-Path $ConformanceCheckout ".git"))) {
        New-Item -ItemType Directory -Force -Path $ConformanceCheckout | Out-Null
        Invoke-NativePhase `
            -Name "conformance-checkout-init" `
            -LogStem "conformance-checkout-init" `
            -FilePath "git" `
            -ArgumentList @("-C", $ConformanceCheckout, "init", "--quiet") `
            -WorkingDirectory $RepoRoot | Out-Null
    }
    Invoke-NativePhase `
        -Name "conformance-fetch" `
        -LogStem "conformance-fetch" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $ConformanceCheckout,
            "fetch",
            "--quiet",
            "--depth=1",
            $ConformanceRepo,
            $ConformanceRef
        ) `
        -WorkingDirectory $RepoRoot | Out-Null
    $ResolvedConformancePhase = Invoke-NativePhase `
        -Name "conformance-resolve" `
        -LogStem "conformance-resolve" `
        -FilePath "git" `
        -ArgumentList @("-C", $ConformanceCheckout, "rev-parse", "FETCH_HEAD") `
        -WorkingDirectory $RepoRoot `
        -Quiet
    $ResolvedConformance = $ResolvedConformancePhase.Stdout.Trim()
    if ($ResolvedConformance -ne $ConformanceRef) {
        throw "pinned conformance revision resolved unexpectedly: $ResolvedConformance"
    }
    $LatestConformancePhase = Invoke-NativePhase `
        -Name "conformance-main-resolve" `
        -LogStem "conformance-main-resolve" `
        -FilePath "git" `
        -ArgumentList @(
            "ls-remote",
            "--refs",
            $ConformanceRepo,
            "refs/heads/main"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    $LatestConformanceFields = @(
        $LatestConformancePhase.Stdout.Trim() -split "\s+" |
            Where-Object { $_ }
    )
    $LatestConformance = if ($LatestConformanceFields.Count -gt 0) {
        $LatestConformanceFields[0]
    }
    else {
        ""
    }
    if ($LatestConformance -ne $ConformanceRef) {
        throw "reviewed conformance pin is no longer upstream main: pinned=$ConformanceRef main=$LatestConformance"
    }
    $ConformanceCommitPhase = Invoke-NativePhase `
        -Name "conformance-commit-evidence" `
        -LogStem "conformance-commit-evidence" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $ConformanceCheckout,
            "show",
            "-s",
            "--format=commit=%H%ncommitted_at=%cI%nsubject=%s",
            "FETCH_HEAD"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    Copy-Item -LiteralPath $ConformanceCommitPhase.StdoutPath -Destination (Join-Path $Results "CONFORMANCE_COMMIT.txt")
    $ConformanceSourcePhase = Invoke-NativePhase `
        -Name "conformance-vendored-source" `
        -LogStem "conformance-vendored-source" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $ConformanceCheckout,
            "show",
            "${ConformanceRef}:src/spec-types/SOURCE"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    $ResolvedConformanceSource = $ConformanceSourcePhase.Stdout.Trim() -replace "^modelcontextprotocol@", ""
    if ($ResolvedConformanceSource -ne $ConformanceSpecSource) {
        throw "pinned conformance spec source resolved unexpectedly: $ResolvedConformanceSource"
    }

    Invoke-NativePhase `
        -Name "conformance-checkout" `
        -LogStem "conformance-checkout" `
        -FilePath "git" `
        -ArgumentList @("-C", $ConformanceCheckout, "checkout", "--detach", $ConformanceRef) `
        -WorkingDirectory $RepoRoot | Out-Null
    $CheckedOutConformancePhase = Invoke-NativePhase `
        -Name "conformance-checkout-verify" `
        -LogStem "conformance-checkout-verify" `
        -FilePath "git" `
        -ArgumentList @("-C", $ConformanceCheckout, "rev-parse", "HEAD") `
        -WorkingDirectory $RepoRoot `
        -Quiet
    if ($CheckedOutConformancePhase.Stdout.Trim() -ne $ConformanceRef) {
        throw "checked-out conformance revision differs from pin"
    }
    $CheckoutStatusPhase = Invoke-NativePhase `
        -Name "conformance-checkout-clean" `
        -LogStem "conformance-checkout-clean" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $ConformanceCheckout,
            "status",
            "--porcelain=v1",
            "--untracked-files=no"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    if ($CheckoutStatusPhase.Stdout.Trim()) {
        throw "pinned conformance checkout contains tracked changes; refusing to build non-pinned source"
    }

    $NodeVersionPhase = Invoke-NativePhase `
        -Name "node-version" `
        -LogStem "node-version" `
        -FilePath "node" `
        -ArgumentList @("--version") `
        -WorkingDirectory $ConformanceCheckout `
        -Quiet
    $NpmVersionPhase = Invoke-NativePhase `
        -Name "npm-version" `
        -LogStem "npm-version" `
        -FilePath "npm" `
        -ArgumentList @("--version") `
        -WorkingDirectory $ConformanceCheckout `
        -Quiet
    $PackageLock = Join-Path $ConformanceCheckout "package-lock.json"
    if (-not (Test-Path -LiteralPath $PackageLock)) {
        throw "pinned conformance checkout has no package-lock.json"
    }
    @(
        "checkout=$ConformanceRef",
        "node=$($NodeVersionPhase.Stdout.Trim())",
        "npm=$($NpmVersionPhase.Stdout.Trim())",
        "package_lock_sha256=$((Get-FileHash -Algorithm SHA256 -LiteralPath $PackageLock).Hash.ToLowerInvariant())",
        "install=npm ci --no-audit --no-fund",
        "build=exactly once via package prepare script during npm ci",
        "entrypoint=node dist/index.js"
    ) | Set-Content -Encoding utf8 (Join-Path $Results "HARNESS_RUNTIME.txt")

    $HadCI = Test-Path Env:CI
    $OldCI = $env:CI
    $HadLefthook = Test-Path Env:LEFTHOOK
    $OldLefthook = $env:LEFTHOOK
    try {
        $env:CI = "true"
        $env:LEFTHOOK = "0"
        Invoke-NativePhase `
            -Name "conformance-install-build" `
            -LogStem "conformance-install-build" `
            -FilePath "npm" `
            -ArgumentList @("ci", "--no-audit", "--no-fund") `
            -WorkingDirectory $ConformanceCheckout | Out-Null
    }
    finally {
        if ($HadCI) { $env:CI = $OldCI } else { Remove-Item Env:CI -ErrorAction SilentlyContinue }
        if ($HadLefthook) { $env:LEFTHOOK = $OldLefthook } else { Remove-Item Env:LEFTHOOK -ErrorAction SilentlyContinue }
    }
    $ConformanceEntrypoint = Join-Path $ConformanceCheckout "dist/index.js"
    if (-not (Test-Path -LiteralPath $ConformanceEntrypoint -PathType Leaf)) {
        throw "npm ci completed but the pinned conformance dist/index.js was not built"
    }
    $PostBuildStatusPhase = Invoke-NativePhase `
        -Name "conformance-source-clean-after-build" `
        -LogStem "conformance-source-clean-after-build" `
        -FilePath "git" `
        -ArgumentList @(
            "-C",
            $ConformanceCheckout,
            "status",
            "--porcelain=v1",
            "--untracked-files=no"
        ) `
        -WorkingDirectory $RepoRoot `
        -Quiet
    if ($PostBuildStatusPhase.Stdout.Trim()) {
        throw "npm ci/build modified tracked conformance source; refusing non-reproducible harness"
    }

    @(
        "verified_at=$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))",
        "final_tag=2026-07-28",
        "final_spec_tag_commit=$($ResolvedFinal[0])",
        "tested_spec_snapshot=$SpecRef",
        "conformance_commit=$ResolvedConformance",
        "conformance_main_at_verification=$LatestConformance",
        "conformance_vendored_spec_source=$ResolvedConformanceSource",
        "conformance_status=latest official harness at verification; version still DRAFT/provisional",
        "claim=final-spec-reconciled subset; latest official scoped tools/discover, request-metadata, and request-header scenarios are unaffected by the final subscriptions-only schema delta",
        "excluded=full suite, auth, MRTR/request-state, subscriptions/listen, schema-reference, server, authorization-server"
    ) | Set-Content -Encoding utf8 (Join-Path $Results "UPSTREAM_STATUS.txt")

    @(
        "spec=$SpecRef",
        "conformance=$ConformanceRef",
        "conformance_main_at_verification=$LatestConformance",
        "conformance_vendored_spec_source=$ConformanceSpecSource",
        "conformance_status=latest official harness; version still DRAFT/provisional",
        "protocol=2026-07-28 final-spec-reconciled subset (scoped tools/discover, request-metadata, and request-header client scenarios only)"
    ) | Set-Content -Encoding utf8 (Join-Path $Results "PINNED_REFS.txt")

    $Scenarios = @(
        "request-metadata",
        "tools_call",
        "http-standard-headers",
        "http-custom-headers",
        "http-invalid-tool-headers"
    )

    # Retain the official client-scenario inventory and prove that the
    # reviewed scope manifest names every applicable scenario exactly once.
    $ListPhase = Invoke-NativePhase `
        -Name "official-client-scenario-list" `
        -LogStem "official-client-scenario-list" `
        -FilePath "node" `
        -ArgumentList @(
            $ConformanceEntrypoint,
            "list",
            "--client",
            "--spec-version",
            "2026-07-28"
        ) `
        -WorkingDirectory $ConformanceCheckout `
        -Quiet
    Copy-Item `
        -LiteralPath $ListPhase.StdoutPath `
        -Destination (Join-Path $Results "OFFICIAL_CLIENT_SCENARIOS.txt")
    Copy-Item -LiteralPath $ScopeManifest -Destination (Join-Path $Results "SCOPE.tsv")

    $OfficialScenarios = Get-Content (Join-Path $Results "OFFICIAL_CLIENT_SCENARIOS.txt") |
        ForEach-Object {
            if ($_ -match "^\s+- (.+?) \[") { $Matches[1] }
        } |
        Sort-Object
    $ScopeRows = Import-Csv (Join-Path $Results "SCOPE.tsv") -Delimiter "`t"
    $ScopedScenarios = @($ScopeRows.scenario | Sort-Object)
    if (Compare-Object $OfficialScenarios $ScopedScenarios) {
        throw "scope manifest does not exactly match the official 2026-07-28 client scenario inventory"
    }
    $ExecutedScenarios = @(
        $ScopeRows |
            Where-Object status -eq "executed" |
            ForEach-Object scenario |
            Sort-Object
    )
    $RequestedScenarios = @($Scenarios | Sort-Object)
    if (Compare-Object $RequestedScenarios $ExecutedScenarios) {
        throw "executed scenarios do not exactly match the reviewed scope manifest"
    }

    foreach ($Scenario in $Scenarios) {
        Invoke-NativePhase `
            -Name "official-scenario-$Scenario" `
            -LogStem $Scenario `
            -FilePath "node" `
            -ArgumentList @(
                $ConformanceEntrypoint,
                "client",
                "--command",
                "`"$Adapter`"",
                "--scenario",
                $Scenario,
                "--spec-version",
                "2026-07-28",
                "--timeout",
                "30000"
            ) `
            -WorkingDirectory $Results | Out-Null
    }

    $script:CurrentPhase = "complete"
    Write-RunStatus -Status "passed" -ExitCode 0
}
catch {
    Write-RunStatus -Status "failed" -ExitCode 1 -ErrorMessage $_.Exception.Message
    throw
}
finally {
    if ($LocationPushed) {
        Pop-Location
    }
    if ($LockAcquired) {
        try {
            Remove-Item -LiteralPath (Join-Path $LockDir "OWNER.txt") -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $LockDir -Force -ErrorAction Stop
        }
        catch {
            Write-Warning "could not release official conformance lock: $LockDir"
        }
    }
}
