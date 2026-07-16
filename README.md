# mcp-loadtest

[![CI](https://github.com/Teerapat-Vatpitak/mcp-loadtest/actions/workflows/ci.yml/badge.svg)](https://github.com/Teerapat-Vatpitak/mcp-loadtest/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/rust-toolchain.toml)

> Load tester **and bug detector** for MCP (Model Context Protocol) servers. Catches lazy-init deadlocks, concurrency races, hangs, and perf regressions that unit tests miss.

## Why mcp-loadtest

Lazy-init inside an async worker thread is one of the easiest ways to ship a broken MCP server: `initialize` works, `tools/list` works, the first concurrent `tools/call` hangs forever. Unit tests never see it — the bug only surfaces when a real client drives the protocol end-to-end. The flagship example is [HKUDS/Vibe-Trading PR #85](https://github.com/HKUDS/Vibe-Trading/pull/85): a five-line fix that took hours of differential testing to find. `mcp-loadtest` finds it in seconds, and exits non-zero so it can fail your CI gate:

```bash
$ mcp-loadtest deadlock-probe --server "python -m vibe_trading_mcp" \
    --tool analyze_options --concurrent 5 \
    --args '{"spot":450,"strike":460,"expiry_days":30}'

Run 01KR9JX7E4P638TKQM96YA0B4Z
Status: FAIL (1 deadlock)
Deadlocks: 1   Hangs: 0   Errors: 0

Error: DEADLOCK DETECTED — 1 deadlock(s), 0 error(s), 0 threshold violation(s)
$ echo $?
1
```

The regression test that catches the exact Vibe-Trading commit lives at [`tests/vibe_trading_regression.rs`](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/crates/mcp-loadtest/tests/vibe_trading_regression.rs) (pinned to `71220c7c`, the parent of PR #85).

## What it does

- **Bug-class detection** — `deadlock_probe` (N sequential `tools/call` probes, each classified success / slow / deadlock; the lazy-init bug class hangs regardless of concurrency), `race_check` (diffs identical calls to surface non-determinism), and a hang-detector watchdog wrapping every call in every scenario.
- **Load testing** — `sustained` / `ramp` / `spike` / `soak` with p50/p95/p99/p999 histograms, cold-start handshake measurement, weighted pattern mixes, and periodic RSS sampling for leak hunting.
- **Record & replay** — `run --trace <file>` records every JSON-RPC frame as versioned JSONL (secret-looking arguments redacted by default); `replay <file> --server "..."` re-runs the trace against a fresh server and diffs responses, exiting non-zero on divergence ([ADR 0021](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/adr/0021-trace-record-replay.md)).
- **Reporting** — ANSI terminal, markdown, self-contained HTML, and schema-stable `metrics.json` ([docs/schema/metrics.v1.json](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/schema/metrics.v1.json), pinned by a conformance test). `compare` diffs two runs for regressions; `cross` runs N servers side by side.
- **AI-agent friendly** — structured errors with `Hint:` lines, and `mcp-loadtest serve --mcp` exposes the tool itself as an MCP server so Claude Code, Cursor, or any MCP-aware agent can call `deadlock_probe`, `sustained_load`, and `compare_runs` directly ([DESIGN.md §21.2](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#212-self-hosted-mcp-server-mcp-loadtest-serve---mcp)).

## CI gating

Every run resolves to a pass/fail and a non-zero exit code, so it drops straight into a pipeline:

- **One-line GitHub Action** — `uses: Teerapat-Vatpitak/mcp-loadtest@v0.0.1` installs a sha256-verified prebuilt release binary, runs `deadlock-probe`/`run`/`cross`/`doctor`, optionally gates against a baseline `metrics.json`, and appends a results section to the job summary. Quick-start workflow: [docs/examples/ci-integration.md](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/examples/ci-integration.md).
- **Threshold gating** — `[thresholds]`: p50–p999 latency, error rate, memory growth, RSS leak slope, per-tool SLOs. Any breach → non-zero exit. Deadlocks are zero-tolerance.
- **Baseline regression diff** — `mcp-loadtest compare baseline.json current.json` flags p99 / error-rate / deadlock regressions, with configurable budgets (`--max-p99-regression-pct`, `--max-error-rate-regression-pp`) also exposed as `compare_runs` MCP tool args (ADR 0009).
- **Protocol-aware strict mode** — `[validation] strict = true` validates every call's arguments against the server's advertised `inputSchema` before the call (a mismatch gates the run) and each result's `structuredContent` against its `outputSchema` (warn-only, never gates — ADR 0010). Off by default.

## Quick start

```bash
# Install from the public repo (not on crates.io yet — see docs/adr/0015)
cargo install --git https://github.com/Teerapat-Vatpitak/mcp-loadtest mcp-loadtest-cli
# ...or download a prebuilt Linux/macOS/Windows binary:
#   https://github.com/Teerapat-Vatpitak/mcp-loadtest/releases

# Quick deadlock smoke against a real MCP server
mcp-loadtest deadlock-probe --server "python -m my_mcp" --tool foo

# Sustained load from a config file (print a starter with `example-config`)
mcp-loadtest run --config bench.toml

# Compare two runs (e.g. main vs PR branch)
mcp-loadtest compare runs/baseline/metrics.json runs/current/metrics.json
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

Library usage: the crate is a normal Rust library — see [`tests/vibe_trading_regression.rs`](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/crates/mcp-loadtest/tests/vibe_trading_regression.rs) for a runnable `Scenario::drive` example.

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

Each scenario is one `impl Scenario` in [`crates/engine/src/scenario/`](https://github.com/Teerapat-Vatpitak/mcp-loadtest/tree/main/crates/engine/src/scenario/) with a JSON-Schema describing its config block ([DESIGN.md §8](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#8-built-in-scenarios)).

## vs `reaatech/mcp-load-test`

[`reaatech/mcp-load-test`](https://github.com/reaatech/mcp-load-test) is the only other MCP load tester we're aware of — a TypeScript monorepo that covers the load-testing basics well. `mcp-loadtest` is built on a different axis: Rust performance plus a single static binary, and a bug-detector layer it doesn't have — deadlock and race detection, a protocol fuzzer, server resource sampling over time, per-tool SLOs, strict schema gating, real-time TUI, HTML reports, WebSocket transport, and self-hosting as an MCP server. Full feature matrix against 4 competitors and 6 adjacent frameworks: [DESIGN.md §10.5](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#105-competitive-parity--differentiation-matrix); positioning: [ADR 0004](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/adr/0004-compete-with-reaatech.md).

## Status

v0.0.1 is the first release (2026-07-11): the GitHub Release ships prebuilt Linux/macOS/Windows binaries, and `cargo install --git` works today (crates.io deferred — [ADR 0015](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/docs/adr/0015-defer-crates-io-distribution.md)). Highlights: deadlock/race/fuzzer bug detection, a real session pool for `sustained`/`ramp`/`spike` (ADR 0017), DNS-rebinding resolver pinning (ADR 0016), multi-version protocol support including the stateless 2026-07-28 mode (ADR 0018/0019), trace record & replay (ADR 0021), the composite GitHub Action, and the published `metrics.v1.json` schema. The remaining P3 backlog is in [DESIGN.md §13.1](https://github.com/Teerapat-Vatpitak/mcp-loadtest/blob/main/DESIGN.md#131-committed-backlog).

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
