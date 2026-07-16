# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `compare` now exits non-zero when any regression flag fires, as `--explain`
  and the composite GitHub Action's gating contract (DESIGN.md §15.4) always
  documented. Previously it printed the diff and exited 0, so regressions
  passed CI. The diff is still rendered to stdout first; the failure line
  names the regressed metrics.
- Config validation now rejects `transport = "ws"` without `server.url` at
  load time (previously the missing URL only surfaced at connect).
- `deadlock-probe --explain` (and its source, DESIGN §21.4) now describe the
  shipped sequential probe — the barrier-released concurrent burst remains
  M8+ backlog per DESIGN §15.2 — and document the quick-subcommand vs
  `run`-config defaults (5 / 2s / 5s vs 20 / 5s / 10s).

### Changed

- Grading: the concurrency dimension's note now discloses inline that
  `total_requests` is a proxy for concurrency capacity (e.g.
  `sustained requests 1234 -> A (>= 100; proxy for concurrency)`).

## [0.0.1] — 2026-07-11

First release. `mcp-loadtest` is a cross-platform load tester and bug detector
for MCP (Model Context Protocol) servers: it catches lazy-init deadlocks,
concurrency races, hangs, memory leaks, and perf regressions that unit tests
miss, and gates CI with a non-zero exit on any violation.

### Added

#### Protocol & transports

- JSON-RPC 2.0 / MCP protocol stack with a zero-copy hot path (ADR 0006):
  borrowing request types, `Session::call_tool(&str, &Value)` — no per-call
  deep clone of the args tree.
- `Session` — spawn/connect, `initialize` handshake (honoring
  `server.startup_timeout`), `tools/list` / `tools/call` / shutdown; plus
  `SessionFactory`, a public cloneable factory producing fresh sessions over
  the run's configured transport (version-aware via
  `SessionFactory::with_version`).
- Four transports behind the `Transport` trait: stdio (child process spawn,
  `SpawnOptions` stderr disposition — ADR 0013), Streamable HTTP, HTTP+SSE,
  and WebSocket (rustls; 16 MB per-frame cap mirroring the stdio line cap).
  Bounded id-mismatch buffers on SSE/WS.
- Multi-version MCP protocol support (ADR 0018): typed `ProtocolVersion` enum
  covering `{2025-03-26, 2025-06-18, 2025-11-25}`; optional
  `[server] protocol_version = "auto" | "<rev>"` pin (useful for CI
  version-matrix runs); default advertised revision `2025-11-25`;
  `MCP-Protocol-Version` header on Streamable HTTP (2025-06-18+ requirement).
  A server answering with a revision outside the supported set logs a warning
  and, under `[validation] strict = true`, fails the run before any scenario
  traffic.
- Stateless `2026-07-28` connection mode (ADR 0019, implemented against the
  release candidate; final-spec reconciliation scheduled 2026-07-29): no
  `initialize` is sent; every request carries the RC `_meta` block, and one
  bounded `server/discover` probes connectivity at construct. stdio and
  Streamable HTTP only (SSE/WS rejected at config load).
- `Transport::raw_send(&[u8])` — raw, unframed bytes on the wire (stdio
  writes verbatim + newline), powering the fuzzer's raw-byte payloads.
- `hang_detect` — two-phase watchdog wrapping every call in every scenario,
  classifying Ok / Slow / Deadlock / Err (hang threshold + grace period).

#### Scenarios

- `sustained` — constant load; `concurrent > 1` drives a true N-worker
  session pool (ADR 0017), with an honest, disclosed sequential fallback.
- `deadlock_probe` — the Vibe-Trading-bug-class detector; wraps each call in
  `hang_detect` and bails on the first deadlock.
- `cold_start` — fresh server respawned per iteration via `SessionFactory`;
  spawn-to-`initialize` handshake recorded under `cold_start:handshake`.
- `ramp` — stepped concurrency, each step its own session pool; feeds
  breaking-point analysis.
- `soak` — long-duration steady load with periodic snapshots and leak
  signals.
- `spike` — baseline, burst, cooldown; each phase its own pool with all
  burst workers joined before cooldown.
- `pattern` — multi-step weighted-random tool-call mixes with think-time and
  `ErrorBehavior`; also drives `sustained`'s multi-pattern form.
- `race_check` — N identical calls diffed to surface non-determinism.
- `fuzzer` — enumerated malformed-but-plausible payloads plus raw-byte
  variants (EmptyBody, InvalidJson, missing/duplicate ids, ...) with
  poisoned-session respawn between iterations.
- `version_matrix` — the same server driven once per MCP protocol revision,
  outcomes diffed side by side under per-tool metric keys `version:<rev>`
  (ADR 0018).

#### Metrics & analysis

- `Recorder` — Arc-shared, lock-free outcome counters + sharded hdrhistogram
  latencies (p50/p95/p99/p999, microsecond resolution) with per-tool
  counters (`record_tool` / `snapshot_per_tool`).
