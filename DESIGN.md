# mcp-loadtest — Design Document

**Status:** `v0.1.0` design baseline. The internal `0.0.1` version was never
tagged or released. A source version does not prove external availability;
exact tag and GitHub Release actions remain gated by `docs/RELEASE.md`.
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

There are excellent general load generators and multiple purpose-built MCP
test/load projects. The remaining gap this project targets is narrower:
MCP-aware reliability CI that classifies hangs/deadlocks, synchronized
response divergence, protocol failures, and per-tool performance signals
without a bespoke client script.

`mcp-loadtest` aims to be the canonical tool: language-agnostic, transport-agnostic, with built-in scenarios for the bug classes that actually occur.

---

## 2. Goals & Non-Goals

### Goals

- Detect deadlocks, hangs, livelocks under realistic concurrent load
- Measure latency (p50/p95/p99), throughput, error rate
- Work across stdio, Streamable HTTP, legacy HTTP+SSE, and WebSocket for the
  protocol revisions each adapter explicitly supports. Remote authentication
  is limited to `headers_from_env`: static headers loaded from environment
  variables. Nonempty remote headers require TLS (`https://` for HTTP/SSE,
  `wss://` for WebSocket), URL userinfo is forbidden, and there is no insecure
  fallback. OAuth login/refresh/discovery is out of scope for the candidate.
- Library mode (Rust crate) for embedding in CI tests
- CLI mode for ad-hoc smoke tests and benchmarks
- Cross-platform (Linux, macOS, Windows — author runs Windows so this is a hard requirement, not aspirational)
- Low-config quick-start through the shipped `deadlock-probe` subcommand and
  `example-config`; a separate zero-config `probe` command is not shipped.

### Non-Goals

- **Not a replacement for unit tests.** Different problem.
- **Not a tool for testing MCP clients.** Client-side bugs are a separate domain.
- **Not validating tool output correctness.** We test protocol-level behavior. If your tool returns wrong data, that's not what we catch.
- **Not a general coverage-guided fuzzer.** The shipped `fuzzer` scenario is a
  bounded catalog of malformed-but-plausible MCP/JSON-RPC payloads.
- **Not a benchmark suite.** We provide infrastructure to bench, not a curated set of "official" benchmarks.

---

## 3. Background

### MCP protocol (relevant subset)

JSON-RPC 2.0 framing over one of four transports:

- **stdio** — line-delimited JSON over child process stdin/stdout (most common, all examples in this doc focus here)
- **HTTP** — Streamable HTTP (simple JSON variant); request via POST, simple JSON response
- **HTTP+SSE** — request via POST, server pushes events via SSE channel
- **WebSocket** — bidirectional frames

The stable default remains the handshake-based `2025-11-25` revision.
`2026-07-28` is an explicit experimental stateless implementation reconciled
to the official final tag at spec commit
`5f5440bb26a62e2cf3440b92da5a667efa03b267` and conformance commit
`49103de6ed70804e940637bf3e9e29e4a3f54e64`. The upstream revision is final;
the implementation remains experimental because it intentionally covers only
tools, discovery, request metadata/headers, stdio, and Streamable HTTP. It is
not the default or a full-protocol conformance claim (ADR 0023).

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
and scenarios receive one borrowed `Session`. Workloads that need real
concurrency spawn independent sessions through `RunContext::session_factory`
and the internal `scenario::pool`.

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
# Targeted smoke for the Vibe-Trading bug class
mcp-loadtest deadlock-probe --server "python -m my_mcp" --tool get_market_data

# Run from config file
mcp-loadtest run --config bench.toml

# Record/replay protocol frames
mcp-loadtest run --config bench.toml --trace ./trace.jsonl
mcp-loadtest replay ./trace.jsonl --server "python -m my_mcp"

# Compare saved metrics, or drive multiple stdio servers side by side
mcp-loadtest compare baseline.json current.json
mcp-loadtest cross --server "python -m server_a" --server "python -m server_b"

# List built-in scenarios
mcp-loadtest list-scenarios

# Print example config
mcp-loadtest example-config > bench.toml

# Diagnose the local toolchain and, optionally, a stdio server
mcp-loadtest doctor --server "python -m my_mcp"

