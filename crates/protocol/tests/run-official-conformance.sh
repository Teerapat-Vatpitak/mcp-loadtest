#!/usr/bin/env bash
set -euo pipefail

# Pin the exact official final specification revision and the latest reviewed
# conformance harness revision. The harness still labels this wire version as
# draft, so the claim remains limited to the explicitly executed scenarios.
conformance_ref="49103de6ed70804e940637bf3e9e29e4a3f54e64"
conformance_spec_source="71e306956a4959c9655e5036be215d41986596e6"
spec_ref="5f5440bb26a62e2cf3440b92da5a667efa03b267"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
spec_repo="https://github.com/modelcontextprotocol/modelcontextprotocol.git"
conformance_repo="https://github.com/modelcontextprotocol/conformance.git"
target_root="$repo_root/target"
results="$target_root/official-conformance-results"
lock_dir="$target_root/official-conformance.lock"
quarantine_root="$target_root/official-conformance-quarantine"
scope_manifest="$repo_root/crates/protocol/tests/conformance-scope-2026-07-28.tsv"
current_phase="setup"
run_started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
results_ready=0
lock_acquired=0
run_completed=0
failure_reason=""

sanitize_tsv() {
  local value="${1-}"
  value="${value//\\/\\\\}"
  value="${value//$'\t'/\\t}"
  value="${value//$'\r'/\\r}"
  value="${value//$'\n'/\\n}"
  printf '%s' "$value"
}

write_run_status() {
  local status="$1"
  local exit_code="$2"
  local error_message="${3-}"

  if [ "$results_ready" -ne 1 ]; then
    return
  fi
  {
    printf 'status=%s\n' "$status"
    printf 'started_at=%s\n' "$run_started_at"
    if [ "$status" != "running" ]; then
      printf 'finished_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    fi
    printf 'exit_code=%s\n' "$exit_code"
    printf 'junit=not provided by upstream harness\n'
    if [ "$status" = "failed" ]; then
      printf 'failed_phase=%s\n' "$(sanitize_tsv "$current_phase")"
      printf 'error=%s\n' "$(sanitize_tsv "$error_message")"
    fi
  } >"$results/RUN_STATUS.txt"
}

finish() {
  local exit_code=$?
  trap - EXIT INT TERM
  set +e

  if [ "$run_completed" -eq 1 ]; then
    exit_code=0
    write_run_status "passed" "$exit_code"
  else
    if [ "$exit_code" -eq 0 ]; then
      exit_code=1
    fi
    write_run_status "failed" "$exit_code" "$failure_reason"
  fi

  if [ "$lock_acquired" -eq 1 ]; then
    rm -f "$lock_dir/OWNER.txt"
    if ! rmdir "$lock_dir"; then
      printf 'warning: could not release official conformance lock: %s\n' \
        "$lock_dir" >&2
    fi
  fi
  exit "$exit_code"
}

trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
  failure_reason="$*"
  printf 'error: %s\n' "$failure_reason" >&2
  return 1
}

python_cmd="$(command -v python3 || true)"
if [ -z "$python_cmd" ]; then
  fail "python3 is required for target validation, phase timing, and final schema reconciliation"
fi

# On Windows a junction/reparse point is not reliably exposed by Bash's -L,
# so use the already-required Python runtime to validate the path with lstat.
current_phase="target-root-validation"
target_validation_message=""
if ! target_validation_message="$(
  "$python_cmd" - "$target_root" 2>&1 <<'PY'
import pathlib
import stat
import sys


target = pathlib.Path(sys.argv[1])
if not target.exists() and not target.is_symlink():
    target.mkdir()
info = target.lstat()
reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
file_attributes = getattr(info, "st_file_attributes", 0)
if (
    not stat.S_ISDIR(info.st_mode)
    or stat.S_ISLNK(info.st_mode)
    or file_attributes & reparse_flag
):
    raise SystemExit(
        "target must be a real directory, not a file, symlink, junction, "
        f"or other reparse point: {target}"
    )
PY
)"; then
  target_validation_message="$(
    printf '%s' "$target_validation_message" |
      tr '\r\n' '  '
  )"
  fail "target validation failed: $target_validation_message"
