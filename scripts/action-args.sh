#!/usr/bin/env bash
#
# Data-only decoder and report-root helpers for action.yml.
#
# Argument contract:
#   - The caller owns a Bash array named `argv`.
#   - The input is a JSON array containing only strings.
#   - Each string is appended to `argv` exactly once, without shell parsing.
#
# This helper intentionally avoids eval, `sh -c`, and shell word splitting.
# Python is used only as a JSON decoder/enumerator. GitHub-hosted Linux, macOS,
# and Windows runners all provide it.

# Accept only immutable stable release tags before a caller-controlled version
# can reach asset paths, git arguments, or GitHub environment files.
mcp_loadtest_is_release_tag() {
  local value="${1-}"
  [[ "$value" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
}

mcp_loadtest_append_json_args() {
  local input="${1-}"
  local python_cmd=""
  local record=""
  local value=""
  local parse_complete=false
  local parse_failed=false

  # Keep an explicitly empty input backward-compatible with "no extra args".
  # The published action default is the unambiguous JSON value `[]`.
  if [ -z "$input" ]; then
    return 0
  fi

  if command -v python3 >/dev/null 2>&1; then
    python_cmd="python3"
  elif command -v python >/dev/null 2>&1; then
    python_cmd="python"
  else
    echo "mcp-loadtest action: Python is required to decode the JSON args input" >&2
    return 2
  fi

  # Process substitution keeps this loop in the current shell, so appends to
  # `argv` survive. The terminal `done` record also propagates decoder failure:
  # a failed/truncated decoder stream can never be mistaken for an empty list.
  while IFS= read -r -d '' record; do
    case "$record" in
      arg)
        if ! IFS= read -r -d '' value; then
          parse_failed=true
          break
        fi
        argv+=("$value")
        ;;
      done)
        parse_complete=true
        ;;
      *)
        parse_failed=true
        break
        ;;
    esac
  done < <(
    "$python_cmd" - "$input" <<'PY'
import json
import sys


def fail(message: str) -> None:
    print(f"mcp-loadtest action: {message}", file=sys.stderr)
    raise SystemExit(2)


try:
    values = json.loads(sys.argv[1])
except (json.JSONDecodeError, UnicodeError):
    fail("args must be valid JSON")

if not isinstance(values, list):
    fail("args must be a JSON array")
if not all(isinstance(value, str) for value in values):
    fail("every args array element must be a string")
if any("\0" in value for value in values):
    fail("args array elements cannot contain NUL bytes")

# Validate the entire value before emitting anything. Tagged records make the
# stream unambiguous even when an argument contains whitespace or newlines.
output = sys.stdout.buffer
for value in values:
    output.write(b"arg\0")
    output.write(value.encode("utf-8"))
    output.write(b"\0")
output.write(b"done\0")
PY
  )

  if [ "$parse_failed" = true ] || [ "$parse_complete" != true ]; then
    return 2
  fi
}