# Expose the three shipped agent tools over stdio
mcp-loadtest serve --mcp
```

There is no `probe` or `report` subcommand, and `run` is config-driven rather
than accepting an inline server/scenario/load profile.

Output structure for each run (dirs are named by ULID, not timestamp):

```
runs/01HXY.../
├── report.md            # human-readable summary       (format "markdown")
├── metrics.json         # aggregated metrics            (format "json";
│                        #   schema: docs/schema/metrics.v1.json — §17.2)
├── report.html          # self-contained HTML           (format "html")
├── server-stderr.log    # initial orchestrator session stderr — only with
│                        #   --capture-stderr / --tee-stderr (ADR 0013)
└── server-stderr/       # immutable factory-session logs (pooled/cold-start)
    ├── session-000001.log
    └── session-000002.log
```

The trace is written to the exact path passed to `run --trace <file>`
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
startup_timeout = "10s"       # connect + initialize/discover + initial tools/list
# url = "https://mcp.example.com/mcp"  # required for http | sse | ws
# allowed_hosts = ["mcp.example.com"]  # exact-match SSRF allowlist
# protocol_version = "2025-11-25"      # "auto" or an explicit supported pin
# headers_from_env = { Authorization = "MCP_AUTHORIZATION" }
# Remote header values come only from env and require https/wss.
# URL userinfo is forbidden. Queries are sent but report as ?redacted:
# never put credentials in a URL query. No literal secrets or OAuth flows.

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
| `sustained`      | Constant load for a fixed duration; `concurrent > 1` drives a real session pool (ADR 0017; disclosed sequential single-session fallback without a factory). The single-tool and multi-step weighted-pattern forms share the pooled path. | baseline p99 latency, throughput, sustained error rate        |
| `spike`          | Baseline → sharp burst at peak concurrency for a fixed window → cooldown back to baseline. Models Black-Friday-style traffic spikes. Each phase is a real session pool (sequential iterations-per-tick fallback without a session factory).          | queue overflow, recovery behavior, fairness under burst       |
| `ramp`           | Step concurrency from `from` to `to` by `step_increment`, optionally feeding the per-step metrics into [`analysis::breaking_point`]. Each step is a real session pool (sequential iterations-per-step fallback without a session factory).           | finds break-point — concurrency where p99 explodes            |
| `soak`           | Long-duration steady load with periodic snapshots; pairs with `analysis::regression` for latency-drift and (via `ProcessSampler`) RSS-slope leak signals.                                                                                                       | memory leaks, latency drift, throughput collapse over hours   |
| `pattern`        | Multi-step weighted-random tool-call sequences with per-pattern `think_time` and `ErrorBehavior`; uses independent pooled sessions when a factory is present and `concurrent > 1`.                                                                                | realistic mixed workloads (explore-then-act, read-then-write) |
| `deadlock_probe` | For N>1, complete N independent session handshakes then release one `hang_detect`-wrapped call per worker through a shared gate; N=1 uses the focused borrowed-session path.                                                                                    | the **Vibe-Trading bug class** specifically                   |
| `race_check`     | Complete N independent handshakes, release one identical call per session through a shared start gate, then canonicalize and compare responses. Divergence is a first-class CI failure.                                                                         | concurrent non-determinism / divergent identical inputs       |
| `fuzzer`         | Cycle through enumerated malformed-but-plausible payloads (unknown method, numeric method, giant payload, control chars, deep-nested, null/string params); classify each via `analysis::fuzz_report`.                                                           | parser bugs, type-confusion in method dispatch                |
| `version_matrix` | Drive the same server once per MCP protocol revision (fresh session per revision via `SessionFactory::with_version`, ADR 0018); each revision's `hang_detect`-wrapped calls record under the per-tool key `version:<rev>`, so the report diffs revisions side by side. | revision-specific deadlocks, errors, latency deltas           |

Deferred:

- `slow_mix` — 80% calls to a fast tool, 20% to a deliberately-slow tool (head-of-line blocking, fairness). Approximable today by configuring a multi-step `pattern` with weighted tools.
- `reconnect` — drop a session mid-call, spawn a replacement, and retry.
  Independent-session pooling exists, but the required mid-call lifecycle and
  retry semantics are not implemented.

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

There is no public `SessionPool` type — `drive` gets one `&mut Session`, so
calls against that borrowed session run sequentially. Pooled scenarios spawn
independent sessions via `RunContext::session_factory` and the internal
`scenario::pool` driver (ADR 0017). Scenario selection is by the config `type` string: a match arm in
`crates/mcp-loadtest-cli/src/cmd_run/builder.rs` (`build_scenario`) plus an
entry in `KNOWN_SCENARIOS` (`crates/core/src/config/validate.rs`).
See §14.2 for the full shipped types.

---

## 9. Test matrix

### Layer A — does mcp-loadtest itself work?

Mock MCP servers in `tests/fixtures/`. Each is a tiny Python script (chosen for ubiquity, not Rust, to make the test environment realistic).

| Mock                | Behavior                                                | Tests                                 |
| ------------------- | ------------------------------------------------------- | ------------------------------------- |
| `mock-normal.py`    | Echoes args; rejects malformed JSON with `-32700`       | happy-path metrics and raw rejection  |
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

The experimental implementation of the final `2026-07-28` subset also has
dedicated official-harness runners:
`bash crates/protocol/tests/run-official-conformance.sh` and
`pwsh crates/protocol/tests/run-official-conformance.ps1`. They pin
final spec commit `5f5440bb26a62e2cf3440b92da5a667efa03b267` and conformance
commit `49103de6ed70804e940637bf3e9e29e4a3f54e64`, verify the final
tag-to-commit identity, and retain the complete official client inventory plus
an explicit executed/not-executed scope manifest. A successful five-scenario
tools/discovery/metadata/header run is a scoped release gate, not an evergreen
unpinned network test or a claim over full-protocol, auth, MRTR,
subscriptions, schema-reference, server, or authorization-server suites. The
conformance pin remains the latest official harness, but its `2026-07-28`
label/schema are still DRAFT/provisional and its vendored schema comes from
specification commit `71e306956a4959c9655e5036be215d41986596e6`.
Final-spec reconciliation is proven separately by the machine-checked final
tag/schema comparison retained in `FINAL_SCHEMA_RECONCILIATION.txt` and
documented in ADR 0023, not by calling that harness a final-promoted suite.

---

## 10. Milestones (revised 2026-05-10 — head-on competition with reaatech/mcp-load-test)

Original 3-week plan replaced after discovering [reaatech/mcp-load-test](https://github.com/reaatech/mcp-load-test) already ships the load-testing basics (see §10.5 for parity matrix). The first release of mcp-loadtest must reach feature parity _and_ surface our differentiators before publishing.

The repository remained private through M1-M7 and the release-hardening pass.
The internal `0.0.1` version was never tagged or released. `v0.1.0` is the
first candidate; public visibility, its immutable tag, and GitHub Release
remain separate maintainer gates in `docs/RELEASE.md`.

M1 through M7 and the post-M7 implementation slices are present in the
candidate. “Present in the tree” is not synonymous with “released”; the
release gates still apply.

| M             | Theme                            | Key deliverables                                                                                                                                                                                                                                  |
| ------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **M1** ✓      | stdio Session                    | `Session::spawn` → handshake → `list_tools`/`call_tool`/`shutdown`; mock-normal.py; happy-path integration test                                                                                                                                   |
| **M2** ✓      | Scenarios + metrics core         | `Scenario` trait; `cold_start` + `sustained` + `deadlock_probe` impls; `hang_detector` (§15.1); hdrhistogram metrics; mocks `mock-broken`/`mock-slow`/`mock-crash` + tests                                                                        |
| **M3** ✓      | Reports + first internal release | TOML config; markdown / JSON / console reporters; sysinfo-based process sampling; **regression test against real Vibe-Trading commit ~PR-85**                                                                                                     |
| **M4** ✓      | Transport parity                 | HTTP transport (StreamableHTTP); SSE transport; HTTP/SSE fixtures; transport-aware concurrency profiles                                                                                                                                           |
| **M5** ✓      | Analysis parity                  | `breaking_point` detection; performance grading (A-F per latency/concurrency/error); realistic patterns (explore-then-act, read-then-write, multi-step) with weighted random + think-time; `soak` scenario polish; `compare-baselines` subcommand |
| **M6** ✓      | Differentiators v1               | Ratatui dashboard library component (not wired to a CLI flag); server resource sampling beyond RSS (CPU, fd, threads); response-divergence analysis; `cross --server ...` comparison                                      |
| **M7** ✓      | Differentiators v2 + release polish | Protocol fuzzer (basic — random/malformed payloads); coverage tracking (tools registered vs. exercised); per-tool SLO assertions; README rewrite with competitive positioning; `cargo install` smoke test on all 3 OS                             |
| **Post-M7** ✓ | Pre-public-release close-out     | Spike scenario; HTML reporter; WebSocket transport; hot-path zero-copy refactor; criterion benches (DESIGN §19 claims now reproducible). See CHANGELOG.                                                                            |
| **v0.1.0 RC** | First public-release candidate   | correctness/security hardening, pinned scoped conformance against the final protocol revision, honest docs; still gated before public/tag/Release                                                                                           |
| _Later_       | Beyond                           | AI-assisted pattern generator; distributed mode; PyO3 binding                                                                                                                                                       |

**Definition of done for the first release:**

- `docs/RELEASE.md` is the authoritative release gate. The milestone and
  competitor matrices below are historical planning context, not a substitute
  checklist and not evidence that an external release exists.
- Every gate in `docs/RELEASE.md` passes on the exact immutable release commit.
- `cargo install --git <repo-url> --tag v0.1.0 --locked mcp-loadtest-cli`
  works on clean Linux/macOS/Windows environments, and verified prebuilt
  binaries are attached to the matching GitHub Release.
- The retained self-contained deadlock regression passes. The ignored,
  network-dependent Vibe-Trading test is supplementary evidence only when its
  pinned upstream fixture is explicitly available and run.
- README and release notes describe only implemented, verified behavior;
  historical competitor comparisons are not a release gate.

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
| **Synchronized response-divergence check**                  | ✗                                    | ✓ v0.1.0 RC             | One identical call per independent session, released through a shared gate; divergent responses fail the run.                                                                          |
| **Ratatui dashboard component**                             | ✗ (post-hoc only)                    | library only            | Renderable by embedders; no CLI flag/subcommand currently exposes it.                                                                                                                   |
| **Cross-server compare** (run vs N targets)                 | partial (compare baselines = 2 runs) | ✓ M6 (1 run, N targets) | Side-by-side: vendor A vs vendor B vs your fork.                                                                                                                                       |
| **Server resource sampling** (CPU/fd/threads/RSS over time) | ✗ (latency only)                     | ✓ M6                    | Find resource exhaustion before throughput collapses.                                                                                                                                  |
| **Protocol fuzzer (mcp-fuzz integrated)**                   | ✗                                    | ✓ M7                    | Random/malformed payloads; finds parser bugs unit tests miss.                                                                                                                          |
| **Coverage tracking** (registered vs exercised tools)       | ✗                                    | ✓ M7                    | Catch silently-broken tools that nobody tests in CI.                                                                                                                                   |
| **Per-tool SLO assertions**                                 | partial (global)                     | ✓ M7                    | Per-tool latency/error budgets in CI.                                                                                                                                                  |
| **Configurable regression thresholds**                      | ✗ (fixed)                            | ✓ v0.1.0 RC              | `compare` CLI flags + `compare_runs` MCP args override p99 / error-rate / deadlock policy; defaults unchanged (ADR 0009).                                                              |
| **Protocol-aware assertions**                               | ✗                                    | ✓ v0.1.0 RC              | Opt-in strict mode validates `tools/call` args vs the server's advertised `inputSchema`; mismatch → `ProtocolError` gates the run. Forward-compatible, off by default (ADR 0005/0010). |
| **Rust perf** + static binary                               | ✗ (Node runtime required)            | ✓                       | `cargo install` → single ~5MB binary; no Node toolchain.                                                                                                                               |
| **AI-assisted pattern generator**                           | ✗                                    | ⏳ M8 stretch           | LLM reads tool schemas → generates realistic call sequences.                                                                                                                           |
| **Distributed mode**                                        | ✗                                    | ⏳ M8 stretch           | Multiple workers driving one server (high-RPS targets).                                                                                                                                |
| **Replay / record**                                         | ✗                                    | ✓ v0.1.0 RC             | `run --trace` writes `mcp-trace/1`; `replay` diffs canonical responses and gates on divergence.                                                                                         |
| **Self-hosted as MCP server** (`mcp-loadtest serve --mcp`)  | ✗                                    | ✓ M7                    | AI agents (Claude, Cursor, etc.) call `deadlock_probe` / `compare` / `report` directly via MCP. Recursive: load-test an MCP using an MCP.                                              |

### Strategic positioning for the first candidate

> mcp-loadtest is a **reliability CI gate for MCP servers**: load generation
> plus MCP-aware deadlock/hang classification, synchronized response-
> divergence checks, traces, resource signals, and deterministic exit codes.

The README at publish must lead with the deadlock demo (replicated Vibe-Trading PR #85 bug, caught in 2 seconds) — not the load-testing checklist. Differentiation first; parity proves we're serious.

---

## 11. Decisions (resolved 2026-05-10)

| #   | Question                | Decision                                                                                                       | Rationale                                                                                                               |
| --- | ----------------------- | -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| 1   | Crate name              | **`mcp-loadtest`** (lib) + **`mcp-loadtest-cli`** (bin)                                                        | descriptive, discoverable, doesn't pigeonhole to "bench"                                                                |
| 2   | License                 | **MIT OR Apache-2.0** (dual)                                                                                   | Rust ecosystem standard; MIT for individuals, Apache-2.0 for corporate patent grant                                     |
| 3   | Repo location           | **`github.com/Teerapat-Vatpitak/mcp-loadtest`**                                                                | personal handle for now; transfer to `mcp-tools/` org if/when sister projects emerge                                   |
| 4   | MCP protocol versioning | stable handshake revisions at runtime; strict validation is opt-in; `2026-07-28` remains an explicitly pinned experimental implementation of a reconciled final-spec subset | test deployed revisions without overstating the implemented or conformance-tested surface |
| 5   | `deadlock_probe`        | both a quick subcommand and `[scenario] type = "deadlock_probe"` under config-driven `run`; N>1 uses a synchronized independent-session gate | subcommand for newcomer UX, scenario for CI |
| 6   | Server stderr           | stdio inherits stderr by default; config-driven `run --capture-stderr` writes `server-stderr.log`, and `--tee-stderr` also mirrors it | accurate opt-in capture without pretending the quick subcommand persists it |
| 7   | Diff-vs-baseline mode   | shipped as `compare` and the `compare_runs` MCP tool                                                            | regression flags gate with a non-zero exit |
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

## 13. Future work (out of scope for the first release candidate)

- **Distributed mode** — multiple loadtest workers driving one server (for very high RPS targets)
- **GUI/web UI** — render reports interactively
- **Plugin system** — user-defined scenarios as separate crates
- **Public benchmark dataset** — track perf of popular MCP servers over time (`mcp-leaderboard`)

The earlier `mcp-fuzz` and `mcp-trace` sister-project ideas were absorbed into
the bounded fuzzer scenario and in-workspace `mcp-trace/1` record/replay
implementation.

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
9. **Later stretch** — distributed multi-worker, PyO3 binding,
   AI-assisted pattern generator
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
    pub url: Option<String>,              // http / sse / ws: required
    pub transport: String,                // "stdio" | "http" | "sse" | "ws"
                                          //   (validated against KNOWN_TRANSPORTS)
    pub startup_timeout: Duration,        // default 10s — complete Run startup budget
    pub allowed_hosts: Vec<String>,       // SSRF allowlist for url transports (ADR 0012)
    pub protocol_version: Option<String>, // "auto" or explicit supported revision
    pub headers_from_env: BTreeMap<String, String>,
                                          // remote header → env-var name;
                                          // complete values resolved at connect
}
```

