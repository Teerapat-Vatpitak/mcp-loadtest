#!/usr/bin/env bash
# Re-run the complete suite without retries hiding intermittent failures.
# Usage: bash scripts/repeat-tests.sh [runs] [output-directory]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

runs="${1:-5}"
if ! [[ "$runs" =~ ^[1-9][0-9]*$ ]] || [ "$runs" -gt 100 ]; then
    echo "runs must be an integer from 1 through 100" >&2
    exit 2
fi
if ! command -v cargo-nextest >/dev/null 2>&1; then
    echo "cargo-nextest is required so every attempt can emit JUnit" >&2
    echo "install it with: cargo install cargo-nextest --locked" >&2
    exit 2
fi

if [ "$#" -ge 2 ]; then
    output_dir="$2"
else
    output_dir="$repo_root/target/test-artifacts/repeat-$(date -u +%Y%m%d-%H%M%S)-$$"
fi
if [ -e "$output_dir" ]; then
    if [ ! -d "$output_dir" ]; then
        echo "output path exists and is not a directory: $output_dir" >&2
        exit 2
    fi
    if find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
        echo "output directory must be empty: $output_dir" >&2
        exit 2
    fi
else
    mkdir -p "$output_dir"
fi
output_dir="$(cd "$output_dir" && pwd)"

artifact_root="$repo_root/target/test-artifacts"
lock_dir="$artifact_root/.repeat-tests.lock"
mkdir -p "$artifact_root"
if ! mkdir "$lock_dir" 2>/dev/null; then
    echo "another repeat suite owns $lock_dir; concurrent runs would mix JUnit evidence" >&2
    exit 75
fi
printf 'pid=%s\noutput=%s\n' "$$" "$output_dir" >"$lock_dir/owner"
cleanup_lock() {
    rm -f -- "$lock_dir/owner"
    rmdir "$lock_dir" 2>/dev/null || true
}
trap cleanup_lock EXIT INT TERM

summary="$output_dir/summary.tsv"
printf 'attempt\texit_code\tstarted_at\tfinished_at\tlog\tjunit\n' >"$summary"
failed_runs=0

# Preserve enough host/tool/source context to distinguish a product flake from
# runner contention. Git object hashes cover tracked and untracked source
# files without copying potentially sensitive contents into the artifact.
{
    printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'head=%s\n' "$(git rev-parse HEAD)"
    printf 'profile=stress\nretries=0\nno_fail_fast=true\n'
    printf 'os='
    uname -a
    rustc --version
    cargo --version
    cargo nextest --version
    if command -v python3 >/dev/null 2>&1; then
        python3 --version
    elif command -v python >/dev/null 2>&1; then
        python --version
    fi
    printf 'git_status_begin\n'
    git status --short
    printf 'git_status_end\n'
} >"$output_dir/environment.txt" 2>&1
while IFS= read -r -d '' source_file; do
    if [[ -f "$source_file" || -L "$source_file" ]]; then
        printf '%s\t%s\n' "$(git hash-object -- "$source_file")" "$source_file"
    fi
done < <(git ls-files --cached --others --exclude-standard -z) \
    >"$output_dir/source-files.git-hash"

export CARGO_TERM_COLOR=never
export NO_COLOR=1

for ((attempt = 1; attempt <= runs; attempt++)); do
    label="$(printf '%02d' "$attempt")"
    log_path="$output_dir/run-$label.log"
    junit_path="$output_dir/run-$label.junit.xml"
    generated_junit="$repo_root/target/nextest/stress/junit.xml"
    started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    # Never attach a previous attempt's report if this output directory is
    # reused or nextest exits before it can write a fresh JUnit file.
    rm -f -- "$generated_junit" "$junit_path"

    printf '\n=== repeat run %d/%d ===\n' "$attempt" "$runs"
    set +e
    cargo nextest run --workspace --all-features --profile stress --no-fail-fast \
        2>&1 | tee "$log_path"
    pipeline_status=("${PIPESTATUS[@]}")
    status="${pipeline_status[0]}"
    tee_status="${pipeline_status[1]}"
    set -e
    if [ "$tee_status" -ne 0 ]; then
        echo "tee failed while writing $log_path (exit $tee_status)" >&2
        # A green test run without retained evidence is not a successful
        # repeat attempt. Preserve cargo's failure code when it also failed.
        if [ "$status" -eq 0 ]; then
            status=74
        fi
    fi

    junit_name=""
    if [ -f "$generated_junit" ]; then
        if cp -- "$generated_junit" "$junit_path"; then
            junit_name="$(basename "$junit_path")"
        else
            echo "failed to retain JUnit as $junit_path" >&2
            if [ "$status" -eq 0 ]; then
                status=74
            fi
        fi
    else
        echo "nextest did not produce JUnit for attempt $attempt" >&2
        if [ "$status" -eq 0 ]; then
            status=74
        fi
    fi
    finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '%d\t%d\t%s\t%s\t%s\t%s\n' \
        "$attempt" "$status" "$started_at" "$finished_at" \
        "$(basename "$log_path")" "$junit_name" >>"$summary"
    if [ "$status" -ne 0 ]; then
        failed_runs=$((failed_runs + 1))
    fi
done

printf '\nArtifacts: %s\nRuns: %d; failed runs: %d\n' \
    "$output_dir" "$runs" "$failed_runs"
if [ "$failed_runs" -ne 0 ]; then
    {
        printf 'status=unresolved-transient-failure\n'
        printf 'failed_runs=%d\n' "$failed_runs"
        printf 'disposition=Release gate failed. Inspect each run log and JUnit, record a root cause, and fix it before re-running; do not discard this evidence set.\n'
    } >"$output_dir/DISPOSITION.txt"
    exit 1
fi
{
    printf 'status=no-transient-failure-observed\n'
    printf 'failed_runs=0\n'
    printf 'disposition=All %d no-retry stress attempts passed; no transient failure in this evidence set requires root-cause attribution.\n' "$runs"
} >"$output_dir/DISPOSITION.txt"
