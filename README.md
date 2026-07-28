# mcp-loadtest

[![CI](https://github.com/Teerapat-Vatpitak/mcp-loadtest/actions/workflows/ci.yml/badge.svg)](https://github.com/Teerapat-Vatpitak/mcp-loadtest/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/rust-toolchain.toml)

> Load tester **and bug detector** for MCP (Model Context Protocol) servers. Catches lazy-init deadlocks, synchronized response divergences, hangs, and perf regressions that unit tests miss.

> **Release identity:** `v0.1.0` is the first version intended for public
> distribution; `v0.0.1` was internal-only and was never tagged or released.
> A source checkout does not prove that external artifacts exist—verify the
> exact `v0.1.0` tag and its GitHub Release before using prebuilt binaries or
> the Action. The release workflow refuses publication unless GitHub
> **immutable releases** is enabled, which locks that tag and its assets after
> publication. This gate requires a separate read-only
> `IMMUTABLE_RELEASES_AUDIT_TOKEN`; the normal workflow token cannot inspect
> the repository setting. crates.io is not a `v0.1.0` distribution channel.

## Why mcp-loadtest

Lazy-init inside an async worker thread is one of the easiest ways to ship a broken MCP server: `initialize` works, `tools/list` works, the first concurrent `tools/call` hangs forever. Unit tests never see it — the bug only surfaces when a real client drives the protocol end-to-end. The flagship example is [HKUDS/Vibe-Trading PR #85](https://github.com/HKUDS/Vibe-Trading/pull/85): a five-line fix that took hours of differential testing to find. `mcp-loadtest` finds it in seconds, and exits non-zero so it can fail your CI gate:

```bash
$ mcp-loadtest deadlock-probe --server "python -m vibe_trading_mcp" \
    --tool analyze_options --concurrent 1 \
    --args '{"spot":450,"strike":460,"expiry_days":30}'

Run 01KR9JX7E4P638TKQM96YA0B4Z
Status: FAIL (1 deadlock)
Deadlocks: 1   Hangs: 0   Errors: 0

Error: DEADLOCK DETECTED — 1 deadlock(s), 0 slow response(s), 0 error(s), 0 teardown failure(s), 0 threshold violation(s)
$ echo $?
1
```

The regression test that catches the exact Vibe-Trading commit lives at [`tests/vibe_trading_regression.rs`](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/crates/engine/tests/vibe_trading_regression.rs) (pinned to `71220c7c`, the parent of PR #85).

## What it does

- **Bug-class detection** — for `concurrent > 1`, `deadlock_probe` completes N independent sessions then releases one `tools/call` per worker through a shared start gate; `concurrent = 1` is the focused single-session probe. `race_check` uses the same synchronized shape and fails on divergent responses. `hang_detect` is used by these and other latency-sensitive scenario paths; it is not a universal transport wrapper.
- **Load testing** — `sustained` / `ramp` / `spike` / `soak` with p50/p95/p99/p999 histograms, cold-start handshake measurement, weighted pattern mixes, and periodic RSS sampling for leak hunting.
- **Record & replay** — `run --trace <file>` records every JSON-RPC frame as versioned JSONL (secret-looking arguments redacted by default); `replay <file> --server "..."` re-runs the trace against a fresh server and diffs responses, exiting non-zero on divergence ([ADR 0021](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/adr/0021-trace-record-replay.md)).
- **Reporting** — ANSI terminal, markdown, self-contained HTML, and schema-stable `metrics.json` ([docs/schema/metrics.v1.json](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/schema/metrics.v1.json), pinned by a conformance test). `compare` diffs two runs for regressions; `cross` runs N servers side by side.
- **AI-agent friendly** — structured errors with `Hint:` lines, and `mcp-loadtest serve --mcp` exposes the tool itself as an MCP server so Claude Code, Cursor, or any MCP-aware agent can call `deadlock_probe`, `sustained_load`, and `compare_runs` directly ([DESIGN.md §21.2](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#212-self-hosted-mcp-server-mcp-loadtest-serve---mcp)).

## CI gating

Every completed run resolves to a pass/fail. A run fails when configured
thresholds are breached, no call succeeds, a deadlock or response divergence
is found, any pooled worker is missing, a race-check cohort is incomplete,
session teardown is uncertain, or the recorder observes a deadlock, timeout,
protocol/malformed response, crash, disconnect, or cancellation:

For diagnostic `deadlock_probe`, `race_check`, and `fuzzer` cohorts, one call
breaching the configured hang threshold also fails the whole cohort—even if
other calls succeeded. Normal load scenarios keep partial slow/application
outcomes under their configured threshold policy.

The fuzzer records a healthy rejection of its deliberately malformed input as
`ExpectedRejection`, not `ProtocolError`; it counts as a successful probe while
real protocol and client-side validation failures still fail closed.

- **Composite GitHub Action** — after `v0.1.0` is released, pin
  `uses: Teerapat-Vatpitak/mcp-loadtest@v0.1.0`. Its `args` input is a JSON
  array of literal strings; it never evaluates caller-controlled shell text.
  Its binary version also defaults to `v0.1.0`, so the Action and executable
  remain aligned unless a caller explicitly opts into `latest`.
  Do not put credentials in `server` or `args`: the Action removes server
  identity and malformed-JSON argument values from its retained
  artifacts/diagnostics, but it cannot sanitize arbitrary server response
  content or hide child process argv from the operating system. Use
  environment-backed credentials.
  Until that tag and its release assets exist, the Action is not a published
  installation channel. Post-release workflow:
  [docs/examples/ci-integration.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/examples/ci-integration.md).
- **Threshold gating** — `[thresholds]`: p50–p999 latency, error rate, memory growth, RSS leak slope, per-tool SLOs. Any breach → non-zero exit. Deadlocks are zero-tolerance. Every configured latency/error/tool SLO requires matching recorder evidence. Process gates likewise fail closed when samples are missing or when stdio factory children fall outside the current single-PID sampler; missing evidence never becomes PASS.
- **Baseline regression diff** — `mcp-loadtest compare baseline.json current.json` flags p99 / error-rate / deadlock regressions, with configurable budgets (`--max-p99-regression-pct`, `--max-error-rate-regression-pp`) also exposed as `compare_runs` MCP tool args (ADR 0009).
- **Protocol-aware strict mode** — every run requires a successful initial
  `tools/list` for protocol discovery and coverage. Enabling
  `[validation] strict = true` additionally validates every call's arguments
  against the advertised `inputSchema` before the call (a mismatch gates the
  run). Each result's `structuredContent` is checked against its
  `outputSchema` (warn-only, never gates — ADR 0010). Schema validation is off
  by default; discovery is not.

## Quick start

To test a checkout—or whenever a verified `v0.1.0` Release is unavailable—
build from source:

```bash
cargo build --release --locked -p mcp-loadtest-cli
# binary: target/release/mcp-loadtest (target/release/mcp-loadtest.exe on Windows)

# Quick deadlock smoke against a real MCP server
./target/release/mcp-loadtest deadlock-probe --server "python -m my_mcp" --tool foo

# Sustained load from a config file (print a starter with `example-config`)
./target/release/mcp-loadtest run --config bench.toml

# Compare two runs (e.g. main vs PR branch)
./target/release/mcp-loadtest compare runs/baseline/metrics.json runs/current/metrics.json
```

When the exact `v0.1.0` tag exists, the reproducible git install is:

```bash
cargo install --git https://github.com/Teerapat-Vatpitak/mcp-loadtest \
  --tag v0.1.0 --locked mcp-loadtest-cli
```

A minimal `bench.toml`:

```toml
[server]
command = "python"
args = ["-m", "my_mcp"]
transport = "stdio"

[scenario]
type = "sustained"
duration = "60s"
concurrent = 50
tool = "get_market_data"
args = { ticker = "AAPL" }

[thresholds]
p99_latency = "500ms"
error_rate = 0.01
hang_timeout = "5s"

[output]
report_dir = "./runs"
formats = ["terminal", "markdown", "json"]  # "html" is also available
```

Remote transports (`http`, `sse`, `ws`) accept static outbound headers only
through environment-variable indirection:

```toml
[server]
transport = "http"
url = "https://mcp.example.com/mcp"
allowed_hosts = ["mcp.example.com"]
headers_from_env = { Authorization = "MCP_AUTHORIZATION" }
```

Set `MCP_AUTHORIZATION` to the complete value, for example `Bearer ...`.
`headers_from_env` is the only explicit remote-credential facility. When it
is nonempty, HTTP/SSE require `https://` and WebSocket requires `wss://`;
there is no plaintext fallback. URL userinfo (`user:password@host`) is
forbidden. URL queries are transmitted unchanged to the target but are
replaced wholesale with `?redacted` in reports and traces, so they must not
contain secrets. Literal secret values in TOML, OAuth login/refresh,
interactive authorization, and token discovery are intentionally out of
scope. Protocol-owned and connection-management headers cannot be
overridden: all `MCP-*`, `Sec-WebSocket-*`, and HTTP hop-by-hop/proxy headers
are denied. `stdio` credentials belong in `[server.env]` instead.

Remote-controlled HTTP response bodies, SSE event data, and WebSocket messages
are capped at 16 MiB during network consumption/reassembly. The SSE and
WebSocket reader queues and id-mismatch buffers also have a shared 32 MiB
retained-byte budget per transport.

The default advertised MCP revision is the stable handshake-based
`2025-11-25`; `2025-03-26` and `2025-06-18` can also be pinned for version
matrix testing. The `2026-07-28` stateless path is an **experimental
implementation of a scoped subset of the final revision**. It must be selected
explicitly and is not the default or a full-revision conformance claim. The
subset is reconciled to the official final specification tag at commit
[`5f5440bb`](https://github.com/modelcontextprotocol/modelcontextprotocol/commit/5f5440bb26a62e2cf3440b92da5a667efa03b267)
and conformance commit
[`49103de6`](https://github.com/modelcontextprotocol/conformance/commit/49103de6ed70804e940637bf3e9e29e4a3f54e64).
The final schema definitions used by this implementation match both the
reviewed candidate definitions and the official harness's vendored pre-final
definitions; the final-only `subscriptions/listen` response-envelope change is
outside the implemented scope.

The conformance pin is the latest official harness commit at verification, but that harness
still labels the `2026-07-28` version DRAFT/provisional and vendors a pre-final
schema sourced from specification commit
`71e306956a4959c9655e5036be215d41986596e6`. Final-spec reconciliation
therefore comes from the independently verified final tag and machine-checked
schema comparison above. Each runner verifies that upstream `main` still
resolves to the reviewed pin before testing; an upstream move fails closed
until the new revision is reviewed. The release gate also runs that official
harness via
`crates/protocol/tests/run-official-conformance.{sh,ps1}` for five
tool/discovery/request-metadata/header client scenarios. Its retained
`FINAL_SCHEMA_RECONCILIATION.txt` records the separate final-schema proof, and
`SCOPE.tsv` names every other applicable client scenario as not executed with
a reason. The run does not claim full-protocol, auth, MRTR/request-state,
`subscriptions/listen`, schema-reference, server, or authorization-server
conformance, and it is not a final-promoted conformance-suite result. See
[ADR 0023](docs/adr/0023-mcp-2026-final-reconciliation.md).

Library usage: the crate is a normal Rust library — see [`tests/vibe_trading_regression.rs`](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/crates/engine/tests/vibe_trading_regression.rs) for a runnable `Scenario::drive` example.

## Built-in scenarios

| Scenario         | Detects                                                               |
| ---------------- | --------------------------------------------------------------------- |
| `cold_start`     | startup time regressions, init-time deadlocks                         |
| `sustained`      | baseline p99 latency, throughput, error rate                          |
| `ramp`           | break-point — concurrency where p99 explodes                          |
| `spike`          | sudden-burst load — baseline → peak window → cooldown                 |
| `soak`           | memory leaks under sustained load                                     |
| `deadlock_probe` | lazy-init deadlocks (the canonical Vibe-Trading bug class)            |
| `race_check`     | non-determinism / order-sensitive bugs                                |
| `pattern`        | weighted random mixes (explore-then-act, read-then-write, multi-step) |
| `version_matrix` | revision-specific bugs — same server driven once per MCP protocol revision, outcomes diffed side by side |

Each scenario is one `impl Scenario` in [`crates/engine/src/scenario/`](https://github.com/Teerapat-Vatpitak/mcp-loadtest/tree/main/crates/engine/src/scenario/) and exposes a schema fragment for its config block in the Rust API. The published, versioned schema currently covers `metrics.json`; a complete standalone config schema has not shipped ([DESIGN.md §8](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#8-built-in-scenarios)).

## Positioning

Purpose-built MCP load-test projects and general load generators already exist,
including [`reaatech/mcp-load-test`](https://github.com/reaatech/mcp-load-test).
`mcp-loadtest` focuses on a narrower reliability-CI workflow: reproduce
protocol-level hangs/deadlocks, drive real multi-session workloads, record
MCP-aware metrics and traces, and turn correctness signals into deterministic
exit codes. Competitive notes in [DESIGN.md §10.5](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#105-competitive-parity--differentiation-matrix)
are a dated design snapshot, not a current market census.

## Status

`v0.1.0` is the first version intended for public distribution. Treat it as a
candidate unless both the exact tag and its published GitHub Release exist and
GitHub reports immutable releases enabled; the source version alone is not
release evidence. The workflow blocks publication while that repository
setting is disabled. Creating either remains an explicit maintainer action governed by
[docs/RELEASE.md](docs/RELEASE.md). `v0.0.1` was internal-only and was never
tagged or released.

The `v0.1.0` line includes synchronized deadlock/race checks, fuzzer detection,
real session pools for sustained/pattern/ramp/spike workloads, resolver
pinning, trace record/replay, a composite Action hardened against shell
evaluation, and a versioned `metrics.v1.json` schema. Its experimental
`2026-07-28` implementation is pinned and reconciled to the final upstream
revision, but the supported and officially tested surface remains the narrow
tools/discovery/request-header subset described above.

## Development

```bash
git clone https://github.com/Teerapat-Vatpitak/mcp-loadtest
cd mcp-loadtest
bash scripts/ci-checks.sh        # or: pwsh scripts/ci-checks.ps1 on Windows
cargo nextest run --workspace --all-features
```

See [CONTRIBUTING.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/CONTRIBUTING.md) for project conventions before opening a PR.

## Documents

- [DESIGN.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md) — what this is + how it works (21 sections)
- [docs/examples/](https://github.com/Teerapat-Vatpitak/mcp-loadtest/tree/main/docs/examples/) — cookbook: CI integration, custom scenarios, debugging deadlocks
- [docs/adr/](https://github.com/Teerapat-Vatpitak/mcp-loadtest/tree/main/docs/adr/) — architecture decision records
- [CHANGELOG.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/CHANGELOG.md) — release history
- [CONTRIBUTING.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/CONTRIBUTING.md) · [CODE_OF_CONDUCT.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/CODE_OF_CONDUCT.md) · [SECURITY.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/SECURITY.md)

## License

Dual-licensed under [MIT](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/LICENSE-MIT) **OR** [Apache-2.0](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/LICENSE-APACHE), at your option.