`headers_from_env` is the complete remote-auth surface for the candidate:
static HTTP/SSE/WS headers only, with values sourced from environment
variables. A nonempty map requires `https://` for HTTP/SSE or `wss://` for
WebSocket, with no plaintext fallback. URL userinfo is rejected. URL queries
are transmitted unchanged to the configured target but replaced wholesale
with `?redacted` in reports and traces, so credentials must never be placed in
a query. Literal header secrets, OAuth login/refresh/discovery, interactive
authorization, and token persistence are not implemented.

Shutdown is not configurable. Stdio applies bounded graceful-exit,
forced-kill/reap, stderr EOF-drain, and cancellation-fallback phases (11
seconds of internal phase budgets); scenario/run outer guards allow 15 seconds
total, leaving a 4-second scheduling margin. Any shutdown
error or outer timeout increments a typed teardown-failure counter and gates
the report instead of being swallowed. `kill_on_drop` remains only a
last-resort termination request when a future is cancelled before orderly
teardown can complete.

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

Implementations: `StdioTransport` (M1), `HttpTransport` (M4 — Streamable HTTP),
`SseTransport` (M4), `WsTransport` (Post-M7). Stdio
spawning goes through `Session::spawn` / `spawn_with` / `spawn_with_timeout`
plus `SpawnOptions` for stderr disposition (ADR 0013).

