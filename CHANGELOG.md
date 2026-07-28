# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

No changes have been assigned beyond `v0.1.0`.

## [0.1.0] — Release contents

This section describes the `v0.1.0` source contents. Its presence in a checkout
does not prove that a tag, GitHub Release, prebuilt binary, or Action release
exists; verify the exact tag, published assets, and GitHub immutable-release
status externally. The release workflow refuses publication while immutable
releases are disabled.
crates.io is not a `v0.1.0` distribution channel. The earlier `0.0.1`
workspace version was never tagged or released.

### Security

- Removed shell evaluation from the composite GitHub Action. The `args` input
  is now a JSON array of strings, decoded into literal argv entries; invalid
  JSON, non-string entries, NUL bytes, and shell syntax fail closed.
- The Action now defaults the installed binary to `v0.1.0` and accepts only a
  resolved stable `vMAJOR.MINOR.PATCH` tag before writing to GitHub environment
  files, blocking multiline command-file injection and mutable-ref surprises.
- Added `server.headers_from_env` for HTTP/SSE/WebSocket authentication
  headers. Configuration stores only environment-variable names; resolved
  secret values are not included in config debug output or error messages.
  Static outbound headers are the complete remote-auth scope for
  `v0.1.0`—OAuth login, refresh, discovery, and interactive authorization
  are not implemented. All `MCP-*`, `Sec-WebSocket-*`, hop-by-hop, and proxy
  headers remain transport-owned and cannot be overridden. Supplying any
  remote header requires `https://` for HTTP/SSE or `wss://` for WebSocket;
  URL userinfo is forbidden. Query strings still reach the target but are
  redacted wholesale in reports/traces and must not carry credentials.
- Remote URL validation is now shared by config parsing and the direct
  HTTP/SSE/WebSocket constructors, including the server-selected SSE POST
  endpoint. Unknown `[server]` keys fail closed without echoing literal
  values, case-insensitive duplicate header names are rejected at connect,
  and secret-backed header values remain marked sensitive in both reqwest and
  WebSocket requests.

### Fixed

- Stdio `Session` now tolerates server-initiated JSON-RPC notifications
  (`notifications/tools/list_changed`, progress updates, etc.) interleaved
  with responses — including a notification emitted before the `initialize`
  result. Previously the transport read the next line as the response, so a
  leading notification desynced the stream and every call was misclassified
  `malformed` (found by dogfooding against the MCP reference "everything"
  server: 100% malformed → 0 errors after the fix). The response read now
  skips notification frames (a `method`, no `id`) up to a bounded cap.
- JSON-RPC responses now fail closed unless `jsonrpc` is exactly `"2.0"`,
  the response id is a valid JSON-RPC string/number/null value, and the
  envelope contains exactly one of `result` or `error`. Mismatched-id success
  payloads and structured errors are retained internally so the raw fuzzer can
  distinguish an explicit allowed rejection from a server that accepted
  malformed input.
- `compare` now exits non-zero when any regression flag fires, as `--explain`
  and the composite GitHub Action's gating contract (DESIGN.md §15.4) always
  documented. Previously it printed the diff and exited 0, so regressions
  passed CI. The diff is still rendered to stdout first; the failure line
  names the regressed metrics.
- Config validation now rejects `transport = "ws"` without `server.url` at
  load time (previously the missing URL only surfaced at connect).
- `deadlock_probe` now completes all N independent session handshakes before
  a shared start gate releases one call per worker when `concurrent > 1`.
  `concurrent = 1` keeps the focused borrowed-session path; a direct library
  context without `SessionFactory` rejects N>1 instead of silently
  serializing. `--explain` documents the synchronized behavior and the
  quick-subcommand vs `run`-config defaults
  (5 / 2s / 5s vs 20 / 5s / 10s).
- Pattern workloads now use the same real N-session pool as the single-tool
  sustained path when `concurrent > 1` and a `SessionFactory` is available.
  A direct-library context without a factory reports its sequential fallback
  instead of silently claiming N-way concurrency.
- `race_check` now requires at least two independent sessions, completes all
  handshakes before releasing one identical call per worker through a shared
  gate, and records response divergence as a first-class CI failure. A missing
  or errored worker makes the cohort inconclusive and fails closed; partial
  responses are never reported as a clean no-divergence result.
- `Report::passed()` now rejects no-op runs, all-failed runs, deadlocks,
  response divergences, incomplete pooled-worker/race-check cohorts, and
  uncertain session teardown. It also rejects recorded deadlock, timeout,
  protocol/malformed, crash, disconnect, or cancellation outcomes even when
  no threshold was configured. Any error or completed call that breaches the
  hang threshold in a `race_check`, `deadlock_probe`, or `fuzzer` diagnostic
  also fails—even in a mixed cohort with surviving successes. Partial slow
  calls and application errors in normal load scenarios remain governed by
  configured threshold policy.
