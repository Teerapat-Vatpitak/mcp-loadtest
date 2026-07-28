#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/action-args.sh
source "${repo_root}/scripts/action-args.sh"

fail() {
  echo "action args test failed: $*" >&2
  exit 1
}

assert_eq() {
  local expected="$1"
  local actual="$2"
  local context="$3"
  if [ "$actual" != "$expected" ]; then
    fail "${context}: expected <${expected}>, got <${actual}>"
  fi
}

assert_same_path() {
  local expected="$1"
  local actual="$2"
  local context="$3"
  local python_cmd=""

  if command -v python3 >/dev/null 2>&1; then
    python_cmd="python3"
  elif command -v python >/dev/null 2>&1; then
    python_cmd="python"
  else
    fail "${context}: Python is required to compare paths"
  fi

  if ! "$python_cmd" - "$expected" "$actual" <<'PY'
import os
import sys

raise SystemExit(0 if os.path.samefile(sys.argv[1], sys.argv[2]) else 1)
PY
  then
    fail "${context}: expected path <${expected}>, got <${actual}>"
  fi
}

assert_count() {
  local expected="$1"
  local context="$2"
  if [ "${#argv[@]}" -ne "$expected" ]; then
    fail "${context}: expected ${expected} argv entries, got ${#argv[@]}"
  fi
}

# Release tags are data too: reject newlines, workflow-command text, mutable
# refs, and semver lookalikes before action.yml writes to GITHUB_ENV.
for valid_tag in v0.1.0 v1.0.0 v12.34.56; do
  mcp_loadtest_is_release_tag "$valid_tag" ||
    fail "valid release tag was rejected: ${valid_tag}"
done
for invalid_tag in \
  latest \
  main \
  v0.1 \
  v01.2.3 \
  v1.02.3 \
  v1.2.03 \
  v1.2.3-rc.1 \
  $'v0.1.0\nINJECTED=value' \
  $'v0.1.0\r\nPATH=/tmp/owned' \
  'v0.1.0::warning::owned'
do
  if mcp_loadtest_is_release_tag "$invalid_tag"; then
    fail "invalid release tag was accepted"
  fi
done

tmp_dir="$(mktemp -d)"
trap 'rm -rf -- "$tmp_dir"' EXIT
marker="${tmp_dir}/injection-ran"

# Every report-producing command gets a new empty root below the runner temp;
# caller-provided output overrides are rejected before that root is created.
runner_temp="${tmp_dir}/runner temp"
mkdir -p "$runner_temp"
mcp_loadtest_prepare_report_target deadlock-probe "$runner_temp" --tool echo
assert_eq "single" "$MCP_LOADTEST_REPORT_MODE" "deadlock report mode"
deadlock_root="$MCP_LOADTEST_REPORT_ROOT"
case "$deadlock_root" in
  "${runner_temp}"/mcp-loadtest-action-runs.*) ;;
  *) fail "deadlock report root was not created below runner temp: ${deadlock_root}" ;;
esac
[ -d "$deadlock_root" ] || fail "deadlock report root was not created"
if find "$deadlock_root" -mindepth 1 -print -quit | grep -q .; then
  fail "new Action report root was not empty"
fi

mcp_loadtest_prepare_report_target cross "$runner_temp" --tool echo
assert_eq "multiple" "$MCP_LOADTEST_REPORT_MODE" "cross report mode"
cross_root="$MCP_LOADTEST_REPORT_ROOT"
[ "$cross_root" != "$deadlock_root" ] || fail "separate invocations reused a report root"

mcp_loadtest_prepare_report_target run "$runner_temp" --config bench.toml
assert_eq "single" "$MCP_LOADTEST_REPORT_MODE" "run report mode"
run_root="$MCP_LOADTEST_REPORT_ROOT"
[ "$run_root" != "$deadlock_root" ] || fail "run reused a prior report root"
[ "$run_root" != "$cross_root" ] || fail "run reused the cross report root"

mcp_loadtest_prepare_report_target doctor "$runner_temp" --runs-dir ./somewhere
assert_eq "none" "$MCP_LOADTEST_REPORT_MODE" "doctor report mode"
assert_eq "" "$MCP_LOADTEST_REPORT_ROOT" "doctor report root"

if mcp_loadtest_prepare_report_target \
  deadlock-probe "$runner_temp" --output-dir "${tmp_dir}/caller" 2>/dev/null
then
  fail "caller-controlled deadlock --output-dir was accepted"
fi
if mcp_loadtest_prepare_report_target \
  cross "$runner_temp" --output-dir="${tmp_dir}/caller" 2>/dev/null
then
  fail "caller-controlled cross --output-dir was accepted"
fi
if mcp_loadtest_prepare_report_target \
  run "$runner_temp" --config bench.toml --action-output-dir "${tmp_dir}/caller" 2>/dev/null
then
  fail "caller-controlled run --action-output-dir was accepted"
fi
if mcp_loadtest_prepare_report_target \
  doctor "$runner_temp" --action-redact-server-identity=false 2>/dev/null
then
  fail "caller-controlled Action redaction override was accepted"
fi