Remote-controlled HTTP response bodies, SSE event data, and WebSocket messages
have a 16 MiB limit enforced during network consumption/reassembly rather than
after an unbounded payload has been materialized. SSE and WebSocket also share
a 32 MiB retained-byte budget across each transport's reader channel and
id-mismatch queue.

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
    pub metrics: Recorder,                 // atomic counters + sharded locked histograms
    pub hang_threshold: Duration,          // passed to hang_detect
    pub grace_period: Duration,            // hang → deadlock window
    pub session_factory: Option<SessionFactory>,
                                          // pooled/respawn scenarios
}

/// What drive() reports back to the orchestrator.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScenarioOutcome {
    pub total_calls: u64,
    pub successful_calls: u64,
    pub hang_count: u32,
    pub deadlock_count: u32,
    pub error_count: u64,
    pub divergence_count: u64,             // first-class correctness gate
    pub incomplete_worker_count: u64,      // requested pool workers that never completed
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
> because `Session::call_tool` takes `&mut self` — the first-cut scenarios
> issued sequential calls against a single session. The shipped solution is
> an internal pool of independent factory-created sessions; `reconnect`
> remains future work.

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
    pub rss_leak_mb_per_sec: Option<f64>,
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
    /// Every configured threshold satisfied and all scenario/lifecycle
    /// evidence is complete (calls succeeded; no deadlock, divergence,
    /// incomplete worker, teardown uncertainty, or terminal protocol error).
    pub fn passed(&self) -> bool;
}
// Rendering goes through the `Reporter` trait
// (report/{markdown,json,html,terminal}.rs), not write_* methods on Report.