fi

if ! mkdir "$lock_dir" 2>/dev/null; then
  fail \
    "official conformance lock already exists; inspect and remove only after proving no runner is active: $lock_dir"
fi
lock_acquired=1
{
  printf 'pid=%s\n' "$$"
  printf 'host=%s\n' "$(hostname)"
  printf 'started_at=%s\n' "$run_started_at"
  printf 'runner=bash\n'
} >"$lock_dir/OWNER.txt"

# Prevent a failed invocation from publishing artifacts left by an earlier
# run. Preserve them recoverably under target with a collision-safe name.
if [ -e "$results" ] || [ -L "$results" ]; then
  mkdir -p "$quarantine_root"
  quarantine_base="$(
    printf '%s-%s' "$(date -u +%Y%m%dT%H%M%SZ)" "$$"
  )"
  quarantine_destination="$quarantine_root/$quarantine_base"
  quarantine_counter=0
  while [ -e "$quarantine_destination" ] || [ -L "$quarantine_destination" ]; do
    quarantine_counter=$((quarantine_counter + 1))
    quarantine_destination="$quarantine_root/$quarantine_base-$quarantine_counter"
  done
  mv "$results" "$quarantine_destination"
fi
mkdir "$results"
results_ready=1
printf 'phase\tstarted_at\tfinished_at\tduration_ms\texit_code\targv_escaped\n' \
  >"$results/PHASES.tsv"
write_run_status "running" 0

now_epoch_ms() {
  "$python_cmd" -c \
    'import time; print(time.time_ns() // 1_000_000)'
}