- Breaking-point detector (first-violator semantics on per-step deltas) and
  A-F performance grading (worst-of-three rollup).
- Race detector (key-sorted JSON canonicalization) and coverage report
  (registered vs exercised tools, `coverage_pct`).
- Fuzz-report classification (`FuzzClass` + `has_critical` signal).
- Regression compare with configurable thresholds (ADR 0009):
  `--max-p99-regression-pct` / `--max-error-rate-regression-pp` /
  `--allow-deadlock-increase`, mirrored as `compare_runs` MCP tool args.
- Process sampling (sysinfo): RSS/CPU/fd/threads over time. Thresholds for
  p50-p999 latency, error rate, absolute memory growth (peak minus
  baseline), least-squares RSS leak slope
  (`thresholds.rss_leak_mb_per_sec`), and per-tool SLOs (`ToolSlo`).

#### Reporting

- Markdown, JSON, ANSI terminal, and self-contained HTML reporters (inline
  SVG histogram, escaped HTML, no external CDN or JS).
- `docs/schema/metrics.v1.json` — the JSON Schema for `metrics.json`, pinned
  to the JSON reporter's real output by a conformance test.
- Live TUI dashboard (ratatui; cargo feature `tui`).

#### Trace record & replay (ADR 0021)

- `run --trace <file>` records every JSON-RPC frame of the run (handshake
  included) as versioned `mcp-trace/1` JSONL, with secret-looking
  `tools/call` argument values redacted by default.
- `replay <trace-file> --server "cmd"` (or `--url`/`--transport`/
  `--allow-host`) re-sends the recorded client frames against a fresh server
  and diffs the responses via canonical JSON (ids ignored), exiting non-zero
  on divergence. Public `mcp_loadtest::trace` module (`TraceWriter`,
  `TracingTransport`, `ReplayReport`, `TraceError`).

#### CLI

- Subcommands: `run`, `deadlock-probe`, `cross` (N servers side by side),
  `compare` (baseline regression diff), `replay`, `doctor` (setup
  diagnostics with a per-item fix line — ADR 0014), `example-config`,
  `list-scenarios`, and `serve --mcp` — the self-hosted MCP server exposing
  `deadlock_probe` / `sustained_load` / `compare_runs` so AI agents can
  drive load tests directly.
- `--explain` on every subcommand (static algorithm text); actionable
  `Hint:` lines printed after error chains; `--capture-stderr` /
  `--tee-stderr` redirecting the spawned server's stderr to
  `runs/<id>/server-stderr.log` (ADR 0013).

#### Security

- Opt-in strict schema validation (ADR 0010): `tools/call` arguments are
  validated against the server's advertised `inputSchema` (violations gate
  the run); each result's `structuredContent` is validated against the
  tool's `outputSchema` (warn-only, never gates). Schema recursion is
  depth-bounded against maliciously deep server schemas.
- SSRF defense (ADR 0012): exact-match `[server].allowed_hosts` allowlist
  plus an always-on block of private/loopback/link-local/ULA/reserved
  addresses on the http/sse/ws transports (the SSE server-provided endpoint
  URL is re-checked); redirect policy is `none` (ADR 0007).
- DNS-rebinding defense via resolver pinning (ADR 0016): hostnames are
  resolved once at connect, every resolved address is vetted against the
  blocklist, and the vetted addresses are pinned for the actual connection.
- Supply-chain gates: `cargo deny` / `cargo audit` in CI with individually
  triaged, documented advisory ignores (ADR 0011).

#### Workspace & distribution

- Six layered crates (ADR 0022), strictly downward dependencies:
  `mcp-loadtest-core` (pure data) ← `mcp-loadtest-protocol` (wire) ←
  `mcp-loadtest-engine` (scenarios + run); core ← `mcp-loadtest-output`
  (renderers + TUI); `mcp-loadtest` facade (the public API surface +
  feature-gated `serve`); `mcp-loadtest-cli` (binary). MSRV 1.88.
- Composite GitHub Action (`action.yml`): one-line
  `uses: Teerapat-Vatpitak/mcp-loadtest@v0.0.1` CI integration — installs a
  sha256-verified prebuilt release binary (with `cargo install --git`
  fallback), runs `deadlock-probe`/`run`/`cross`/`doctor`, optionally gates
  against a baseline `metrics.json`, and appends a results section to the
  job summary.
- Distribution via `cargo install --git` + prebuilt GitHub Release binaries
  (ADR 0015); crates.io packaging metadata and `cargo-binstall` metadata in
  place ahead of a future crates.io publish (ADR 0020).
- Python mock-server fixtures (stdlib-only) covering
  normal/slow/broken/crash/leak/error/slow-init/malformed/schema/http/sse/
  stateless behaviors, a regression test against the real Vibe-Trading
  deadlock commit, and criterion microbenchmarks
  (record/histogram/session_loopback/hang_detect).

[Unreleased]: https://github.com/Teerapat-Vatpitak/mcp-loadtest/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/Teerapat-Vatpitak/mcp-loadtest/releases/tag/v0.0.1