// metrics/types.rs — snapshots produced by Recorder
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
    pub successful_requests: u64,          // Success + ExpectedRejection
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

`ProcessStats` is currently a single-PID observation. When a stdio scenario
uses factory-spawned children (pool, cold start, version matrix, or fuzzer
respawn), `Run` clears the irrelevant initial-child sample and records a scope
note. Configured memory-growth/leak thresholds then fail closed as unavailable;
they never pass using the idle initial process. Aggregate child-PID sampling is
future work.

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
transport / response-shape or version errors / id-mismatch /
startup-timeout / schema-violation. Response parsing requires `jsonrpc:
"2.0"`, a JSON-RPC string/number/null id, and exactly one of `result` or
`error`; mismatched-id payloads remain available to the raw fuzzer for
fail-closed acceptance classification.
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

Reusable per-call watchdog. `deadlock_probe`, `race_check`, and selected
latency-sensitive paths use it; it is not automatically wrapped around every
transport call:

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

The Vibe-Trading-bug-class detector. The quick and config forms use the same
synchronized implementation:

```
Algorithm: deadlock_probe(server, tool, N, hang_threshold)
1. Run connects a primary session and lists tools for discovery/coverage.
2. If N = 1, issue one call on that borrowed session.
3. If N > 1, require `RunContext::session_factory`; direct library contexts
   without one are rejected instead of silently serializing.
4. Spawn/connect N independent sessions. Each completes the selected
   revision's handshake/discovery before reaching the pool's shared start gate.
5. Release the gate and issue one `tools/call` per worker.
6. Wrap every worker call in `hang_detect(hang_threshold, grace_period)`.
7. Categorize each:
     - Ok with duration → SUCCESS
     - Late response within grace_period → SLOW
     - No response after grace_period → DEADLOCK
8. Join all workers and aggregate their structured outcomes.
9. Shut down each worker with a bounded timeout.
10. Report:
     - if DEADLOCK count > 0 → severity=CRITICAL, "DEADLOCK DETECTED"
     - else if calls are slow/errors → expose their structured counts
     - else → severity=PASS
```