# Give every Action invocation an empty report root that it alone owns.
# Results are returned in globals so paths containing spaces never pass through
# shell word splitting:
#   MCP_LOADTEST_REPORT_MODE = single | multiple | none
#   MCP_LOADTEST_REPORT_ROOT = unique root, or empty for `none`
#
# Callers cannot choose either output override. The Action appends its own
# `--output-dir` for deadlock-probe/cross or hidden `--action-output-dir` for
# run only after this function accepts the caller's literal argv.
mcp_loadtest_prepare_report_target() {
  if [ "$#" -lt 2 ]; then
    echo "mcp-loadtest action: report target requires a command and temp directory" >&2
    return 2
  fi

  local command="$1"
  local temp_base="$2"
  shift 2
  local arg=""
  local report_root=""

  MCP_LOADTEST_REPORT_MODE=""
  MCP_LOADTEST_REPORT_ROOT=""

  case "$command" in
    doctor)
      MCP_LOADTEST_REPORT_MODE="none"
      ;;
    cross)
      MCP_LOADTEST_REPORT_MODE="multiple"
      ;;
    deadlock-probe|run)
      MCP_LOADTEST_REPORT_MODE="single"
      ;;
    *)
      echo "mcp-loadtest action: unsupported command while preparing reports" >&2
      return 2
      ;;
  esac

  for arg in "$@"; do
    case "$arg" in
      --action-output-dir|--action-output-dir=*)
        echo "mcp-loadtest action: --action-output-dir is reserved for the Action" >&2
        return 2
        ;;
      --action-redact-server-identity|--action-redact-server-identity=*)
        echo "mcp-loadtest action: --action-redact-server-identity is reserved for the Action" >&2
        return 2
        ;;
    esac
    case "${command}:${arg}" in
      deadlock-probe:--output-dir|deadlock-probe:--output-dir=*|\
      cross:--output-dir|cross:--output-dir=*|\
      run:--output-dir|run:--output-dir=*)
        echo "mcp-loadtest action: --output-dir is reserved for the Action" >&2
        return 2
        ;;
    esac
  done

  if [ "$MCP_LOADTEST_REPORT_MODE" = "none" ]; then
    return 0
  fi

  if [ -z "$temp_base" ]; then
    echo "mcp-loadtest action: runner temp directory is empty" >&2
    return 2
  fi
  case "$temp_base" in
    *$'\r'*|*$'\n'*)
      echo "mcp-loadtest action: runner temp directory cannot contain CR or LF" >&2
      return 2
      ;;
  esac

  if ! report_root="$(mktemp -d "${temp_base%/}/mcp-loadtest-action-runs.XXXXXX")"; then
    echo "mcp-loadtest action: could not create an invocation report directory" >&2
    return 2
  fi
  if ! chmod 700 "$report_root"; then
    echo "mcp-loadtest action: could not restrict the invocation report directory" >&2
    return 2
  fi
  MCP_LOADTEST_REPORT_ROOT="$report_root"
}

# Enumerate ULID-shaped immediate child directories inside the unique root.
# Results are returned in MCP_LOADTEST_RUN_DIRS. A tagged terminal record
# prevents a truncated decoder from being mistaken for "zero reports".
mcp_loadtest_collect_run_dirs() {
  local report_root="${1-}"
  local python_cmd=""
  local record=""
  local value=""
  local parse_complete=false
  local parse_failed=false

  MCP_LOADTEST_RUN_DIRS=()

  if command -v python3 >/dev/null 2>&1; then
    python_cmd="python3"
  elif command -v python >/dev/null 2>&1; then
    python_cmd="python"
  else
    echo "mcp-loadtest action: Python is required to enumerate the current report" >&2
    return 2
  fi

  while IFS= read -r -d '' record; do
    case "$record" in
      run-dir)
        if ! IFS= read -r -d '' value; then
          parse_failed=true
          break
        fi
        MCP_LOADTEST_RUN_DIRS+=("$value")
        ;;
      done)
        parse_complete=true
        ;;
      *)
        parse_failed=true
        break
        ;;
    esac
  done < <(
    "$python_cmd" - "$report_root" <<'PY'
import os
import re
import sys


root_text = sys.argv[1]
root = os.fsencode(root_text)
ulid = re.compile(br"^[0-9A-HJKMNP-TV-Z]{26}$")
try:
    with os.scandir(root) as scan:
        entries = list(scan)
except OSError as error:
    print(f"mcp-loadtest action: cannot inspect report directory: {error}", file=sys.stderr)
    raise SystemExit(2)

run_dirs = []
for entry in entries:
    name = os.fsencode(entry.name)
    if ulid.fullmatch(name) and entry.is_dir(follow_symlinks=False):
        run_dirs.append(name)

output = sys.stdout.buffer
for name in sorted(run_dirs):
    output.write(b"run-dir\0")
    output.write(os.fsencode(os.path.join(root_text, os.fsdecode(name))))
    output.write(b"\0")
output.write(b"done\0")
PY
  )

  if [ "$parse_failed" = true ] || [ "$parse_complete" != true ]; then
    return 2
  fi
}