# A foreign ULID outside the Action-owned root is never eligible for
# attribution. Only immediate ULID directories inside the unique root return.
foreign_run="01J00000000000000000000000"
owned_run="01J00000000000000000000001"
mkdir -p \
  "${tmp_dir}/foreign reports/${foreign_run}" \
  "${deadlock_root}/${owned_run}" \
  "${deadlock_root}/not-a-run"
mcp_loadtest_collect_run_dirs "$deadlock_root"
if [ "${#MCP_LOADTEST_RUN_DIRS[@]}" -ne 1 ]; then
  fail "expected exactly one report inside the Action-owned root"
fi
assert_same_path \
  "${deadlock_root}/${owned_run}" \
  "${MCP_LOADTEST_RUN_DIRS[0]}" \
  "Action-owned invocation report"

# Common flags, spaces, embedded quotes, and a JSON tool argument must each
# survive as exactly one literal argv element.
argv=(mcp-loadtest deadlock-probe)
mcp_loadtest_append_json_args \
  '["--tool","get market data","--label","say \"hello\"","--args","{\"ticker\":\"AAPL\",\"count\":2,\"nested\":{\"active\":true}}"]'
assert_count 8 "common arguments"
assert_eq "--tool" "${argv[2]}" "flag"
assert_eq "get market data" "${argv[3]}" "spaces"
assert_eq "--label" "${argv[4]}" "quoted flag"
assert_eq 'say "hello"' "${argv[5]}" "embedded quotes"
assert_eq "--args" "${argv[6]}" "JSON flag"
assert_eq '{"ticker":"AAPL","count":2,"nested":{"active":true}}' "${argv[7]}" "JSON value"

# Command substitution text must remain data and must not create the marker.
command_substitution_payload="$(printf '["$(touch %s)"]' "$marker")"
argv=(mcp-loadtest)
mcp_loadtest_append_json_args "$command_substitution_payload"
assert_count 2 "command substitution"
assert_eq "\$(touch ${marker})" "${argv[1]}" "command substitution literal"
[ ! -e "$marker" ] || fail "command substitution payload executed"

# Semicolons, closing punctuation, and comments must remain one literal arg.
array_breakout_payload="$(printf '["x); touch %s; #"]' "$marker")"
argv=(mcp-loadtest)
mcp_loadtest_append_json_args "$array_breakout_payload"
assert_count 2 "array breakout"
assert_eq "x); touch ${marker}; #" "${argv[1]}" "array breakout literal"
[ ! -e "$marker" ] || fail "array-breakout payload executed"

# GitHub workflow-command-looking text must remain an unprinted literal argv
# value; the decoder must not reinterpret or emit it as control syntax.
argv=(mcp-loadtest)
mcp_loadtest_append_json_args \
  '["::error file=owned.rs,line=1::injected","%0AENV_INJECTED=yes"]'
assert_count 3 "workflow-command literals"
assert_eq "::error file=owned.rs,line=1::injected" "${argv[1]}" "workflow command literal"
assert_eq "%0AENV_INJECTED=yes" "${argv[2]}" "encoded newline literal"

# Empty strings and embedded newlines are legitimate argv values.
argv=(mcp-loadtest)
mcp_loadtest_append_json_args '["","line one\nline two"]'
assert_count 3 "empty and multiline arguments"
assert_eq "" "${argv[1]}" "empty argument"
assert_eq $'line one\nline two' "${argv[2]}" "multiline argument"

# Invalid, ambiguous, and non-string inputs fail closed.
for invalid in \
  'not JSON' \
  '"--tool echo"' \
  '{"tool":"echo"}' \
  '["--count",3]' \
  '["--flag",null]' \
  '["nul\u0000byte"]'
do
  argv=(mcp-loadtest)
  if mcp_loadtest_append_json_args "$invalid" 2>/dev/null; then
    fail "invalid input was accepted: ${invalid}"
  fi
  assert_count 1 "failure must not append partial argv"
done

# Guard the actual composite action against reintroducing shell execution
# primitives or interpolating caller inputs into GitHub workflow commands.
if grep -nE \
  '(^|[[:space:]])(eval|sh[[:space:]]+-c|bash[[:space:]]+-c)([[:space:]]|$)' \
  "${repo_root}/action.yml"
then
  fail "action.yml must not use eval, sh -c, or bash -c"
fi
if grep -nE \
  '::(error|warning|notice)::.*INPUT_' \
  "${repo_root}/action.yml"
then
  fail "action.yml must not interpolate caller input into workflow commands"
fi
if grep -nE \
  '(echo|printf).*INPUT_SERVER' \
  "${repo_root}/action.yml"
then
  fail "action.yml must not echo the caller's server command"
fi
if ! grep -F \
  'argv+=(--action-redact-server-identity)' \
  "${repo_root}/action.yml" >/dev/null
then
  fail "action.yml must append its reserved server-identity redaction flag"
fi
if grep -nE \
  'mcp_loadtest_(snapshot_run_dirs|collect_new_run_dirs)' \
  "${repo_root}/action.yml" \
  "${repo_root}/scripts/action-args.sh"
then
  fail "Action report attribution must not use set-difference snapshots"
fi
if ! grep -Fq "steps.run.outputs.passed || 'false'" "${repo_root}/action.yml"; then
  fail "Action passed output must remain false when installation fails before the run step"
fi

echo "action args contract: all tests passed"