For a focused first-call diagnosis, set `concurrent = 1`. For N>1 the field
means genuine synchronized independent-session concurrency.

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
  require the corresponding recorder/process sample set
  if evidence is missing: append an unavailable ThresholdViolation
  compare report's metric to threshold
  if violated: append ThresholdViolation { metric, expected, actual }
Return: violations vec.
```

`Report::passed()` also applies unconditional correctness gates: at least one
call must be attempted and succeed; deadlocks/divergences and incomplete
pooled-worker cohorts fail; recorded deadlock/timeout/protocol/malformed/
crash/disconnect/cancellation outcomes fail. Any error in a `race_check` or
`deadlock_probe`/`fuzzer` diagnostic also fails; so does any completed
diagnostic call that exceeds `hang_threshold`, including a mixed cohort where
other calls succeeded. Partial slow calls and application errors in normal
load scenarios are governed by configured threshold policy.

The run-level initial `tools/list` is also a protocol precondition, not a
best-effort coverage hint. Its failure tears the session down and returns a
run error before scenario traffic in both permissive and strict-schema modes.

---

## 16. Mock server specs

Mocks live in `tests/fixtures/<name>.py`. They are intentionally small Python
servers using stdio + JSON-RPC by hand (no fastmcp dependency, to avoid version
coupling). Shipped fixtures: `mock-normal.py`, `mock-slow.py`,
`mock-broken.py`, `mock-crash.py`, `mock-leak.py`, `mock-error.py`,
`mock-slow-init.py`, `mock-malformed.py`, plus `mock-http-server.py` and
`mock-sse-server.py` (transport parity coverage), `mock-schema.py` /
`mock-output-schema.py` (strict args/result validation — ADR 0010), and
`mock-stateless-http.py` (2026-07-28 stateless mode — ADR 0019). Shared
helpers live in `_common.py`; the canonical inventory is
`crates/engine/tests/fixtures/CLAUDE.md`. Pseudocode for the original eight +
transport pair follows.

### 16.1 mock-normal.py

```python
# Echoes args and rejects malformed JSON without exiting.
while True:
    line = sys.stdin.readline()
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        respond_error(None, -32700, "parse error")
        continue
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