- Session teardown now closes stdin, applies bounded graceful-exit and
  forced-kill/reap phases, joins or aborts the stderr pump without detaching
  it, and records any teardown error/timeout as a typed report-gating signal.
  Run duration is finalized after shutdown; trace replay also exits non-zero
  when target teardown is uncertain. Scenario and test outer deadlines leave
  scheduling margin above the transport's composed internal budget.
- Captured stderr for pooled and cold-start factory sessions now uses one
  immutable file per session under
  `runs/<id>/server-stderr/session-NNNNNN.log`; concurrent workers can no
  longer truncate the shared initial `server-stderr.log`.
- Every run now fails before scenario traffic when its initial `tools/list`
  discovery cannot be obtained. A server that rejects discovery but accepts a
  known `tools/call` can no longer report PASS; strict mode additionally
  reuses the successful registry for schema validation.
- Configured process-memory thresholds fail closed when process samples are
  missing, insufficient, non-finite, or unusable; config parsing also rejects
  NaN and positive/negative infinity. Unset process thresholds remain
  best-effort and do not gate platforms without observable process data.
- Configured global latency/error gates and per-tool SLOs now require recorder
  samples. A custom or incomplete scenario can no longer satisfy a threshold
  with default-zero metrics, and an unexercised tool SLO is a typed violation.
- Process metrics are cleared and configured process thresholds fail closed
  when pooled, cold-start, version-matrix, or fuzzer traffic creates stdio
  factory children outside the current single-PID sampler; an idle initial
  child can no longer produce PASS.
- `cold_start` rejects `warmup = true` with fewer than two iterations because
  that plan has no measured evidence. Zero-length optional spike phases are
  skipped instead of spawning workers that can only look incomplete.
- Healthy protocol-fuzzer rejection now has its own `ExpectedRejection`
  metric. It counts as a successful fuzz probe without masking genuine
  protocol/malformed errors or client-side schema/version failures.
- The normal stdio fixture now returns JSON-RPC `-32700` for malformed JSON
  instead of crashing. A repeat-suite transient showed the old crash racing
  the raw fuzzer's liveness deadline and appearing nondeterministically as
  either `Disconnected` or `Deadlock`; the regression now requires an explicit
  rejection with zero disconnects and zero deadlocks.
- Process-heavy nextest contention pushed pooled Python fixture initialization
  past the test default, so the deadlock integration test now uses a test-local
  30-second startup budget. The production 10-second default and strict
  complete-worker/pass/fail assertions are unchanged.
- The no-retry release repeat gate exposed nine Windows stdio shutdown
  failures across two of five attempts. Tokio's registered process-wait
  callback could lag the process table during the 24-way subprocess wave, so
  an accepted termination was misreported as a 2-second forced-reap timeout.
  Stdio teardown now reconciles both graceful and forced async-wait failures
  with a direct `try_wait` probe: a proven exit/reap succeeds, a still-live
  process or failed probe remains a typed failure. A follow-up strict run
  proved that some processes really remained live after the old 2-second
  forced budget, so forced reap now has a bounded 5 seconds and its outer
  lifecycle guards retain scheduling margin. Nextest limits engine
  integration-test binaries to four at once while a focused regression still
  drives eight stubborn child shutdowns concurrently; scenario-level
  concurrency tests retain their real worker counts. The release procedure
  keeps the original failed JUnit and logs as unresolved local evidence rather
  than discarding them.
- The hang watchdog now classifies successful calls from elapsed wall time,
  not merely the winning `select!` branch. Under a saturated executor the call
  future and threshold timer could become ready in the same poll; the old
  future-first bias produced `Ok` after the threshold. Deterministic scheduler
  stall regressions require `Slow` or `Deadlock` at the real deadlines.
- A Run's `server.startup_timeout` is one deadline across transport
  connect/spawn, initialize or stateless discover, and the mandatory initial
  `tools/list`; strict factory workers reuse the same rule. Header/upgrade and
  tools-list stalls now return the configured typed timeout and run bounded
  teardown instead of hanging without a report.
- Remote reader failures are latched outside the foreground response queue.
  SSE/WS aggregate-budget, parser, socket, and unexpected-close failures that
  arrive after a matching response therefore still gate teardown instead of
  being hidden by an earlier success.

### Changed

