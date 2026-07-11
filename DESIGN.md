# mcp-loadtest — Design Document

**Status:** 0.0.1 (2026-07-11)
**Author:** Teerapat Vatpitak
**Reviewers:** _(pending)_

---

## 1. Motivation

The Model Context Protocol ecosystem is exploding — new MCP servers ship weekly across Python, Node, Rust. But **MCP servers fail in ways unit tests don't catch**:

- Lazy-init deadlocks inside async worker threads
- Race conditions when concurrent `tools/call` arrive before `tools/list` completes
- Memory leaks under sustained load
- Hangs that look like work-in-progress
- Subtle protocol violations only visible at scale

### Canonical motivating case

The author hit a deadlock in [HKUDS/Vibe-Trading](https://github.com/HKUDS/Vibe-Trading) where:

- `initialize` → ✅ worked
- `tools/list` → ✅ worked
- `tools/call` → ❌ hung forever

Root cause: `_get_registry()` lazy-init inside the FastMCP asyncio worker thread, blocking on `import src.tools.shell.*`. Standard pytest didn't catch this — the bug only surfaces when a real client opens a session and calls a tool through stdio.

The fix took ~5 lines (PR [#85](https://github.com/HKUDS/Vibe-Trading/pull/85)) and a regression smoke test (PR [#86](https://github.com/HKUDS/Vibe-Trading/pull/86)) — but **finding** the bug took hours of differential testing because no purpose-built tool exists for stress-testing MCP servers.

### The gap

There are excellent tools for HTTP load testing (k6, vegeta, wrk2), gRPC (ghz), GraphQL (autocannon plugins). **There is nothing for MCP.** Ad-hoc Python scripts and pytest fixtures are what people use.

`mcp-loadtest` aims to be the canonical tool: language-agnostic, transport-agnostic, with built-in scenarios for the bug classes that actually occur.

---

## 2. Goals & Non-Goals

### Goals

- Detect deadlocks, hangs, livelocks under realistic concurrent load
- Measure latency (p50/p95/p99), throughput, error rate
- Work against any MCP server regardless of language / transport (stdio, HTTP, SSE, WebSocket)
- Library mode (Rust crate) for embedding in CI tests
- CLI mode for ad-hoc smoke tests and benchmarks
- Cross-platform (Linux, macOS, Windows — author runs Windows so this is a hard requirement, not aspirational)
- Zero-config quick-start: `mcp-loadtest probe -s "python -m my_mcp"` should just work

### Non-Goals

- **Not a replacement for unit tests.** Different problem.
- **Not a tool for testing MCP clients.** Client-side bugs are a separate domain.
- **Not validating tool output correctness.** We test protocol-level behavior. If your tool returns wrong data, that's not what we catch.
- **Not a fuzzer.** A protocol fuzzer (random/malformed payloads) is a different design — possibly a future sister project (`mcp-fuzz`).
- **Not a benchmark suite.** We provide infrastructure to bench, not a curated set of "official" benchmarks.

---

## 3. Background

### MCP protocol (relevant subset)

JSON-RPC 2.0 framing over one of four transports:

- **stdio** — line-delimited JSON over child process stdin/stdout (most common, all examples in this doc focus here)
- **HTTP** — Streamable HTTP (simple JSON variant); request via POST, simple JSON response
- **HTTP+SSE** — request via POST, server pushes events via SSE channel
- **WebSocket** — bidirectional frames

Lifecycle (stdio):

```
client → server   {"method":"initialize", "params":{...}}
client ← server   {"result":{"protocolVersion":...,"capabilities":{...}}}
client → server   {"method":"notifications/initialized"}    # one-way notif
client → server   {"method":"tools/list"}
client ← server   {"result":{"tools":[{...},...]}}
client → server   {"method":"tools/call", "params":{"name":"X","arguments":{...}}}
client ← server   {"result":{"content":[{...}]}}
```

### Bug classes we target

| Class                     | Example                                         | Why hard to catch in unit tests                             |
| ------------------------- | ----------------------------------------------- | ----------------------------------------------------------- |
| Lazy-init deadlock        | Vibe-Trading PR #85                             | Bug only surfaces with full subprocess + protocol handshake |
| Concurrent tool-call race | tools/call before tools/list completes          | Need real concurrency; mocked async ≠ real async            |
| Resource exhaustion       | 1000 concurrent calls → fd / mem leak           | Need sustained load                                         |
| Slow-tool head-of-line    | One slow tool blocks queue                      | Need mixed workload                                         |
| Reconnect / mid-call kill | Connection drops between request and response   | Hard to simulate without tooling                            |
| Notification ordering     | Server sends `notifications/cancelled` mid-call | Need sequence-aware client                                  |

---

## 4. Architecture

### High-level

```
┌─────────────────────────────────────────────────────────────┐
│                       CLI / Library                          │
│  - parse args / TOML config (Config)                         │
│  - build_scenario: "type" string → Box<dyn Scenario>         │
│  - render Report via Reporters (emit)                        │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│                 Run (orchestrator, run.rs)                   │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ProcessSampler│  │   Scenario   │  │    Reporters     │  │
│  │ sysinfo RSS/ │  │ drive(&mut   │  │ markdown / json  │  │
│  │ CPU/fd/thread│  │ Session,&ctx)│  │ html / terminal  │  │
│  └──────┬───────┘  └──────┬───────┘  └──────────────────┘  │
│         │                  │                                 │
│         └──────┬───────────┘                                 │
│                ▼                                             │
│  ┌─────────────────────────────────┐                        │
│  │      Session (session.rs)       │                        │
│  │   JSON-RPC id + handshake       │                        │
│  │   list_tools/call_tool/shutdown │                        │
│  │   hang_detect per-call watchdog │                        │
│  └────────────────┬────────────────┘                        │
│                   ▼                                          │
│  ┌─────────────────────────────────┐                        │
│  │ Transport (stdio / http / sse / │                        │
│  │ ws — trait, Box<dyn Transport>) │                        │
│  └─────────────────────────────────┘                        │
└─────────────────────────────────────────────────────────────┘
                  │
                  ▼
        ┌─────────────────────┐
        │  Server under test  │  (stdio child process, or an
        └─────────────────────┘   http/sse/ws endpoint — any language)
```

### Module layout (`crates/`)

The workspace is six layered crates (ADR 0022; strictly downward
dependencies).

```
crates/
├── core/       (mcp-loadtest-core)     # pure data — imports nothing from the workspace
│   config/ (validate.rs = KNOWN_SCENARIOS / KNOWN_TRANSPORTS), metrics/ (+ Recorder),
│   report.rs (Report + Reporter + format_iso8601_utc), outcome.rs (ScenarioOutcome),
│   coverage.rs, fuzz_report.rs, trace/format.rs (on-disk mcp-trace/1), version.rs (ProtocolVersion)
├── protocol/   (mcp-loadtest-protocol) # wire — imports core
│   jsonrpc.rs, mcp.rs, schema.rs (strict validator, ADR 0010), session/,
│   hang_detector.rs, factory.rs (SessionFactory),
│   transport/ (Transport trait; stdio/, http, sse, ws; guard.rs SSRF ADR 0012; spawn_options ADR 0013)
├── engine/     (mcp-loadtest-engine)   # scenarios + run — imports core + protocol
│   run/ (executor spawn → drive → report; connect; thresholds; factory shim),
│   process.rs (sysinfo sampler), trace/ (writer + replay runtime), breaking_point.rs, race_detector.rs,
│   scenario/ (Scenario trait, RunContext; sustained/deadlock_probe/cold_start/version_matrix/
│              ramp/soak/spike/race_check/pool/pattern/fuzzer)
├── output/     (mcp-loadtest-output)   # renderers + TUI — imports core
│   report/ (markdown, json, html (+ chart/css), terminal), grading.rs, regression.rs,
│   tui/ (live dashboard, cargo feature `tui`)
├── mcp-loadtest/  (facade)             # thin re-export surface over the four crates above
│   lib.rs (public API re-exports), per-module shims, serve/ (cargo feature `serve` — the only real code)
└── mcp-loadtest-cli/                   # binary (clap)
    main.rs / dispatch.rs / lib.rs, cmd_run.rs (+ cmd_run/builder.rs = build_scenario registry),
    cmd_deadlock / cmd_replay / cmd_compare / cmd_cross / cmd_doctor/, emit / explain / hints
```

There is no `server_manager.rs` and no `driver.rs` — spawning lives in
`StdioTransport` + `SpawnOptions` (protocol), orchestration in `run/` (engine),
and scenarios drive a single `Session` directly (the multi-session pool is M8+
backlog, §13.1).

### Key crate dependencies

| Crate              | Why                              |
| ------------------ | -------------------------------- |
| tokio              | async runtime                    |
| serde / serde_json | JSON-RPC payloads                |
| clap               | CLI                              |
| toml               | config                           |
| hdrhistogram       | percentile latency               |
| sysinfo            | RSS/CPU per pid (cross-platform) |
| indicatif          | terminal progress                |
| tracing            | structured logging               |
| thiserror / anyhow | errors                           |
| tokio-util         | LinesCodec for stdio framing     |

No proc-macro magic. No "framework" — just composable structs.

---

## 5. Library API (Rust crate)

```rust
use std::path::PathBuf;
use std::time::Duration;

use mcp_loadtest::{Config, Run};
use mcp_loadtest::scenario::sustained::Sustained;
use serde_json::json;

#[tokio::test]
async fn no_deadlock_under_sustained_load() {
    // Server + thresholds parse from the same TOML the CLI uses
    // (or build programmatically: Config::new + with_* builders).
    let config = Config::from_toml_str(
        r#"
        [server]
        command = "python"
        args = ["-m", "vibe_trading_mcp"]
        env.LOG_LEVEL = "warn"

        [scenario]
        type = "sustained"
        tool = "get_market_data"

        [thresholds]
        p99_latency = "500ms"
        error_rate = 0.01
        hang_timeout = "5s"
        "#,
    )
    .expect("config");

    // Scenarios are concrete structs implementing `Scenario`.
    let scenario = Box::new(Sustained {
        concurrent: 20,
        duration: Duration::from_secs(30),
        tool: "get_market_data".to_string(),
        args: json!({ "ticker": "AAPL" }),
    });

    let report = Run::new(config, scenario, PathBuf::from("./runs"))
        .execute()
        .await
        .expect("run failed");

    assert!(report.passed(), "thresholds violated: {report:?}");
    assert_eq!(report.scenario_outcome.deadlock_count, 0);
}
```

Design choices:

- **Config-first** — server/thresholds come from the same TOML schema the CLI uses (`Config::from_toml_str` / `Config::from_file`, or `Config::new` + `with_*` builders); one schema, two front-ends
- **Scenarios are plain structs** — `Run::new` takes a `Box<dyn Scenario>`; no enum to extend, no builder DSL
- **`execute()` returns `Result<Report, RunError>`** — lets tests pattern-match on metrics
- **Report exposes structured snapshots** (`ScenarioMetrics`, `ProcessStats`, `ScenarioOutcome`) — can drive custom assertions
- **No global state** — multiple `Run`s in parallel inside one test process is supported

---

## 6. CLI surface

```bash
# Quick health check (initialize + tools/list + 1 call per tool)
mcp-loadtest probe --server "python -m my_mcp"

# Targeted smoke for the Vibe-Trading bug class
mcp-loadtest deadlock-probe --server "python -m my_mcp" --tool get_market_data

# Run a custom scenario from CLI
mcp-loadtest run \
  --server "python -m my_mcp" \
  --scenario sustained \
  --concurrent 50 --duration 60s \
  --tool get_market_data --args '{"ticker":"AAPL"}'

# Run from config file
mcp-loadtest run --config bench.toml

# Re-render saved run
mcp-loadtest report ./runs/2026-05-10T14-30-00/

# List built-in scenarios
mcp-loadtest list-scenarios

# Print example config
mcp-loadtest example-config > bench.toml
```

Output structure for each run (dirs are named by ULID, not timestamp):

```
runs/01HXY.../
├── report.md            # human-readable summary       (format "markdown")
├── metrics.json         # aggregated metrics            (format "json";
│                        #   schema: docs/schema/metrics.v1.json — §17.2)
├── report.html          # self-contained HTML           (format "html")
└── server-stderr.log    # captured server stderr — only with
                         #   --capture-stderr / --tee-stderr (ADR 0013)
```

`trace.jsonl` is written only when recording with `run --trace <file>`
(§17.1 — `Report.trace_path` is `None` otherwise). Deferred (not written):
the `config.toml` echo, `server.stdout.log`, and a separate `summary.json`
(`metrics.json` already carries the CI pass/fail fields).

---

## 7. Configuration schema (TOML)

```toml
[server]
command = "python"
args = ["-m", "my_mcp"]
env.LOG_LEVEL = "warn"
working_dir = "/path/to/proj"
transport = "stdio"           # stdio | http | sse | ws
startup_timeout = "10s"       # how long to wait for initialize response

[scenario]
type = "sustained"            # cold_start | sustained | spike | ramp | soak | pattern | deadlock_probe | race_check | fuzzer | version_matrix
duration = "60s"
concurrent = 50

# scenario-specific knobs
ramp_from = 1                 # ramp only
ramp_to = 100                 # ramp only
spike_at = "30s"              # spike only
spike_multiplier = 10         # spike only
leak_check_interval = "10s"   # leak only

# What to call. Multiple entries → weighted random selection.
[[scenario.tool_call]]
name = "get_market_data"
args = { ticker = "AAPL" }
weight = 1.0

[[scenario.tool_call]]
name = "analyze_options"
args = { ticker = "SPY", expiry = "2026-06-19" }
weight = 0.3

[thresholds]
p50_latency = "100ms"
p99_latency = "500ms"
error_rate = 0.01             # fraction; 0.01 = 1%
hang_timeout = "5s"           # call considered hung if no response in this long
memory_growth_mb = 50         # fail if RSS grows by more than this MB during run

[output]
report_dir = "./runs"
formats = ["markdown", "json", "terminal"]
```

---

## 8. Built-in scenarios

| Scenario         | Description                                                                                                                                                                                                                                                     | Detects                                                       |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| `cold_start`     | Spawn → initialize → first tool call. Repeats N times via `RunContext::session_factory`, respawning a fresh server per iteration; handshake time recorded under `cold_start:handshake`.                                                                          | regression in startup time, init-time deadlocks               |
| `sustained`      | Constant load for a fixed duration; `concurrent > 1` drives a real session pool (ADR 0017; sequential single-session fallback without a factory). Drives the multi-step weighted-random `pattern` engine internally (pattern form still sequential). | baseline p99 latency, throughput, sustained error rate        |
| `spike`          | Baseline → sharp burst at peak concurrency for a fixed window → cooldown back to baseline. Models Black-Friday-style traffic spikes. Each phase is a real session pool (sequential iterations-per-tick fallback without a session factory).          | queue overflow, recovery behavior, fairness under burst       |
| `ramp`           | Step concurrency from `from` to `to` by `step_increment`, optionally feeding the per-step metrics into [`analysis::breaking_point`]. Each step is a real session pool (sequential iterations-per-step fallback without a session factory).           | finds break-point — concurrency where p99 explodes            |
| `soak`           | Long-duration steady load with periodic snapshots; pairs with `analysis::regression` for latency-drift and (via `ProcessSampler`) RSS-slope leak signals.                                                                                                       | memory leaks, latency drift, throughput collapse over hours   |
| `pattern`        | Multi-step weighted-random tool-call sequences with per-pattern `think_time` and `ErrorBehavior`. Building block used directly by `sustained`.                                                                                                                  | realistic mixed workloads (explore-then-act, read-then-write) |
| `deadlock_probe` | initialize → tools/list → fire N `tools/call` to same tool wrapped in `hang_detect`. Bails on first deadlock to avoid flooding a wedged session.                                                                                                                | the **Vibe-Trading bug class** specifically                   |
| `race_check`     | Issue N identical `tools/call` and run the responses through `analysis::race_detector` (key-sorted JSON canonicalization).                                                                                                                                      | non-determinism / divergent responses to identical inputs     |
| `fuzzer`         | Cycle through enumerated malformed-but-plausible payloads (unknown method, numeric method, giant payload, control chars, deep-nested, null/string params); classify each via `analysis::fuzz_report`.                                                           | parser bugs, type-confusion in method dispatch                |
| `version_matrix` | Drive the same server once per MCP protocol revision (fresh session per revision via `SessionFactory::with_version`, ADR 0018); each revision's `hang_detect`-wrapped calls record under the per-tool key `version:<rev>`, so the report diffs revisions side by side. | revision-specific deadlocks, errors, latency deltas           |

Deferred:

- `slow_mix` — 80% calls to a fast tool, 20% to a deliberately-slow tool (head-of-line blocking, fairness). Approximable today by configuring a multi-step `pattern` with weighted tools.
- `reconnect` — drop session mid-call (close stdin), spawn new session, retry (resilience, leftover state, zombies). Needs the session pool that lands in M8+.

Each scenario is a concrete struct implementing the `Scenario` trait
(`crates/engine/src/scenario/mod.rs`):

```rust
#[async_trait]
pub trait Scenario: Send + Sync {
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome;
    fn config_schema(&self) -> Value;
    fn name(&self) -> &'static str;
}
```

There is no `SessionPool` — `drive` gets one `&mut Session`, so calls against
that session run sequentially (pooled scenarios spawn extra sessions via
`RunContext::session_factory` — ADR 0017). Scenario selection is by the config `type` string: a match arm in
`crates/mcp-loadtest-cli/src/cmd_run/builder.rs` (`build_scenario`) plus an
entry in `KNOWN_SCENARIOS` (`crates/core/src/config/validate.rs`).
See §14.2 for the full shipped types.

---

## 9. Test matrix

### Layer A — does mcp-loadtest itself work?

Mock MCP servers in `tests/fixtures/`. Each is a tiny Python script (chosen for ubiquity, not Rust, to make the test environment realistic).

| Mock                | Behavior                                                | Tests                                 |
| ------------------- | ------------------------------------------------------- | ------------------------------------- |
| `mock-normal.py`    | Echoes args, responds in 1ms                            | happy-path metrics shape              |
| `mock-slow.py`      | Tool sleeps 2s                                          | latency histogram correctness         |
| `mock-broken.py`    | Hangs on first tools/call (replicates Vibe-Trading bug) | `deadlock_probe` correctly classifies |
| `mock-crash.py`     | Panics on 1% of calls                                   | error-rate accuracy                   |
| `mock-leak.py`      | Allocates 10 KB/call, never frees                       | `leak` scenario detects               |
| `mock-error.py`     | Returns JSON-RPC errors per spec                        | error classification                  |
| `mock-slow-init.py` | Takes 5s to respond to `initialize`                     | `cold_start` measures correctly       |
| `mock-malformed.py` | Returns invalid JSON occasionally                       | parser robustness                     |

Test invariant: for each (scenario × mock) pair, the report's machine-readable summary contains expected fields with expected ranges. This is the bulk of integration tests.

### Layer B — does it catch real bugs?

Snapshot test against a known-buggy commit of Vibe-Trading:

- Pin to commit `~PR-85` (just before the fix)
- Run `deadlock_probe` scenario
- Assert: report flags ≥1 deadlock, identifies `tools/call` as the offending request

Re-run against post-fix commit:

- Same scenario, expect 0 deadlocks

This is the killer demo. It goes in the README.

### Layer C — cross-platform

CI matrix: ubuntu-latest, macos-latest, windows-latest × stable Rust × Python 3.13 (for fixtures).

---

## 10. Milestones (revised 2026-05-10 — head-on competition with reaatech/mcp-load-test)

Original 3-week plan replaced after discovering [reaatech/mcp-load-test](https://github.com/reaatech/mcp-load-test) already ships the load-testing basics (see §10.5 for parity matrix). The first release of mcp-loadtest must reach feature parity _and_ surface our differentiators before publishing.

The repo stays private through the M1-M7 development phase. 0.0.1 ships from a **public** repo via `cargo install --git` + prebuilt GitHub Release binaries; the crates.io publish is deferred to keep the first release off append-only (ADR 0015 — amends the distribution channel of ADR 0004).

M1 through M7 are all shipped. Post-M7 work (spike scenario, HTML reporter, WebSocket transport, hot-path zero-copy refactor, criterion benches) is folded into the first release rather than coined as a new milestone — the work is small + cohesive enough that bundling it makes more sense than a standalone "M8". The "Week N" column is dropped because milestones are no longer time-boxed — they're done.

| M             | Theme                            | Key deliverables                                                                                                                                                                                                                                  |
| ------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **M1** ✓      | stdio Session                    | `Session::spawn` → handshake → `list_tools`/`call_tool`/`shutdown`; mock-normal.py; happy-path integration test                                                                                                                                   |
| **M2** ✓      | Scenarios + metrics core         | `Scenario` trait; `cold_start` + `sustained` + `deadlock_probe` impls; `hang_detector` (§15.1); hdrhistogram metrics; mocks `mock-broken`/`mock-slow`/`mock-crash` + tests                                                                        |
| **M3** ✓      | Reports + first internal release | TOML config; markdown / JSON / console reporters; sysinfo-based process sampling; **regression test against real Vibe-Trading commit ~PR-85**                                                                                                     |
| **M4** ✓      | Transport parity                 | HTTP transport (StreamableHTTP); SSE transport; HTTP/SSE fixtures; transport-aware concurrency profiles                                                                                                                                           |
| **M5** ✓      | Analysis parity                  | `breaking_point` detection; performance grading (A-F per latency/concurrency/error); realistic patterns (explore-then-act, read-then-write, multi-step) with weighted random + think-time; `soak` scenario polish; `compare-baselines` subcommand |
| **M6** ✓      | Differentiators v1               | Real-time terminal TUI dashboard (live latency/throughput/RSS); server resource sampling beyond RSS (CPU, fd, threads); `race_detector` scenario; cross-server compare (`run --server srv-a --server srv-b`)                                      |
| **M7** ✓      | Differentiators v2 + release polish | Protocol fuzzer (basic — random/malformed payloads); coverage tracking (tools registered vs. exercised); per-tool SLO assertions; README rewrite with competitive positioning; `cargo install` smoke test on all 3 OS                             |
| **Post-M7** ✓ | Pre-public-release close-out     | Spike scenario; HTML reporter; WebSocket transport; hot-path zero-copy refactor; criterion benches (DESIGN §19 claims now reproducible). See CHANGELOG.                                                                            |
| **v0.0.1**    | First public release             | repo flips **public**; `cargo install --git` + GitHub Release binaries (crates.io deferred — ADR 0015); HN/lobste.rs/r/rust announce                                                                                                              |
| _M8+ stretch_ | Beyond                           | AI-assisted pattern generator; distributed mode (multi-worker); replay/record; PyO3 binding                                                                                                                                                       |

**Definition of done for 0.0.1:**

- `cargo install --git <repo-url> mcp-loadtest-cli` works on Linux/macOS/Windows, and prebuilt binaries are attached to the GitHub Release (crates.io publish deferred — ADR 0015).
- `mcp-loadtest deadlock-probe -s "python -m vibe_trading_mcp"` reproduces the original bug on commit `~PR-85`.
- All §10.5 parity-must-have rows are checked.
- All §10.5 differentiator rows are checked.
- README has side-by-side comparison table vs. reaatech, citing concrete benchmarks.

---

## 10.5 Competitive parity & differentiation matrix

reaatech/mcp-load-test as of 2026-05-10 (TS monorepo, 77 source files, ~50% of README claims fleshed out per file-size sampling).

### Parity — features they have, we must match before publishing

| Feature                                           | reaatech | mcp-loadtest target | Milestone |
| ------------------------------------------------- | -------- | ------------------- | --------- |
| stdio transport                                   | ✓        | ✓                   | M1        |
| HTTP (StreamableHTTP) transport                   | ✓        | ✓                   | M4        |
| SSE transport                                     | ✓        | ✓                   | M4        |
| WebSocket transport                               | ✗        | ✓                   | Post-M7   |
| Latency histograms p50/p95/p99/p999 per tool      | ✓        | ✓                   | M2        |
| Breaking point detection                          | ✓        | ✓                   | M5        |
| Performance grading A-F                           | ✓        | ✓                   | M5        |
| Soak / leak detection                             | ✓        | ✓                   | M5        |
| Spike scenario                                    | ✓        | ✓                   | Post-M7   |
| Compare baselines                                 | ✓        | ✓                   | M5        |
| Realistic patterns (explore-then-act, multi-step) | ✓        | ✓                   | M5        |
| Console + markdown + JSON reporters               | ✓        | ✓                   | M3        |
| HTML reporter (self-contained)                    | ✗        | ✓                   | Post-M7   |
| Programmatic library API                          | ✓        | ✓                   | M2/M3     |

### Differentiators — features we have/will have that they don't

| Feature                                                     | reaatech                             | mcp-loadtest            | Why it matters                                                                                                                                                                         |
| ----------------------------------------------------------- | ------------------------------------ | ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Deadlock detection (`deadlock_probe`)**                   | ✗                                    | ✓ M2                    | Lazy-init / async-worker bugs that break in prod. Direct response to Vibe-Trading PR #85.                                                                                              |
| **Race detector**                                           | ✗                                    | ✓ M6                    | Order-sensitive concurrent tool calls; finds protocol-level race bugs.                                                                                                                 |
| **Real-time TUI dashboard**                                 | ✗ (post-hoc only)                    | ✓ M6                    | Watch perf cliff happen live during a run.                                                                                                                                             |
| **Cross-server compare** (run vs N targets)                 | partial (compare baselines = 2 runs) | ✓ M6 (1 run, N targets) | Side-by-side: vendor A vs vendor B vs your fork.                                                                                                                                       |
| **Server resource sampling** (CPU/fd/threads/RSS over time) | ✗ (latency only)                     | ✓ M6                    | Find resource exhaustion before throughput collapses.                                                                                                                                  |
| **Protocol fuzzer (mcp-fuzz integrated)**                   | ✗                                    | ✓ M7                    | Random/malformed payloads; finds parser bugs unit tests miss.                                                                                                                          |
| **Coverage tracking** (registered vs exercised tools)       | ✗                                    | ✓ M7                    | Catch silently-broken tools that nobody tests in CI.                                                                                                                                   |
| **Per-tool SLO assertions**                                 | partial (global)                     | ✓ M7                    | Per-tool latency/error budgets in CI.                                                                                                                                                  |
| **Configurable regression thresholds**                      | ✗ (fixed)                            | ✓ 0.0.1                 | `compare` CLI flags + `compare_runs` MCP args override p99 / error-rate / deadlock policy; defaults unchanged (ADR 0009).                                                              |
| **Protocol-aware assertions**                               | ✗                                    | ✓ 0.0.1                 | Opt-in strict mode validates `tools/call` args vs the server's advertised `inputSchema`; mismatch → `ProtocolError` gates the run. Forward-compatible, off by default (ADR 0005/0010). |
| **Rust perf** + static binary                               | ✗ (Node runtime required)            | ✓                       | `cargo install` → single ~5MB binary; no Node toolchain.                                                                                                                               |
| **AI-assisted pattern generator**                           | ✗                                    | ⏳ M8 stretch           | LLM reads tool schemas → generates realistic call sequences.                                                                                                                           |
| **Distributed mode**                                        | ✗                                    | ⏳ M8 stretch           | Multiple workers driving one server (high-RPS targets).                                                                                                                                |
| **Replay / record**                                         | ✗                                    | ⏳ M8 stretch           | Capture prod traffic, replay deterministically.                                                                                                                                        |
| **Self-hosted as MCP server** (`mcp-loadtest serve --mcp`)  | ✗                                    | ✓ M7                    | AI agents (Claude, Cursor, etc.) call `deadlock_probe` / `compare` / `report` directly via MCP. Recursive: load-test an MCP using an MCP.                                              |

### Strategic positioning (for README at 0.0.1)

> mcp-loadtest is a **load tester + bug detector** for MCP servers. Match-or-exceed reaatech/mcp-load-test on every load-testing dimension, and **detect classes of bugs no other tool finds**: deadlocks, races, resource leaks, coverage gaps.

The README at publish must lead with the deadlock demo (replicated Vibe-Trading PR #85 bug, caught in 2 seconds) — not the load-testing checklist. Differentiation first; parity proves we're serious.

---

## 11. Decisions (resolved 2026-05-10)

| #   | Question                | Decision                                                                                                       | Rationale                                                                                                               |
| --- | ----------------------- | -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| 1   | Crate name              | **`mcp-loadtest`** (lib) + **`mcp-loadtest-cli`** (bin)                                                        | descriptive, discoverable, doesn't pigeonhole to "bench"                                                                |
| 2   | License                 | **MIT OR Apache-2.0** (dual)                                                                                   | Rust ecosystem standard; MIT for individuals, Apache-2.0 for corporate patent grant                                     |
| 3   | Repo location           | **`github.com/Teerapat-Vatpitak/mcp-loadtest`**                                                                | personal handle for now; transfer to `mcp-tools/` org if/when sister projects emerge                                   |
| 4   | MCP protocol versioning | pin to spec v1.x initially, warn on mismatch; `--strict-protocol` flag for fail-on-mismatch; detect-and-adapt later | ship fast, add complexity when justified                                                                                |
| 5   | `deadlock_probe`        | **both** subcommand (`mcp-loadtest deadlock-probe -s "..."`) and scenario in `run --scenario deadlock_probe`   | subcommand for newcomer UX, scenario for CI; near-zero implementation cost                                              |
| 6   | Server stderr           | **always capture** to `runs/<id>/server.stderr.log`; opt-in `--tee-stderr` to also stream                      | stderr critical for debugging; capture is cheap; tee opt-in to avoid CI log spam                                        |
| 7   | Diff-vs-baseline mode   | **defer to M5 stretch**                                                                                        | the first cut emits JSON; users diff externally. Proper baseline storage + regression detection has too many edge cases for a first cut |
| 8   | Library API → 1.0       | When **all three**: 3 months no breaking changes + 5+ external users + 1 real bug caught in wild               | calendar time + adoption + value-prop validation, all required                                                          |

---

## 12. Naming options (decide in §11.1)

- `mcp-loadtest` — clear, no surprises
- `mcp-bench` — implies benchmarking specifically
- `mcphammer` — playful, memorable, but maybe too aggressive for a tool that aims to be canonical
- `mcptest` — too generic
- `mcp-stress` — accurate but slightly negative
- `lockesmith` — clever ("lock-finder for MCP servers") but obscure

Author's preference: **`mcp-loadtest`** for now. Rename later if needed.

---

## 13. Future work (out of scope for 0.0.1)

- **`mcp-fuzz`** — sister project for protocol fuzzing (random/malformed payloads)
- **`mcp-trace`** — record + replay tool for debugging production MCP issues
- **Distributed mode** — multiple loadtest workers driving one server (for very high RPS targets)
- **GUI/web UI** — render reports interactively
- **Plugin system** — user-defined scenarios as separate crates
- **Public benchmark dataset** — track perf of popular MCP servers over time (`mcp-leaderboard`)

### 13.1 Committed backlog

Prioritized. Each item is a debt the first cut explicitly took on; provenance in parentheses
so a future planner can trace the contract. The bullets above remain the broader ecosystem horizon.

**P1 — correctness / security debt taken on early**

1. ~~**`cold_start` real handshake-time histogram**~~ — **done**: `RunContext`
   gained a `SessionFactory`; `cold_start` respawns a fresh server per iteration, records
   the spawn→`initialize` handshake under `cold_start:handshake`, and the placeholder pin
   test was replaced with measured-latency coverage. (DESIGN §8)
2. ~~**Result-side strict schema validation**~~ — **done**: each successful
   `tools/call` result's `structuredContent` is validated against the tool's advertised
   `outputSchema`; non-gating Warn policy per `classify_schema_violation`. (ADR 0010)
3. ~~**DNS-rebinding defense (resolver-pinning connector)**~~ — **done (ADR 0016)**: hostnames are resolved once, every resolved address is vetted against the
   blocklist, and the vetted addresses are pinned for the actual connection on http/sse/ws.
   (ADR 0012 "Open" → closed by ADR 0016)

**P2 — API / packaging hygiene**

4. ~~**Remove deprecated alias `DEFAULT_LEAK_THRESHOLD_MB_PER_SEC`**~~ — **done**:
   removed per its deprecation notice; the constant had already been demoted to `pub(crate)`,
   so no external API impact.
5. ~~**Feature-gate `serve` / `tui` behind cargo features**~~ — **already shipped**
   (`9eb1f14`, `[features] default = []` / `serve` / `tui`); this entry was stale
   at writing.

**P3 — differentiators / ecosystem (longer horizon)**

6. ~~**Fuzzer raw-byte payloads**~~ — **done (plan T3.1)**: additive
   `Transport::raw_send` (stdio writes verbatim bytes + newline; other transports
   inherit an unsupported default), fuzzer exercises the raw variants with
   poisoned-session respawn via `SessionFactory` when a factory is attached, and
   keeps the honest skip without one. (CHANGELOG — fuzzer)
7. ~~**`insta` snapshot parity for html / terminal reporters**~~ — **done (plan
   T3.2)**: the shared fixture was already deterministic; real
   snapshots (pass/fail/empty × html/terminal) replaced the landmark tests.
8. **Sister projects** — largely absorbed instead of spun out: `mcp-trace`
   shipped as the in-lib `trace` module + `run --trace`/`replay` CLI (plan
   T3.3, ADR 0021 — dependency-cycle rationale); `mcp-fuzz`'s scope is covered
   by the fuzzer's raw-byte variants (plan T3.1). Spinning either out stays
   possible later. (ADR 0004 Path C)
9. **M8+ stretch** — distributed multi-worker, PyO3 binding, AI-assisted pattern generator
   (see §13 list above). (ADR 0004 Path C)

---

## 14. Concrete Rust types

The public types as shipped, verified against the code. The types
live in the layer crates (ADR 0022) — `config`/`metrics`/`report`/`ProtocolVersion`
in `mcp-loadtest-core`, `Session`/transports in `mcp-loadtest-protocol`,
`Run`/scenarios in `mcp-loadtest-engine` — and are all re-exported through the
`mcp-loadtest` facade, so the `mcp_loadtest::...` paths below still resolve. The
pre-implementation
sketch this section used to hold — a `Server` struct, a `Transport` enum, a
closed `ScenarioKind` enum, a `SessionPool` — never shipped; see the
historical note at the end of §14.2.

### 14.1 Server config

There is no `Server` type. The server-under-test is the `[server]` TOML block
(`config.rs`):

```rust
// #[non_exhaustive]; built by Config::from_toml_str / Config::from_file
// or programmatically via ServerConfig::stdio(command, args).
pub struct ServerConfig {
    pub command: Option<String>,          // stdio: required
    pub args: Vec<String>,                // stdio
    pub env: BTreeMap<String, String>,    // stdio; BTreeMap for stable serialization
    pub working_dir: Option<PathBuf>,     // stdio
    pub url: Option<String>,              // http / sse: required
    pub transport: String,                // "stdio" | "http" | "sse" | "ws"
                                          //   (validated against KNOWN_TRANSPORTS)
    pub startup_timeout: Duration,        // default 10s — initialize budget
    pub allowed_hosts: Vec<String>,       // SSRF allowlist for url transports (ADR 0012)
}
```

(Shutdown is not configurable: `Run` bounds it with a fixed 5s
`SHUTDOWN_TIMEOUT` and the stdio child is `kill_on_drop`.)

`Transport` is a **trait**, not an enum (`protocol/transport/mod.rs`) — the
config field is a validated string, and `Session` wraps a `Box<dyn Transport>`:

```rust
#[async_trait]
pub trait Transport: Send {
    /// Send one JSON-RPC request body, await the matching response body.
    async fn request(&mut self, body: &str) -> Result<String, TransportError>;
    /// Send a notification (no `id`, no response expected).
    async fn notify(&mut self, body: &str) -> Result<(), TransportError>;
    /// PID of the underlying process, if any (stdio knows; http/sse don't).
    fn pid(&self) -> Option<u32> { None }
    /// Close gracefully.
    async fn shutdown(self: Box<Self>) -> Result<(), TransportError>;
}
```

Implementations: `StdioTransport` (M1), `HttpTransport` (M4 — Streamable HTTP,
simple JSON variant), `SseTransport` (M4), `WsTransport` (Post-M7). Stdio
spawning goes through `Session::spawn` / `spawn_with` / `spawn_with_timeout`
plus `SpawnOptions` for stderr disposition (ADR 0013).

### 14.2 Scenario

Scenarios are a trait plus one concrete struct per scenario
(`scenario/mod.rs` + `scenario/<name>.rs`):

```rust
#[async_trait]
pub trait Scenario: Send + Sync {
    /// Drive the scenario until completion or until ctx.cancel_token fires.
    async fn drive(&self, session: &mut Session, ctx: &RunContext) -> ScenarioOutcome;
    /// JSON Schema fragment for this scenario's TOML block (`example-config`).
    fn config_schema(&self) -> Value;
    /// Short identifier (logs, reports, CLI args).
    fn name(&self) -> &'static str;
}

/// Per-run context shared between scenarios and the orchestrator.
/// #[non_exhaustive] — construct via RunContext::new(..).
pub struct RunContext {
    pub run_start: Instant,
    pub cancel_token: CancellationToken,   // scenarios must observe for shutdown
    pub metrics: Recorder,                 // lock-free per-call recorder
    pub hang_threshold: Duration,          // passed to hang_detect
    pub grace_period: Duration,            // hang → deadlock window
}

/// What drive() reports back to the orchestrator.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScenarioOutcome {
    pub total_calls: u64,
    pub successful_calls: u64,
    pub hang_count: u32,
    pub deadlock_count: u32,
    pub error_count: u64,
    pub notes: Vec<String>,                // free-form report lines
    pub hung_for_ms: Vec<u128>,            // duration of each deadlock-classified call
}
```

Concrete structs carry the per-scenario knobs directly, e.g.
`Sustained { concurrent, duration, tool, args }` and
`DeadlockProbe { concurrent, hang_threshold, grace_period, tool, args }`.
Selection is by the config `type` string — registration is a match arm in
`crates/mcp-loadtest-cli/src/cmd_run/builder.rs` (`build_scenario`, which also
owns the defaults) plus an entry in `KNOWN_SCENARIOS`
(`config/validate.rs`). Weighted multi-step tool calls are not a generic
`Vec<ToolCall>` on every scenario: they live in `scenario/pattern.rs`
(`PatternScenario`), which `sustained` drives internally when pattern config
is present.

> **Historical sketch (early draft) — superseded by the shipped API.** The
> original design here was a closed `ScenarioKind` enum (one variant per
> scenario, fields = knobs), a `Scenario { kind, tool_calls }` wrapper with
> weighted `ToolCall`s, and a `SessionPool` handed to `drive`. None of it
> shipped: the enum became per-scenario structs + string dispatch (open to
> external scenario authors without touching the lib), and the pool was cut
> because `Session::call_tool` takes `&mut self` — the first-cut scenarios issued
> sequential calls against a single session. The multi-session pool (true
> in-flight concurrency, the `reconnect` scenario, a real `cold_start`
> histogram) is the committed M8+ backlog — see §13.1.

### 14.3 Run + Report

```rust
// run.rs
pub struct Run {
    pub config: Config,                    // server + scenario + thresholds + output
    pub scenario: Box<dyn Scenario>,       // constructed by the caller (build_scenario)
    pub output_dir: PathBuf,               // where runs/<ulid>/ is created
    pub stderr_capture: StderrCapture,     // Off (default) | Capture | Tee — ADR 0013
}

impl Run {
    pub fn new(config: Config, scenario: Box<dyn Scenario>, output_dir: PathBuf) -> Self;
    pub fn with_stderr_capture(self, capture: StderrCapture) -> Self;
    pub async fn execute(self) -> Result<Report, RunError>;
}
```

Pass/fail budgets are not a standalone `Thresholds` type — they're the
`[thresholds]` config block, evaluated by pure functions in
`run/thresholds.rs`:

```rust
// config.rs — all fields optional; missing = "no constraint". #[non_exhaustive].
pub struct ThresholdsConfig {
    pub p50_latency: Option<Duration>,
    pub p95_latency: Option<Duration>,
    pub p99_latency: Option<Duration>,
    pub p999_latency: Option<Duration>,
    pub error_rate: Option<f64>,           // 0.0..=1.0
    pub hang_timeout: Option<Duration>,    // feeds hang_detect; default 5s when unset
    pub memory_growth_mb: Option<f64>,
    pub tool_slos: Vec<ToolSlo>,           // per-tool p99 budgets (M7)
}

// report/mod.rs
pub struct Report {
    pub run_id: String,                    // ULID
    pub started_at: SystemTime,
    pub duration: Duration,
    pub scenario_name: String,             // Scenario::name()
    pub server_info: ServerInfo,           // command/args (or url), pid, protocol_version
    pub metrics: ScenarioMetrics,          // latency + throughput + outcome counts
    pub process: ProcessStats,
    pub scenario_outcome: ScenarioOutcome, // §14.2 — carries deadlock_count / hang_count
    pub trace_path: Option<PathBuf>,       // None unless recording via `run --trace`
    pub threshold_violations: Vec<ThresholdViolation>,
    pub coverage: Option<CoverageReport>,  // registered vs exercised tools (M7)
}

impl Report {
    /// Every configured threshold satisfied AND no deadlocks detected.
    pub fn passed(&self) -> bool;
}
// Rendering goes through the `Reporter` trait
// (report/{markdown,json,html,terminal}.rs), not write_* methods on Report.

// metrics/types.rs — snapshots produced by the lock-free Recorder
pub struct ScenarioMetrics {
    pub latency: LatencyStats,
    pub throughput: ThroughputStats,
    pub outcomes: OutcomeCounts,           // one u64 per CallOutcome variant (§18)
}

pub struct LatencyStats {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub p999: Duration,
    pub mean: Duration,
    pub min: Duration,
    pub max: Duration,
    pub count: u64,
}

pub struct ThroughputStats {
    pub total_requests: u64,
    pub successful_requests: u64,
    pub requests_per_sec: f64,
}

// report/mod.rs
pub struct ProcessStats {
    pub peak_rss_mb: f64,
    pub final_rss_mb: f64,
    pub baseline_rss_mb: f64,              // start-of-run RSS baseline
    pub avg_cpu_pct: f64,
    pub peak_fd: u64,                      // 0 where sysinfo can't see fds (Windows)
    pub final_fd: u64,
    pub peak_threads: u64,                 // Linux only; 0 elsewhere
    pub final_threads: u64,
    pub samples: Vec<ProcessSample>,       // { at_secs, rss_mb, cpu_pct, fd, threads }
}

pub struct ThresholdViolation {
    pub kind: ThresholdKind,               // closed enum; serialized as "p99_latency" etc.
    pub expected: String,                  // e.g. "<= 500ms"
    pub actual: String,                    // e.g. "812ms"
}
```

Deltas from the original sketch: the raw `hdrhistogram::Histogram` stays
internal to `metrics::Recorder` (sharded per worker) — reports expose the
`LatencyStats` snapshot instead; per-category error counts live in
`OutcomeCounts` (flat struct) rather than an `ErrorStats` map; and
`ThresholdViolation.kind` is the `ThresholdKind` enum, not a free-form
string (see §17.2).

### 14.4 Errors

Errors are layered `thiserror` enums, one per layer:
`TransportError` → `SessionError` → `RunError` (plus `ConfigError` for
`Config::from_*`).

```rust
// run.rs — what can fail an entire run
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    #[error("session: {0}")]
    Session(#[from] SessionError),   // spawn / handshake / transport failures

    #[error("io: {0}")]
    Io(#[from] std::io::Error),      // writing run artifacts

    #[error("config: {0}")]
    Config(String),
}
```

`SessionError` (`session.rs`) covers io / json / server-side JSON-RPC error /
transport / id-mismatch / startup-timeout / schema-violation;
`TransportError` (`protocol/transport/mod.rs`) covers io / http / closed /
timeout / other. Spawn and handshake failures from the old sketch
(`ServerStart`, `Handshake`) arrive as `RunError::Session(..)` variants.
Crucially, **per-call failures don't fail the run** — they're classified
into `CallOutcome` buckets (§18) via `scenario::classify_error` and end up
in the report's `OutcomeCounts`.

---

## 15. Algorithm specs

The detection logic is the IP of this tool. Spec'd precisely so any implementer can reproduce.

### 15.1 Hang detector

Per-call watchdog. Wraps every `tools/call` request:

```
Algorithm: hang_detector(req, threshold)
1. Record send_at = now().
2. Send req to server.
3. Spawn watchdog task with timer = threshold.
4. Race: watchdog completes OR response arrives.
5. If response arrives first:
     duration = now() - send_at
     return Ok((response, duration))
6. If watchdog completes first:
     mark request_id as HUNG
     continue listening for late response (up to grace_period)
     if late response arrives: classify as LATE (not HUNG)
     if no response within grace_period: classify as DEADLOCK
     return Err(Hang { request_id, hung_for })
```

Hang ≠ deadlock. Hang means "no response within `hang_threshold`". Deadlock means "no response within `hang_threshold + grace_period`" — i.e. server appears genuinely stuck, not just slow.

### 15.2 Deadlock probe scenario

The Vibe-Trading-bug-class detector. Specific call sequence designed to reproduce lazy-init races.

```
Algorithm: deadlock_probe(server, tool, N, hang_threshold)
1. Spawn server. Record startup_duration = time-to-stdout-EOF or initialize-response.
2. Send `initialize`. Await with timeout = startup_timeout. (fails → SERVER_INIT_ERROR)
3. Send `notifications/initialized`.
4. Send `tools/list`. Await with timeout = 1s. (fails → TOOLS_LIST_HANG)
5. Synchronization barrier — all N tasks ready to send concurrently.
6. Release barrier. All N tasks send `tools/call` to `tool` simultaneously.
7. Each task: hang_detector(req, hang_threshold).
8. After all N return (Ok or Err): wait grace_period.
9. Categorize each:
     - Ok with duration → SUCCESS
     - Late response within grace_period → SLOW
     - No response after grace_period → DEADLOCK
10. Send shutdown notification, wait shutdown_timeout, kill if needed.
11. Report:
     - if DEADLOCK count > 0 → severity=CRITICAL, "DEADLOCK DETECTED"
     - else if SLOW > 0.5 * N → severity=WARNING, "concurrency degrades latency"
     - else → severity=PASS
```

The barrier in step 5-6 is critical. Without it, requests serialize naturally and lazy-init bugs hide. Barrier forces real concurrency at the point of greatest stress.

**Shipped reality:** `Session::call_tool` takes `&mut self`, so the
shipped `DeadlockProbe` issues its N calls **sequentially** against one
session and relies on `hang_detect` for classification (documented in
`scenario/deadlock_probe.rs`). The barrier across N independent sessions
needs the session pool (M8+ backlog, §13.1). The lazy-init bug class is
still caught — the offending call hangs forever regardless of concurrency.

### 15.3 Leak detector

```
Algorithm: leak_detector(server, scenario, sample_interval, growth_threshold_mb)
1. Run sustained scenario. Concurrently:
2. Every sample_interval, sample server's RSS via sysinfo.
3. After scenario completes:
4. Fit linear regression: rss_mb = a * t + b, where t in seconds
5. Predicted total growth = a * scenario.duration_secs
6. If predicted_growth > growth_threshold_mb:
     classify as LEAK_DETECTED
     report: slope (MB/sec), R² (fit quality), samples
7. R² < 0.5 → "noisy, can't conclude" — report as INDETERMINATE
```

Caveat: warmup-and-stabilize matters. First 30s of samples are discarded by default to avoid false positives from JIT / lazy-load.

### 15.4 Threshold evaluator

```
Algorithm: evaluate_thresholds(report, thresholds)
For each threshold field that is Some:
  compare report's metric to threshold
  if violated: append ThresholdViolation { metric, expected, actual }
Return: violations vec — empty means PASS.
```

Simple, but worth specifying so the report's `passed()` is unambiguous.

---

## 16. Mock server specs

Mocks live in `tests/fixtures/<name>.py`. Each is < 50 lines of Python — minimal MCP server using stdio + JSON-RPC by hand (no fastmcp dep, to avoid version coupling). Shipped fixtures: `mock-normal.py`, `mock-slow.py`, `mock-broken.py`, `mock-crash.py`, `mock-leak.py`, `mock-error.py`, `mock-slow-init.py`, `mock-malformed.py`, plus `mock-http-server.py` and `mock-sse-server.py` (transport parity coverage). Pseudocode for each below.

### 16.1 mock-normal.py

```python
# Echoes args, responds in 1ms. Reference implementation.
while True:
    line = sys.stdin.readline()
    msg = json.loads(line)
    if msg["method"] == "initialize":
        respond({"protocolVersion":"...", "capabilities":{...}})
    elif msg["method"] == "tools/list":
        respond({"tools":[{"name":"echo","inputSchema":{...}}]})
    elif msg["method"] == "tools/call":
        respond({"content":[{"type":"text","text":json.dumps(msg["params"]["arguments"])}]})
```

### 16.2 mock-slow.py

Same as mock-normal, but `tools/call` does `time.sleep(2)` before responding. Used to verify latency histogram correctness (p99 should be ~2s).

### 16.3 mock-broken.py

```python
# Replicates Vibe-Trading lazy-init deadlock pattern.
# initialize and tools/list work; first tools/call hangs forever.
calls_made = 0
while True:
    msg = json.loads(sys.stdin.readline())
    if msg["method"] in ("initialize", "tools/list"):
        respond_normally()
    elif msg["method"] == "tools/call":
        # The bug: blocking import in worker
        if calls_made == 0:
            calls_made += 1
            time.sleep(999999)              # actual deadlock
        else:
            respond_normally()
```

`deadlock_probe` against this MUST report `deadlock_count >= 1`.

### 16.4 mock-crash.py

```python
# Panics 1% of calls (random.random() < 0.01). Tests error rate accuracy.
# Crash = exit(1), not JSON-RPC error.
```

### 16.5 mock-http-server.py

```python
# Streamable HTTP transport fixture. Stdlib http.server only — no fastapi/etc.
# Used by HttpTransport integration tests.
```

### 16.6 mock-sse-server.py

```python
# HTTP+SSE transport fixture. Endpoint handshake + id-correlated responses.
# Stdlib http.server only. Used by SseTransport integration tests.
```

### 16.7 mock-leak.py

```python
# Allocates 10 KB per tools/call into a module-global list. Never frees.
# Tests leak detector — slope should be ~10KB × rps.
# Today leak/drift signals are exercised via `Soak::detect_leak` over synthetic
# (t, rss) series; a real leaking fixture is still useful for end-to-end coverage.
```

### 16.8 mock-error.py

```python
# Returns JSON-RPC errors per spec: -32601 method not found,
# -32602 invalid params, -32603 internal error.
# Cycles through error codes per call. Tests error classification (§18).
```

### 16.9 mock-slow-init.py

```python
# Sleeps 5s on `initialize` before responding. Tests cold_start measurement.
```

### 16.10 mock-malformed.py

```python
# Returns invalid JSON every 10th response (truncated, missing field).
# Tests parser robustness — should classify as MALFORMED_RESPONSE not crash.
```

All mocks share common framing helpers in `tests/fixtures/_common.py` (read frame, write frame, respond ok/err).

---

## 17. Output format spec

### 17.1 Trace format (`trace.jsonl`)

One JSON object per line. Schema:

```json
{
  "ts": 0.0,                              // seconds since run start (f64)
  "kind": "request|response|error|hang|deadlock|process_sample|scenario_event",
  "request_id": 123,                      // matches JSON-RPC id, present for request/response/error/hang/deadlock
  "method": "tools/call",                 // present for request
  "params": {...},                        // present for request (compact, can be large)
  "result": {...},                        // present for response (truncated to 1KB by default)
  "error": {"category": "...", "message": "...", "code": -32603},  // present for error
  "duration_ms": 12.5,                    // present for response/error
  "rss_mb": 45.2,                         // present for process_sample
  "cpu_pct": 12.3                         // present for process_sample
}
```

Stream-friendly. Can be processed with `jq` or any line-oriented tool.

### 17.2 metrics.json

```json
{
    "run_id": "01HXY...",
    "started_at": "2026-05-10T07:30:00Z",
    "duration_secs": 60.0,
    "scenario": {
        "name": "sustained"
    },
    "server": {
        "command": "python",
        "args": ["-m", "my_mcp"],
        "pid": 12345,
        "protocol_version": "2025-03-26"
    },
    "latency_ms": {
        "p50": 12.3,
        "p95": 45.6,
        "p99": 123.4,
        "p999": 456.7,
        "min": 1.2,
        "max": 999.9,
        "mean": 23.4,
        "count": 12345
    },
    "throughput": {
        "total_requests": 12345,
        "successful_requests": 12300,
        "requests_per_sec": 205.75
    },
    "errors": {
        "total": 45,
        "by_category": {
            "Hang": 0,
            "Timeout": 5,
            "ServerError": 30,
            "ProtocolError": 10,
            "Crash": 0,
            "Malformed": 0,
            "Disconnected": 0,
            "Cancelled": 0
        }
    },
    "process": {
        "peak_rss_mb": 156.3,
        "final_rss_mb": 142.1,
        "avg_cpu_pct": 23.4
    },
    "deadlock_count": 0,
    "hang_count": 0,
    "threshold_violations": [
        { "metric": "p99_latency", "expected": "<=100ms", "actual": "123.4ms" }
    ],
    "passed": false
}
```

On the Rust side, `metric` is a `ThresholdKind` enum (`crate::report::ThresholdKind`); serde flattens it to the string slug shown here via `#[serde(rename = "metric")]` + per-variant snake_case so the wire format stays stable across refactors.

JSON Schema published at [`docs/schema/metrics.v1.json`](docs/schema/metrics.v1.json) for downstream tooling. Its conformance to this reporter's output is pinned by `tests/metrics_schema.rs`.

### 17.3 report.md template

```markdown
# Run {run_id}

**Status:** ❌ FAIL (1 threshold violation)
**Server:** `python -m vibe_trading_mcp`
**Scenario:** Sustained, 50 concurrent, 60s
**Started:** 2026-05-10 07:30:00 UTC

## Summary

- Total requests: 12,345
- Throughput: 205.75 req/s
- Error rate: 0.36%
- Deadlocks: 0 Hangs: 0

## Latency

| p50    | p95    | p99            | p999    | max     |
| ------ | ------ | -------------- | ------- | ------- |
| 12.3ms | 45.6ms | **123.4ms** ❌ | 456.7ms | 999.9ms |

(latency histogram ASCII chart here)

## Errors

| Category      | Count |
| ------------- | ----- |
| ServerError   | 30    |
| ProtocolError | 10    |
| Timeout       | 5     |

## Process

Peak RSS: 156.3 MB · Final RSS: 142.1 MB · Avg CPU: 23.4%

## Threshold violations

- ❌ **p99_latency**: expected ≤100ms, got 123.4ms

## Trace

Full trace: `./trace.jsonl` (12,345 events, 8.2 MB)
```

---

## 18. Error taxonomy

Every failure is classified into exactly one category. Used for `ErrorStats.by_category` and reporting.

| Category        | Definition                                                                            | Example                              |
| --------------- | ------------------------------------------------------------------------------------- | ------------------------------------ |
| `Hang`          | No response within `hang_threshold`, but response arrived before grace_period expires | tool genuinely slow under contention |
| `Deadlock`      | No response after `hang_threshold + grace_period`                                     | Vibe-Trading PR #85                  |
| `Timeout`       | Client-side configured deadline exceeded (separate from hang_threshold)               | network buffer full                  |
| `ServerError`   | JSON-RPC error response with `code` in `[-32099..=-32000]` (server-defined)           | tool returned business error         |
| `ProtocolError` | JSON-RPC error with `code` `-32600..=-32603` (transport / spec violations)            | malformed request rejected           |
| `Crash`         | Server process exited (non-zero or signal) during call                                | unhandled panic                      |
| `Malformed`     | Response was not valid JSON or didn't match JSON-RPC schema                           | partial response, broken framing     |
| `Disconnected`  | Transport closed unexpectedly mid-call                                                | broken pipe                          |
| `Cancelled`     | Client cancelled the request before response                                          | scenario shutdown                    |

Classification precedence: top-down. A request that hangs and then the server crashes → classified as `Crash` (the terminal event), but trace.jsonl records both `hang` and `crash` events for forensics.

---

## 19. Performance targets for the tool itself

`mcp-loadtest` should never be the bottleneck.

| Aspect                                         | Target                                |
| ---------------------------------------------- | ------------------------------------- |
| Driver per-request CPU overhead                | < 50µs (excluding JSON serialization) |
| Memory per concurrent worker                   | < 100KB                               |
| Max sustainable concurrency on a 4-core laptop | ≥ 1000 workers                        |
| Trace file write throughput                    | ≥ 100k events/sec                     |
| Histogram update                               | lock-free per-worker, merged at end   |

These are tested in `benches/` (criterion); reproduce with `cargo bench --workspace`.

---

## 20. Versioning + stability policy

- v0.x: API can change anywhere
- v1.0: locked. Breaking changes require major version bump (semver strict)
- MCP spec: `protocol_version` field in `initialize` is checked. Mismatch warns but does not fail by default. Override with `--strict-protocol`.
- Library MSRV (minimum supported Rust version): stable - 2 (e.g. if 1.85 is current stable, MSRV is 1.83).

When to commit to 1.0:

- After 3 months of v0.x with no breaking changes
- After 5+ external users have integrated
- After at least 1 real bug caught in the wild and reported back

---

## 21. AI-friendliness (design pillar)

mcp-loadtest is a tool that AI agents will both **operate** (Claude Code running CI) and **be operated by** (developers asking Claude "load-test my MCP server"). Design accordingly.

### 21.1 First-class library API for embedding in agent tools

- All public types have `#[derive(Debug, Serialize, Deserialize)]` so they're trivially JSON-able.
- The library API is **documented with rustdoc examples** that compile (doctested in CI). LLMs read these examples to build correct calls on the first try.
- No "you must construct in this exact order" sequencing — builders are commutative where possible.

### 21.2 Self-hosted MCP server: `mcp-loadtest serve --mcp`

The single most important AI-friendly feature. mcp-loadtest exposes itself as an MCP server with these tools:

| Tool               | Args                                                            | Returns                                            |
| ------------------ | --------------------------------------------------------------- | -------------------------------------------------- |
| `deadlock_probe`   | `server_command`, `tool`, `concurrent`                          | `{ deadlock_count, hung_for_ms[], details }`       |
| `sustained_load`   | `server_command`, `concurrent`, `duration_secs`, `tool`, `args` | `{ p50_ms, p99_ms, error_rate, requests_per_sec }` |
| `compare_runs`     | `baseline_run_dir`, `current_run_dir`                           | structured diff with regression flags              |
| `report_summary`   | `run_dir`                                                       | markdown summary string                            |
| `list_recent_runs` | `limit`                                                         | run dirs with metadata                             |

A user can say to Claude / Cursor / any MCP-aware agent: _"Find deadlocks in my new MCP server at `python -m foo`"_ — and the agent calls `deadlock_probe` directly. **No human-in-the-loop required to spawn a child process and parse stdout** — the agent gets structured JSON back.

**Reaatech doesn't do this.** It's our most under-priced differentiator.

### 21.3 Actionable error messages with hints

Every `Err` returned to the user includes a suggested next step:

```
Error: server stdin closed unexpectedly during initialize handshake.
Hint: server may have crashed before responding. Check stderr at:
      runs/01HXY.../server.stderr.log
      Or re-run with --tee-stderr to see it live.
```

vs. the bad version:

```
Error: BrokenPipe(Os { code: 32, ... })
```

LLMs (and humans) act on the first; bounce off the second.

### 21.4 `--explain` flag on every subcommand

```
$ mcp-loadtest deadlock-probe --explain
Algorithm:
  1. Spawn server process.
  2. Send `initialize`. Wait up to startup_timeout (default 10s).
  3. Send `notifications/initialized`.
  4. Send `tools/list`. Wait up to 1s.
  5. Synchronization barrier — N concurrent `tools/call` ready to fire.
  6. Release barrier. All N calls fire in parallel.
  7. Each call wrapped in hang_detect(hang_threshold=5s, grace_period=10s):
     - response within hang_threshold → SUCCESS
     - response between threshold and grace_period → SLOW (warning)
     - no response after grace_period → DEADLOCK (critical)
  8. Report aggregated results.

Tunable knobs: --concurrent, --hang-threshold, --grace-period.
See DESIGN.md §15.2 for the spec source.
```

LLMs use this to plan the right invocation. Reduces "I tried it but it didn't do what I expected" loops.

### 21.5 JSON Schema published for config + outputs

`schema/config.v1.json` and `schema/metrics.v1.json` shipped at well-known paths. LLMs validate generated configs / parse outputs without guessing field shapes.

### 21.6 `mcp-loadtest doctor`

Diagnoses common setup issues:

- Python interpreter not on PATH (for fixture-based tests).
- MSVC vs GNU toolchain mismatch on Windows.
- Stale `runs/` accumulation.
- MCP server fails initialize — captures stderr and reports.

Outputs a checklist with ✅/❌ per item and a one-line fix per ❌. Exactly the kind of thing an LLM agent can chain into a fix-it loop.

### 21.7 Trace format is LLM-readable

`runs/<id>/trace.jsonl` is line-oriented JSON with stable field names (DESIGN.md §17.1). Pipeable through `jq`, parseable by any agent without custom code:

```bash
$ jq 'select(.kind=="hang")' runs/01HXY.../trace.jsonl
```

### 21.8 Reports include "What this means" interpretation

A report that says `p99 latency: 234ms` is data. A report that adds `"95% of users would call this acceptable; the slow tail (top 1%) is concentrated on `analyze_options` calls"` is information. We aim for the latter — derived sentences, not just numbers.

### 21.9 Snapshot tests for output formats

`insta::assert_snapshot!` on report markdown / JSON. Output shapes are stable across releases unless explicitly changed (with CHANGELOG entry). LLM agents that parse our output don't break across patch versions.

### 21.10 Cookbook in `docs/examples/`

Per-scenario copy-pasteable commands + expected output. LLMs train on README-style examples; cookbook entries make those examples concrete and executable.

Examples to ship at 0.0.1:

- "Find deadlocks in my new MCP server"
- "Add a regression gate to my CI"
- "Compare two implementations of the same MCP server"
- "Detect a memory leak before production"