run_phase() {
  local name="$1"
  local stem="$2"
  local working_directory="$3"
  local replay="$4"
  shift 4
  local command_name="$1"
  shift
  local -a arguments=("$@")
  local safe_stem="${stem//[^A-Za-z0-9_.-]/_}"
  local stdout_path="$results/$safe_stem.stdout.log"
  local stderr_path="$results/$safe_stem.stderr.log"
  local combined_path="$results/$safe_stem.log"
  local argv_path="$results/$safe_stem.argv.txt"
  local started_at
  local finished_at
  local started_ms
  local finished_ms
  local duration_ms
  local resolved_executable=""
  local exit_code=127
  local argv_record

  current_phase="$name"
  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  started_ms="$(now_epoch_ms)"

  if [[ "$command_name" == */* ]]; then
    if [ -x "$command_name" ] || [ -f "$command_name" ]; then
      resolved_executable="$command_name"
    fi
  else
    resolved_executable="$(command -v "$command_name" 2>/dev/null || true)"
  fi

  {
    printf 'working_directory=%q\n' "$working_directory"
    printf 'argv='
    if [ -n "$resolved_executable" ]; then
      printf '%q ' "$resolved_executable" "${arguments[@]}"
    else
      printf '%q ' "$command_name" "${arguments[@]}"
    fi
    printf '\n'
  } >"$argv_path"

  if [ -z "$resolved_executable" ]; then
    : >"$stdout_path"
    printf 'executable not found: %s\n' "$command_name" >"$stderr_path"
  elif (
    cd "$working_directory" &&
      "$resolved_executable" "${arguments[@]}"
  ) >"$stdout_path" 2>"$stderr_path"; then
    exit_code=0
  else
    exit_code=$?
  fi

  finished_ms="$(now_epoch_ms)"
  finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  duration_ms=$((finished_ms - started_ms))
  {
    printf '%s\n' '=== stdout ==='
    cat "$stdout_path"
    printf '%s\n' '=== stderr ==='
    cat "$stderr_path"
  } >"$combined_path"

  if [ "$replay" = "replay" ] || [ "$exit_code" -ne 0 ]; then
    cat "$stdout_path"
    cat "$stderr_path" >&2
  fi

  argv_record="$(tr '\r\n' '  ' <"$argv_path")"
  {
    sanitize_tsv "$name"
    printf '\t'
    sanitize_tsv "$started_at"
    printf '\t'
    sanitize_tsv "$finished_at"
    printf '\t%s\t%s\t' "$duration_ms" "$exit_code"
    sanitize_tsv "$argv_record"
    printf '\n'
  } >>"$results/PHASES.tsv"

  if [ "$exit_code" -ne 0 ]; then
    failure_reason="phase '$name' failed with exit code $exit_code; see $combined_path"
    return "$exit_code"
  fi
}

cd "$repo_root"
run_phase \
  "adapter-build" \
  "adapter-build" \
  "$repo_root" \
  "replay" \
  cargo build --locked -p mcp-loadtest-protocol --example conformance_client
adapter="$repo_root/target/debug/examples/conformance_client"

# Fetch and verify the separately reviewed specification revision. Retaining
# its identity as evidence is stronger than recording an unchecked SHA.
spec_checkout="$repo_root/target/official-spec-ref"
if [ ! -d "$spec_checkout/.git" ]; then
  mkdir -p "$spec_checkout"
  run_phase \
    "spec-checkout-init" \
    "spec-checkout-init" \
    "$repo_root" \
    "replay" \
    git -C "$spec_checkout" init --quiet
fi
run_phase \
  "spec-fetch-final" \
  "spec-fetch-final" \
  "$repo_root" \
  "replay" \
  git -C "$spec_checkout" fetch --quiet --depth=1 \
  "$spec_repo" "$spec_ref"
run_phase \
  "spec-resolve-final" \
  "spec-resolve-final" \
  "$repo_root" \
  "quiet" \
  git -C "$spec_checkout" rev-parse FETCH_HEAD
resolved_spec="$(tr -d '\r\n' <"$results/spec-resolve-final.stdout.log")"
if [ "$resolved_spec" != "$spec_ref" ]; then
  fail "pinned specification revision resolved to unexpected commit: $resolved_spec"
fi
run_phase \
  "spec-commit-evidence" \
  "spec-commit-evidence" \
  "$repo_root" \
  "quiet" \
  git -C "$spec_checkout" show -s \
  --format='commit=%H%ncommitted_at=%cI%nsubject=%s' FETCH_HEAD
cp "$results/spec-commit-evidence.stdout.log" "$results/SPEC_COMMIT.txt"

# The final tag is the contract: require exactly one immutable tag ref and
# require it to resolve to the separately fetched final commit.
run_phase \
  "spec-resolve-final-tag" \
  "spec-resolve-final-tag" \
  "$repo_root" \
  "quiet" \
  git ls-remote --refs "$spec_repo" refs/tags/2026-07-28
resolved_final="$(
  awk \
    '$2 == "refs/tags/2026-07-28" { print $1 }' \
    "$results/spec-resolve-final-tag.stdout.log"
)"
resolved_final_count="$(
  printf '%s\n' "$resolved_final" |
    sed '/^$/d' |
    wc -l |
    tr -d ' '
)"
if [ "$resolved_final_count" -ne 1 ] || [ "$resolved_final" != "$spec_ref" ]; then
  fail "2026-07-28 final tag resolved unexpectedly: $resolved_final"
fi

# Reconcile the provisional harness's vendored draft schema against the final
# dated schema. Fail closed unless the complete JSON-object delta is confined
# to the reviewed subscriptions/listen change outside our scope.
run_phase \
  "spec-fetch-conformance-source" \
  "spec-fetch-conformance-source" \
  "$repo_root" \
  "replay" \
  git -C "$spec_checkout" fetch --quiet --depth=1 \
  "$spec_repo" "$conformance_spec_source"
run_phase \
  "spec-resolve-conformance-source" \
  "spec-resolve-conformance-source" \
  "$repo_root" \
  "quiet" \
  git -C "$spec_checkout" rev-parse FETCH_HEAD
resolved_conformance_source_commit="$(
  tr -d '\r\n' <"$results/spec-resolve-conformance-source.stdout.log"
)"
if [ "$resolved_conformance_source_commit" != "$conformance_spec_source" ]; then
  fail \
    "conformance schema source resolved unexpectedly: $resolved_conformance_source_commit"
fi
run_phase \
  "retain-conformance-source-schema" \
  "retain-conformance-source-schema" \
  "$repo_root" \
  "quiet" \
  git -C "$spec_checkout" show \
  "${conformance_spec_source}:schema/draft/schema.json"
source_schema="$results/CONFORMANCE_VENDORED_DRAFT_SCHEMA.json"
cp "$results/retain-conformance-source-schema.stdout.log" "$source_schema"
run_phase \
  "retain-final-schema" \
  "retain-final-schema" \
  "$repo_root" \
  "quiet" \
  git -C "$spec_checkout" show \
  "${spec_ref}:schema/2026-07-28/schema.json"
final_schema="$results/FINAL_2026-07-28_SCHEMA.json"
cp "$results/retain-final-schema.stdout.log" "$final_schema"

reconcile_script="$results/RECONCILE_SCHEMA.py"
cat >"$reconcile_script" <<'PY'
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
PY
run_phase \
  "final-schema-reconciliation" \
  "final-schema-reconciliation" \
  "$repo_root" \
  "replay" \
  "$python_cmd" \
  "$reconcile_script" \
  "$source_schema" \
  "$final_schema" \
  "$results/FINAL_SCHEMA_RECONCILIATION.txt"

# Verify and execute the immutable conformance checkout. Running its exact
# local dist entry point avoids npx reinstall/build churn for every phase.
conformance_checkout="$repo_root/target/official-conformance-ref"
if [ ! -d "$conformance_checkout/.git" ]; then
  mkdir -p "$conformance_checkout"
  run_phase \
    "conformance-checkout-init" \
    "conformance-checkout-init" \
    "$repo_root" \
    "replay" \
    git -C "$conformance_checkout" init --quiet
fi
run_phase \
  "conformance-fetch" \
  "conformance-fetch" \
  "$repo_root" \
  "replay" \
  git -C "$conformance_checkout" fetch --quiet --depth=1 \
  "$conformance_repo" "$conformance_ref"
run_phase \
  "conformance-resolve" \
  "conformance-resolve" \
  "$repo_root" \
  "quiet" \
  git -C "$conformance_checkout" rev-parse FETCH_HEAD
resolved_conformance="$(
  tr -d '\r\n' <"$results/conformance-resolve.stdout.log"
)"
if [ "$resolved_conformance" != "$conformance_ref" ]; then
  fail \
    "pinned conformance revision resolved unexpectedly: $resolved_conformance"
fi
run_phase \
  "conformance-main-resolve" \
  "conformance-main-resolve" \
  "$repo_root" \
  "quiet" \
  git ls-remote --refs "$conformance_repo" refs/heads/main
latest_conformance="$(
  awk 'NR == 1 { print $1 }' \
    "$results/conformance-main-resolve.stdout.log" |
    tr -d '\r\n'
)"
if [ "$latest_conformance" != "$conformance_ref" ]; then
  fail \
    "reviewed conformance pin is no longer upstream main: pinned=$conformance_ref main=$latest_conformance"
fi
run_phase \
  "conformance-commit-evidence" \
  "conformance-commit-evidence" \
  "$repo_root" \
  "quiet" \
  git -C "$conformance_checkout" show -s \
  --format='commit=%H%ncommitted_at=%cI%nsubject=%s' FETCH_HEAD
cp \
  "$results/conformance-commit-evidence.stdout.log" \
  "$results/CONFORMANCE_COMMIT.txt"
run_phase \
  "conformance-vendored-source" \
  "conformance-vendored-source" \
  "$repo_root" \
  "quiet" \
  git -C "$conformance_checkout" show \
  "${conformance_ref}:src/spec-types/SOURCE"
resolved_conformance_source="$(
  tr -d '\r\n' <"$results/conformance-vendored-source.stdout.log" |
    sed 's/^modelcontextprotocol@//'
)"
if [ "$resolved_conformance_source" != "$conformance_spec_source" ]; then
  fail \
    "pinned conformance spec source resolved unexpectedly: $resolved_conformance_source"
fi
run_phase \
  "conformance-checkout" \
  "conformance-checkout" \
  "$repo_root" \
  "replay" \
  git -C "$conformance_checkout" checkout --detach "$conformance_ref"
run_phase \
  "conformance-checkout-verify" \
  "conformance-checkout-verify" \
  "$repo_root" \
  "quiet" \
  git -C "$conformance_checkout" rev-parse HEAD
checked_out_conformance="$(
  tr -d '\r\n' <"$results/conformance-checkout-verify.stdout.log"
)"
if [ "$checked_out_conformance" != "$conformance_ref" ]; then
  fail "checked-out conformance revision differs from pin"
fi
run_phase \
  "conformance-checkout-clean" \
  "conformance-checkout-clean" \
  "$repo_root" \
  "quiet" \
  git -C "$conformance_checkout" status \
  --porcelain=v1 \
  --untracked-files=no
if [ -n "$(tr -d '\r\n' <"$results/conformance-checkout-clean.stdout.log")" ]; then
  fail \
    "pinned conformance checkout contains tracked changes; refusing to build non-pinned source"
fi

run_phase \
  "node-version" \
  "node-version" \
  "$conformance_checkout" \
  "quiet" \
  node --version
run_phase \
  "npm-version" \
  "npm-version" \
  "$conformance_checkout" \
  "quiet" \
  npm --version
package_lock="$conformance_checkout/package-lock.json"
if [ ! -f "$package_lock" ]; then
  fail "pinned conformance checkout has no package-lock.json"
fi
package_lock_sha256="$(
  "$python_cmd" -c \
    'import hashlib, pathlib, sys; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())' \
    "$package_lock"
)"
{
  printf 'checkout=%s\n' "$conformance_ref"
  printf 'node=%s\n' \
    "$(tr -d '\r\n' <"$results/node-version.stdout.log")"
  printf 'npm=%s\n' \
    "$(tr -d '\r\n' <"$results/npm-version.stdout.log")"
  printf 'package_lock_sha256=%s\n' "$package_lock_sha256"
  printf 'install=npm ci --no-audit --no-fund\n'
  printf 'build=exactly once via package prepare script during npm ci\n'
  printf 'entrypoint=node dist/index.js\n'
} >"$results/HARNESS_RUNTIME.txt"

ci_was_set="${CI+x}"
ci_previous="${CI-}"
lefthook_was_set="${LEFTHOOK+x}"
lefthook_previous="${LEFTHOOK-}"
export CI=true
export LEFTHOOK=0
npm_exit=0
run_phase \
  "conformance-install-build" \
  "conformance-install-build" \
  "$conformance_checkout" \
  "replay" \
  npm ci --no-audit --no-fund || npm_exit=$?
if [ -n "$ci_was_set" ]; then
  export CI="$ci_previous"
else
  unset CI
fi
if [ -n "$lefthook_was_set" ]; then
  export LEFTHOOK="$lefthook_previous"
else
  unset LEFTHOOK
fi
if [ "$npm_exit" -ne 0 ]; then
  exit "$npm_exit"
fi
conformance_entrypoint="$conformance_checkout/dist/index.js"
if [ ! -f "$conformance_entrypoint" ]; then
  fail "npm ci completed but the pinned conformance dist/index.js was not built"
fi
run_phase \
  "conformance-source-clean-after-build" \
  "conformance-source-clean-after-build" \
  "$repo_root" \
  "quiet" \
  git -C "$conformance_checkout" status \
  --porcelain=v1 \
  --untracked-files=no
if [ -n "$(
  tr -d '\r\n' \
    <"$results/conformance-source-clean-after-build.stdout.log"
)" ]; then
  fail \
    "npm ci/build modified tracked conformance source; refusing non-reproducible harness"
fi

{
  printf 'verified_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'final_tag=2026-07-28\n'
  printf 'final_spec_tag_commit=%s\n' "$resolved_final"
  printf 'tested_spec_snapshot=%s\n' "$spec_ref"
  printf 'conformance_commit=%s\n' "$resolved_conformance"
  printf 'conformance_main_at_verification=%s\n' "$latest_conformance"
  printf 'conformance_vendored_spec_source=%s\n' \
    "$resolved_conformance_source"
  printf 'conformance_status=latest official harness at verification; version still DRAFT/provisional\n'
  printf 'claim=final-spec-reconciled subset; latest official scoped tools/discover, request-metadata, and request-header scenarios are unaffected by the final subscriptions-only schema delta\n'
  printf 'excluded=full suite, auth, MRTR/request-state, subscriptions/listen, schema-reference, server, authorization-server\n'
} >"$results/UPSTREAM_STATUS.txt"

{
  printf 'spec=%s\n' "$spec_ref"
  printf 'conformance=%s\n' "$conformance_ref"
  printf 'conformance_main_at_verification=%s\n' "$latest_conformance"
  printf 'conformance_vendored_spec_source=%s\n' "$conformance_spec_source"
  printf 'conformance_status=latest official harness; version still DRAFT/provisional\n'
  printf 'protocol=2026-07-28 final-spec-reconciled subset (scoped tools/discover, request-metadata, and request-header client scenarios only)\n'
} >"$results/PINNED_REFS.txt"

scenarios=(
  request-metadata
  tools_call
  http-standard-headers
  http-custom-headers
  http-invalid-tool-headers
)

# Retain the official client-scenario inventory and prove that the reviewed
# scope manifest names every applicable scenario exactly once.
run_phase \
  "official-client-scenario-list" \
  "official-client-scenario-list" \
  "$conformance_checkout" \
  "quiet" \
  node "$conformance_entrypoint" list \
  --client \
  --spec-version 2026-07-28
cp \
  "$results/official-client-scenario-list.stdout.log" \
  "$results/OFFICIAL_CLIENT_SCENARIOS.txt"
cp "$scope_manifest" "$results/SCOPE.tsv"

run_phase \
  "official-scenario-inventory-normalize" \
  "official-scenario-inventory-normalize" \
  "$repo_root" \
  "quiet" \
  sed -n 's/^  - \(.*\) \[.*/\1/p' \
  "$results/OFFICIAL_CLIENT_SCENARIOS.txt"
run_phase \
  "official-scenario-inventory-sort" \
  "official-scenario-inventory-sort" \
  "$repo_root" \
  "quiet" \
  sort "$results/official-scenario-inventory-normalize.stdout.log"
run_phase \
  "scope-inventory-normalize" \
  "scope-inventory-normalize" \
  "$repo_root" \
  "quiet" \
  awk -F '\t' 'NR > 1 { print $1 }' "$results/SCOPE.tsv"
run_phase \
  "scope-inventory-sort" \
  "scope-inventory-sort" \
  "$repo_root" \
  "quiet" \
  sort "$results/scope-inventory-normalize.stdout.log"
run_phase \
  "scope-inventory-compare" \
  "scope-inventory-compare" \
  "$repo_root" \
  "replay" \
  diff -u \
  "$results/official-scenario-inventory-sort.stdout.log" \
  "$results/scope-inventory-sort.stdout.log"

run_phase \
  "scope-executed-normalize" \
  "scope-executed-normalize" \
  "$repo_root" \
  "quiet" \
  awk -F '\t' 'NR > 1 && $2 == "executed" { print $1 }' "$results/SCOPE.tsv"
run_phase \
  "scope-executed-sort" \
  "scope-executed-sort" \
  "$repo_root" \
  "quiet" \
  sort "$results/scope-executed-normalize.stdout.log"
printf '%s\n' "${scenarios[@]}" >"$results/requested-scenarios.txt"
run_phase \
  "requested-scenario-sort" \
  "requested-scenario-sort" \
  "$repo_root" \
  "quiet" \
  sort "$results/requested-scenarios.txt"
run_phase \
  "scope-executed-compare" \
  "scope-executed-compare" \
  "$repo_root" \
  "replay" \
  diff -u \
  "$results/requested-scenario-sort.stdout.log" \
  "$results/scope-executed-sort.stdout.log"

for scenario in "${scenarios[@]}"; do
  run_phase \
    "official-scenario-$scenario" \
    "$scenario" \
    "$results" \
    "replay" \
    node "$conformance_entrypoint" client \
    --command "$adapter" \
    --scenario "$scenario" \
    --spec-version 2026-07-28 \
    --timeout 30000
done

current_phase="complete"
run_completed=1
