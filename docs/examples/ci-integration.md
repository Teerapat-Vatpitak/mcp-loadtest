# CI integration

Use `mcp-loadtest` as a regression gate on every pull request. The exit code is
non-zero for threshold violations and unconditional correctness failures such
as no successful calls, deadlocks, response divergences, and terminal
transport failures.

> **Availability note:** use the examples pinned to `v0.1.0` only when the
> exact tag and its published GitHub Release exist and GitHub immutable
> releases is enabled. Source text and a manifest version do not prove
> availability. Otherwise, test by building an authorized checkout under
> [`docs/RELEASE.md`](../RELEASE.md).

## Quickest path: the composite action

This repo contains a composite GitHub Action ([`action.yml`](../../action.yml)
at the repo root). With a verified `v0.1.0` Release, an exact `uses:` pin gets
you a deadlock probe on every PR:

```yaml
# .github/workflows/loadtest.yml
name: MCP load test

on:
    pull_request:

jobs:
    loadtest:
        runs-on: ubuntu-latest
        steps:
            - uses: actions/checkout@v4

            - uses: actions/setup-python@v5
              with:
                  python-version: "3.13"

            - name: Install MCP server under test
              run: pip install -e .

            - name: Deadlock probe
              id: loadtest
              # Post-release only: pin the immutable exact version.
              uses: Teerapat-Vatpitak/mcp-loadtest@v0.1.0
              with:
                  version: v0.1.0
                  server: "python -m my_mcp"
                  # JSON array: each element becomes exactly one argv entry.
                  # Shell syntax is never evaluated.
                  args: '["--tool","get_market_data","--concurrent","10","--args","{\"ticker\":\"AAPL\"}"]'

            - name: Upload run artifacts
              if: always()
              uses: actions/upload-artifact@v4
              with:
                  name: mcp-loadtest-runs
                  path: runs/
```

After release, the action downloads the prebuilt release binary for the runner
(linux x64, macOS x64/arm64, windows x64 — windows runners work because the
steps use `shell: bash` via git-bash), verifies the `.sha256` sidecar, and
falls back to `cargo install --git` on any other platform (needs a Rust
toolchain in that case). A non-zero exit from the tool **fails the step** —
that is the regression gate. The action uploads nothing itself; keep the
`actions/upload-artifact` step if you want the reports.

### Inputs

| Input               | Default          | Description                                                                                                     |
| ------------------- | ---------------- | --------------------------------------------------------------------------------------------------------------- |
| `server`            | — (required)     | Server command string passed to `--server`, e.g. `"python -m my_mcp"`. Ignored by `command: run` (TOML has it). |
| `command`           | `deadlock-probe` | One of `deadlock-probe`, `run`, `cross`, `doctor`. For `run`, pass `--config ci.toml` via `args`.               |
| `args`              | `[]`             | Extra CLI arguments as a JSON array of strings. Each element is one literal argument; shell syntax is rejected. |
| `version`           | `v0.1.0`         | Stable release tag of the binary to install. Set `latest` explicitly only when reproducibility is not required. |
| `baseline`          | `""`             | Baseline `metrics.json` for a single-report `run` or `deadlock-probe`; rejected for `cross` and `doctor`.       |
| `working-directory` | `.`              | Directory to run in.                                                                                            |

The Action reference and installed binary are both pinned in the example.
`version: latest` asks GitHub for the newest Release at run time and is less
reproducible; it also cannot resolve before the first Release exists.

### Outputs

| Output       | Description                                                                                                         |
| ------------ | ------------------------------------------------------------------------------------------------------------------- |
| `report-dir` | Exact new run directory for `run`/`deadlock-probe`, the current `cross` report root, or empty for `doctor`.        |
| `passed`     | `"true"` only when command, report attribution, and optional comparison all pass; otherwise `"false"`.           |

A short results table is appended to the job summary (`$GITHUB_STEP_SUMMARY`)
on every run, including the `compare` diff when `baseline` is set.
Report attribution uses the set of run directories created by that invocation,
not modification time, so an older or concurrently written report cannot be
selected as the comparison input.

The rest of this page is the manual recipe — same behavior, more knobs — for
when you need fine-grained control over install, caching, or thresholds.

## What this gives you

- Every PR runs a fixed load profile against your MCP server.
- If p99 latency regresses past your budget, CI fails.
- If a lazy-init deadlock sneaks in, CI fails in under 10 seconds.
- The machine-readable `metrics.json` is uploaded as a build artifact for
  trend analysis or `mcp-loadtest compare` against a stored baseline.

## TOML config (`ci.toml`)

Keep this in your repo next to `Cargo.toml` or `pyproject.toml`. It declares
both the load shape and the pass/fail budget — version-controlled together.

```toml
# ci.toml — invoked from CI as: mcp-loadtest run --config ci.toml

[server]
command = "python"
args = ["-m", "my_mcp"]
transport = "stdio"
startup_timeout = "10s"

[server.env]
LOG_LEVEL = "warn"

[scenario]
type = "sustained"
duration = "30s"
concurrent = 25
tool = "get_market_data"
args = { ticker = "AAPL" }

[thresholds]
# Latency budget — anything past these fails the build.
p50_latency = "50ms"
p95_latency = "200ms"
p99_latency = "500ms"
# Max acceptable error rate (1 % of calls).
error_rate = 0.01
# Anything that doesn't return inside this window counts as a hang.
hang_timeout = "5s"

[output]
report_dir = "./runs"
# CI only needs the machine-readable formats; terminal output is for humans.
formats = ["markdown", "json"]
```