- Workspace version is `0.1.0`; a manifest version alone is not evidence that
  an external release exists.
- The GitHub Action's `args` contract intentionally breaks from shell-quoted
  text to a JSON array of strings. Callers must migrate, for example:
  `args: '["--tool","echo","--args","{\"message\":\"hi\"}"]'`.
- Grading: the concurrency dimension's note now discloses inline that
  `total_requests` is a proxy for concurrency capacity (e.g.
  `sustained requests 1234 -> A (>= 100; proxy for concurrency)`).
- CI/repeat-test scripts emit JUnit plus per-attempt logs under
  `target/nextest/` and `target/test-artifacts/`; CI uploads those diagnostics
  even when a test attempt fails. Repeat output directories cannot retain a
  stale per-run JUnit file, and a failed log write fails the Bash evidence
  run. Each evidence set also records tool/OS/git status and Git object hashes
  for every tracked or untracked source file, so a transient can be tied to
  the exact dirty source tree without copying source contents.
  `DISPOSITION.txt` marks a green no-retry cohort or makes failed attempts an
  explicitly unresolved release blocker pending root-cause analysis.
- Captured stderr keeps the orchestrator's initial session at
  `runs/<id>/server-stderr.log` and writes factory-spawned pooled/cold-start
  sessions to unique files below `runs/<id>/server-stderr/`.
- Release automation now grants write permission only to one final publisher:
  every platform build is collected first, all four archives and checksums are
  verified, and a draft is made public only after all eight assets exist.
  Workflow actions are pinned to reviewed commit SHAs.

### Added

`v0.1.0` is a cross-platform load tester and bug detector
for MCP (Model Context Protocol) servers. Its primary promise is deterministic
CI gating for protocol-level hangs, deadlocks, response divergence, terminal
transport failures, and configured performance thresholds.

#### Protocol & transports

- JSON-RPC 2.0 / MCP protocol stack with a zero-copy hot path (ADR 0006):
  borrowing request types, `Session::call_tool(&str, &Value)` — no per-call
  deep clone of the args tree.
- `Session` — spawn/connect, `initialize` handshake, `tools/list`,
  `tools/call`, and shutdown; plus `SessionFactory`, a public cloneable
  factory producing fresh sessions over the run's configured transport
  (version-aware via `SessionFactory::with_version`). Run applies
  `server.startup_timeout` across transport connect, initialize/discover, and
  its required initial `tools/list` rather than resetting or dropping the
  deadline between phases.
- Four transports behind the `Transport` trait: stdio (child process spawn,
  `SpawnOptions` stderr disposition — ADR 0013), Streamable HTTP, HTTP+SSE,
  and WebSocket (rustls). Remote-controlled HTTP response bodies, SSE event
  data, and WebSocket messages are rejected above 16 MiB while the network
  stream is being consumed; SSE/WS also enforce a 32 MiB aggregate byte
  budget across their reader queues and id-mismatch buffers.
- Multi-version MCP protocol support (ADR 0018): typed `ProtocolVersion` enum
  covering `{2025-03-26, 2025-06-18, 2025-11-25}`; optional
  `[server] protocol_version = "auto" | "<rev>"` pin (useful for CI
  version-matrix runs); default advertised revision `2025-11-25`;
  `MCP-Protocol-Version` header on Streamable HTTP (2025-06-18+ requirement).
  A server answering with a revision outside the supported set logs a warning
  and, under `[validation] strict = true`, fails the run before any scenario
  traffic.
- Experimental stateless `2026-07-28` connection mode (ADR 0019), selected
  only by an explicit version pin and never used as the default. The
  implemented subset is reconciled to the final specification tag at commit
  `5f5440bb26a62e2cf3440b92da5a667efa03b267` and conformance commit
  `49103de6ed70804e940637bf3e9e29e4a3f54e64`. The final definitions used by
  this client match the reviewed pre-final schema definitions, so no
  tools/discovery/request-header wire change was required; the final-only
  `subscriptions/listen` response-envelope delta is outside the implemented
  scope. It sends no `initialize`, follows the final request
  metadata/discovery contract, and is scoped to stdio + Streamable HTTP
  (SSE/WS rejected at config load).