### 17.1 Trace format (`mcp-trace/1`)

`run --trace <file>` writes to the exact requested path. The first JSONL line
is a header:

```json
{"format":"mcp-trace/1","run_id":"01HXY...","server":"python -m my_mcp","started_at":"2026-07-28T00:00:00Z"}
```

Every later line is a raw wire frame:

```json
{"dir":"c2s","elapsed_ms":12,"method":"tools/call","body":"{\"jsonrpc\":\"2.0\",\"id\":3,...}"}
{"dir":"s2c","elapsed_ms":18,"method":"tools/call","body":"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":...}"}
```

`dir` is `c2s` or `s2c`; `body` is a JSON string holding the wire payload,
not an embedded object. Response frames inherit the method label of the
request they answer. Secret-looking keys under client
`params.arguments` are redacted by default, but server responses are not
redacted. The trace contains frames only—hang/deadlock classifications and
process samples live in the report/metrics instead.

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

Every call is classified into exactly one outcome. Failure outcomes are used
for `ErrorStats.by_category`; the fuzz-only `ExpectedRejection` outcome is a
successful probe and is reported separately.

| Category        | Definition                                                                            | Example                              |
| --------------- | ------------------------------------------------------------------------------------- | ------------------------------------ |
| `Hang`          | No response within `hang_threshold`, but response arrived before grace_period expires | tool genuinely slow under contention |
| `Deadlock`      | No response after `hang_threshold + grace_period`                                     | Vibe-Trading PR #85                  |
| `Timeout`       | Client-side configured deadline exceeded (separate from hang_threshold)               | network buffer full                  |
| `ServerError`   | Unexpected JSON-RPC server failure, including `-32603` and server-defined errors       | fuzzer triggered an internal error   |
| `ProtocolError` | Unexpected protocol/spec failure outside an intentional fuzz rejection                | malformed response or id mismatch    |
| `Crash`         | Server process exited (non-zero or signal) during call                                | unhandled panic                      |
| `Malformed`     | Response was not valid JSON or didn't match JSON-RPC schema                           | partial response, broken framing     |
| `Disconnected`  | Transport closed unexpectedly mid-call                                                | broken pipe                          |
| `Cancelled`     | Client cancelled the request before response                                          | scenario shutdown                    |
| `ExpectedRejection` | Fuzz probe was explicitly rejected with `-32700`, `-32600`, `-32601`, `-32602`, or `tools/call` `isError: true` | healthy protocol-fuzz rejection |

`-32603` (`Internal error`) is never an expected fuzz rejection. A raw-frame
liveness probe that cleanly survives without an explicit rejection is recorded
as `Success`, not `ExpectedRejection`.

Classification precedence is scenario/error-mapper specific. `mcp-trace/1`
records wire frames, not synthetic hang/crash events; consult
`metrics.json` and `ScenarioOutcome` for classifications.

---

## 19. Performance targets for the tool itself

`mcp-loadtest` should never be the bottleneck.

| Aspect                                         | Target                                |
| ---------------------------------------------- | ------------------------------------- |
| Driver per-request CPU overhead                | < 50µs (excluding JSON serialization) |
| Memory per concurrent worker                   | < 100KB                               |
| Max sustainable concurrency on a 4-core laptop | ≥ 1000 workers                        |
| Trace file write throughput                    | ≥ 100k events/sec                     |
| Histogram update                               | sharded, lock-protected state         |

These are tested in `benches/` (criterion); reproduce with `cargo bench --workspace`.

---

## 20. Versioning + stability policy