`mcp-loadtest example-config > ci.toml` is a fast starting point; trim the
fields you don't care about.

## GitHub Actions

```yaml
# .github/workflows/loadtest.yml
name: MCP load test

on:
    pull_request:
    push:
        branches: [main]

jobs:
    loadtest:
        runs-on: ubuntu-latest
        steps:
            - uses: actions/checkout@v4

            - name: Set up Python
              uses: actions/setup-python@v5
              with:
                  python-version: "3.13"

            - name: Install MCP server under test
              run: pip install -e .

            # Post-release example: install from the immutable git tag.
            - name: Install mcp-loadtest
              run: cargo install --git https://github.com/Teerapat-Vatpitak/mcp-loadtest --tag v0.1.0 --locked mcp-loadtest-cli

            # Headline check — fails the build on any deadlock.
            - name: Deadlock probe
              run: |
                  mcp-loadtest deadlock-probe \
                    --server "python -m my_mcp" \
                    --tool get_market_data \
                    --concurrent 10 \
                    --hang-threshold 2s \
                    --grace-period 5s \
                    --args '{"ticker":"AAPL"}'

            # Sustained load with threshold gating from ci.toml.
            - name: Sustained load
              run: mcp-loadtest run --config ci.toml

            - name: Upload run artifacts
              if: always()
              uses: actions/upload-artifact@v4
              with:
                  name: mcp-loadtest-runs
                  path: runs/
```

The `if: always()` on the artifact step keeps the report even when the load
test fails — that's exactly when you want to download it and look at
`report.md`.

## What a passing run looks like

```text
$ mcp-loadtest run --config ci.toml
Run 01KR9JX7E4P638TKQM96YA0B4Z
Status: PASS
Server: python -m my_mcp
Scenario: sustained (25 concurrent, 30s)

Latency  p50=12.3ms  p95=45.6ms  p99=98.2ms  p999=210.1ms
Throughput: 1842 req/s   Errors: 0 (0.00%)
Deadlocks: 0  Hangs: 0
Threshold violations: none

Report: runs/01KR9JX7E4P638TKQM96YA0B4Z/report.md
Metrics: runs/01KR9JX7E4P638TKQM96YA0B4Z/metrics.json
$ echo $?
0
```

## What a failing run looks like

The exit code is non-zero, the threshold violations are listed inline, and the
full report is on disk for the artifact step to grab:

```text
$ mcp-loadtest run --config ci.toml
Run 01KR9JZ8H8XQF5KCDH7V2QY3MM
Status: FAIL (1 threshold violation)
Server: python -m my_mcp
Scenario: sustained (25 concurrent, 30s)

Latency  p50=18.4ms  p95=420.3ms  p99=812.5ms  p999=1480.9ms
Throughput: 1218 req/s   Errors: 0 (0.00%)
Deadlocks: 0  Hangs: 0
Threshold violations:
  - p99_latency: expected <= 500ms, got 812.5ms

Error: 1 threshold violation(s) — see report
$ echo $?
1
```

## Compare against a baseline (optional)

Once you have a known-good `metrics.json` checked in (or pulled from a
nightly main run), use `compare` to gate on regressions instead of absolute
budgets:

```yaml
- name: Regression diff vs main
  run: |
      mcp-loadtest compare \
        ./baseline/metrics.json \
        ./runs/*/metrics.json \
        --format markdown >> $GITHUB_STEP_SUMMARY
```

`compare` exits non-zero when any metric regresses past the policy in the
baseline. The markdown rendering drops cleanly into the GitHub Actions job
summary.

## Remote authentication scope

For `http`, `sse`, or `ws`, keep credentials in the CI secret store and map a
header name to the environment-variable name in TOML:

```toml
[server]
transport = "http"
url = "https://mcp.example.com/mcp"
allowed_hosts = ["mcp.example.com"]
headers_from_env = { Authorization = "MCP_AUTHORIZATION" }
```

```yaml
- name: Authenticated remote load test
  env:
    MCP_AUTHORIZATION: Bearer ${{ secrets.MCP_TOKEN }}
  run: mcp-loadtest run --config remote-ci.toml
```

The environment variable must contain the complete header value.
`headers_from_env` is the only explicit remote-credential facility. If it is
nonempty, HTTP/SSE require `https://` and WebSocket requires `wss://`; the
client never falls back to plaintext. URL userinfo is forbidden. Query
strings are transmitted unchanged to the target but replaced wholesale with
`?redacted` in reports and traces, so never put credentials in a query.
Literal headers in TOML, OAuth login/refresh/discovery, and interactive
authorization are not supported, and protocol-owned/connection headers
cannot be overridden. Never place a secret in the Action's `server` or `args`
value: arguments are not shell-evaluated, and the Action redacts server
identity plus malformed-JSON argument diagnostics, but arbitrary server
response content is not sanitized. Workflow inputs are not a secret store and
the operating system can inspect child process argv. Use environment-backed
credentials.

## Tips

- **Pin a fixed concurrency.** Run-to-run noise on shared runners makes
  variable concurrency unreliable. Pick a number that fits on a 2-vCPU runner.
- **Set `hang_timeout` shorter than the job timeout.** A wedged server should
  show up as a clear `FAIL`, not a CI cancellation.
- **Keep one `ci.toml` per profile.** A "smoke" profile (10s, 5 concurrent)
  on PR + a "soak" profile (10m, 5 concurrent) on a nightly schedule is the
  pattern that scales.
- **The `metrics.json` schema is stable** — see
  [`docs/schema/metrics.v1.json`](../schema/metrics.v1.json). Safe to script
  custom assertions on top of it without breakage across patch releases.