- Reproducible Unix/Windows official-conformance runners pinned to
  final specification commit
  `5f5440bb26a62e2cf3440b92da5a667efa03b267` and conformance commit
  `49103de6ed70804e940637bf3e9e29e4a3f54e64`, covering request metadata,
  `tools_call`, standard/custom HTTP headers, and invalid tool headers. The
  latter is the latest official harness at verification, but it still marks `2026-07-28`
  DRAFT/provisional and vendors the pre-final schema from specification commit
  `71e306956a4959c9655e5036be215d41986596e6`; the machine-checked final
  tag/schema comparison is the separate final-spec reconciliation proof. The
  runners verify that the final tag resolves to the reviewed commit, fail if
  upstream conformance `main` no longer equals the reviewed pin, and retain
  `FINAL_SCHEMA_RECONCILIATION.txt`, the upstream status, complete official
  client inventory, and an executed/not-executed scope manifest. A passing
  retained run is a
  five-scenario tools/discovery/metadata/header gate; it is not a
  full-protocol, auth, MRTR/request-state, `subscriptions/listen`,
  schema-reference, server, authorization-server, or final-promoted-suite
  conformance claim (ADR 0023).
- `Transport::raw_send(&[u8])` — raw, unframed bytes on the wire (stdio
  writes verbatim + newline), powering the fuzzer's raw-byte payloads.
- `hang_detect` — reusable two-phase watchdog used by deadlock/race and
  selected latency-sensitive scenario paths, classifying Ok / Slow /
  Deadlock / Err (hang threshold + grace period). It is not a universal
  transport wrapper.

#### Scenarios

- `sustained` — constant load; `concurrent > 1` drives a true N-worker
  session pool (ADR 0017), with an honest, disclosed sequential fallback.
- `deadlock_probe` — the Vibe-Trading-bug-class detector; for N>1, releases
  one `hang_detect`-wrapped call per independent session through a shared
  start gate (`concurrent = 1` remains the focused single-session path).
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
- `race_check` — N synchronized calls on independent sessions, with identical
  responses canonicalized and compared; any divergence gates the run.
- `fuzzer` — enumerated malformed-but-plausible payloads plus raw-byte
  variants (EmptyBody, InvalidJson, missing/duplicate ids, ...) with
  poisoned-session respawn between iterations.
- `version_matrix` — the same server driven once per MCP protocol revision,
  outcomes diffed side by side under per-tool metric keys `version:<rev>`
  (ADR 0018).

#### Metrics & analysis

- `Recorder` — Arc-shared atomic outcome counters plus sharded,
  lock-protected hdrhistogram latency state (p50/p95/p99/p999, microsecond
  resolution), with per-tool counters (`record_tool` /
  `snapshot_per_tool`).
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
- Ratatui dashboard as a library component behind cargo feature `tui`.
  No CLI flag or subcommand exposes it in `v0.1.0`.

#### Trace record & replay (ADR 0021)

- `run --trace <file>` records every JSON-RPC frame of the run (handshake
  included) as versioned `mcp-trace/1` JSONL, with secret-looking
  `tools/call` argument values redacted by default.
- Explicit trace artifacts now finalize fail-closed: frame
  serialization/write/flush failures are latched across every session-writer
  clone and prevent a successful report or `trace_path` claim.
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
  `Hint:` lines printed after error chains. The `run` subcommand's
  `--capture-stderr` / `--tee-stderr` flags redirect the spawned stdio
  server's initial stderr to `runs/<id>/server-stderr.log` and factory
  sessions to unique files under `runs/<id>/server-stderr/` (ADR 0013).

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
- Composite GitHub Action (`action.yml`) for an exact
  `uses: Teerapat-Vatpitak/mcp-loadtest@v0.1.0` pin. It installs a
  sha256-verified release binary (with `cargo install --git` fallback), runs
  `deadlock-probe`/`run`/`cross`/`doctor`, optionally compares a baseline
  `metrics.json`, and appends a job summary. External availability requires
  both the exact tag and its GitHub Release with GitHub immutable releases
  enabled; this source text alone is not availability evidence.
- Release automation is prepared for `cargo install --git` plus prebuilt
  GitHub Release binaries, subject to the hard gates in `docs/RELEASE.md`.
  The workflow refuses to publish unless GitHub immutable releases is enabled
  (verified before and immediately before publication using a separately
  provisioned read-only Administration audit token) and invokes the complete
  reusable Action contract (cross-platform argv plus composite end-to-end
  checks), not only the parser smoke test.
  crates.io publication remains a separate, irreversible maintainer decision
  and is not part of the `v0.1.0` distribution.
- Python mock-server fixtures (stdlib-only) covering
  normal/slow/broken/crash/leak/error/slow-init/malformed/schema/http/sse/
  stateless behaviors, a regression test against the real Vibe-Trading
  deadlock commit, and criterion microbenchmarks
  (record/histogram/session_loopback/hang_detect).

[Unreleased]: https://github.com/Teerapat-Vatpitak/mcp-loadtest/commits/main