- v0.x: API can change anywhere
- v1.0: locked. Breaking changes require major version bump (semver strict)
- MCP spec: `protocol_version` field in `initialize` is checked. Mismatch warns but does not fail by default. Override with `--strict-protocol`.
- Library MSRV is pinned to Rust 1.88 and checked explicitly in CI.

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

The stdio serve mode exposes exactly three tools:

| Tool               | Args                                                            | Returns                                            |
| ------------------ | --------------------------------------------------------------- | -------------------------------------------------- |
| `deadlock_probe`   | `server_command`, `tool`, `concurrent`                          | `{ deadlock_count, hung_for_ms[], details }`       |
| `sustained_load`   | `server_command`, `concurrent`, `duration_secs`, `tool`, `args` | `{ p50_ms, p99_ms, error_rate, requests_per_sec }` |
| `compare_runs`     | `baseline_run_dir`, `current_run_dir`                           | structured diff with regression flags              |

A user can ask an MCP-aware agent to call `deadlock_probe` and receive
structured JSON. The operator remains responsible for authorizing the server
command and reviewing filesystem/process effects.

**Reaatech doesn't do this.** It's our most under-priced differentiator.

### 21.3 Actionable error messages with hints

Every `Err` returned to the user includes a suggested next step:

```
Error: server stdin closed unexpectedly during initialize handshake.
Hint: server may have crashed before responding. Check stderr at:
      runs/01HXY.../server-stderr.log
      Or use config-driven `run --tee-stderr` to see it live.
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
  1. Spawn/connect, initialize (or discover), and complete the required
     initial `tools/list` within one startup_timeout budget (default 10s).
  2. Send `notifications/initialized` as part of handshake-mode startup.
  3. Retain the discovered tool registry for startup and strict validation.
  4. Start the configured deadlock probe.
  5. For N>1, complete N independent sessions and release one `tools/call`
     per worker through a shared start gate. N=1 uses the primary session.
  6. Each call wrapped in hang_detect(hang_threshold, grace_period):
     - response within hang_threshold → SUCCESS
     - response between threshold and grace_period → SLOW (warning)
     - no response after grace_period → DEADLOCK (critical)
  7. Bail on the first DEADLOCK — the session is wedged — and report.

Defaults: this subcommand probes quickly (N=5, hang_threshold=2s,
  grace_period=5s); `run` with `[scenario] type = "deadlock_probe"` is the
  thorough CI form (N=20, hang_threshold=5s, grace_period=10s).

Tunable knobs: --concurrent, --hang-threshold, --grace-period.
See DESIGN.md §15.2 for the spec source (and its "Shipped reality" note).
```

LLMs use this to plan the right invocation. Reduces "I tried it but it didn't do what I expected" loops.

### 21.5 JSON Schema published for outputs

`docs/schema/metrics.v1.json` is shipped and pinned to the JSON reporter by a
conformance test. Scenarios expose config-schema fragments in Rust, but a
complete standalone `config.v1.json` has not shipped.

### 21.6 `mcp-loadtest doctor`

Diagnoses common setup issues:

- Python interpreter not on PATH (for fixture-based tests).
- MSVC vs GNU toolchain mismatch on Windows.
- Stale `runs/` accumulation.
- MCP server fails initialize — captures stderr and reports.

Outputs a checklist with ✅/❌ per item and a one-line fix per ❌. Exactly the kind of thing an LLM agent can chain into a fix-it loop.

### 21.7 Trace format is LLM-readable

The exact file passed to `run --trace <file>` is line-oriented
`mcp-trace/1` JSON with stable field names (DESIGN.md §17.1). It is pipeable
through `jq`:

```bash
$ jq 'select(.method=="tools/call") | {dir, elapsed_ms, body}' trace.jsonl
```

### 21.8 Reports include "What this means" interpretation

A report that says `p99 latency: 234ms` is data. A report that adds `"95% of users would call this acceptable; the slow tail (top 1%) is concentrated on `analyze_options` calls"` is information. We aim for the latter — derived sentences, not just numbers.

### 21.9 Snapshot tests for output formats

`insta::assert_snapshot!` on report markdown / JSON. Output shapes are stable across releases unless explicitly changed (with CHANGELOG entry). LLM agents that parse our output don't break across patch versions.

### 21.10 Cookbook in `docs/examples/`

Per-scenario copy-pasteable commands + expected output. LLMs train on README-style examples; cookbook entries make those examples concrete and executable.

Candidate cookbook targets:

- "Find deadlocks in my new MCP server"
- "Add a regression gate to my CI"
- "Compare two implementations of the same MCP server"
- "Detect a memory leak before production"
