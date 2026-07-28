# 0021. Trace record + replay (`run --trace` / `replay`)

Date: 2026-07-07
Status: Accepted

## Context

The differentiators roadmap calls for recording every
JSON-RPC frame of a run to a trace file and replaying a recorded trace against
a server, diffing the responses. Four decisions need locking before code:
where the code lives (the plan said a `crates/mcp-trace` workspace crate), the
on-disk format + its versioning, what gets redacted (tool arguments routinely
carry secrets), and the replay semantics (what exactly is re-sent, and what
counts as a divergence).

## Decision

### 1. A `mcp_loadtest::trace` module, not a sister crate (amends the plan)

The recording tap is a decorator around the `Transport` trait, and it must be
constructible **inside `Run::execute`** so that `run --trace` records the
`initialize` handshake and every scenario frame. A separate `crates/mcp-trace`
would need `mcp-loadtest` as a dependency (for `Transport`), while
`mcp-loadtest` would need `mcp-trace` back (for the decorator) — a dependency
cycle. So the code lives in `crates/mcp-loadtest/src/trace/` (`format` /
`writer` / `replay` submodules, each under the 300-line convention). Spinning
the format + replay halves out into a crate later stays possible; only the
decorator is cycle-bound.

### 2. Trace format: JSONL, versioned `mcp-trace/1`

Line-delimited JSON. The first line is a header; every following line is one
frame.

```json
{"format":"mcp-trace/1","run_id":"01J...","server":"python mock.py","started_at":"2026-07-07T12:00:00Z"}
{"dir":"c2s","elapsed_ms":3,"method":"initialize","body":"{\"jsonrpc\":\"2.0\",...}"}
{"dir":"s2c","elapsed_ms":9,"method":"initialize","body":"{\"jsonrpc\":\"2.0\",...}"}
```

- **Header**: `format` (exactly `mcp-trace/1`), `run_id` (the run's ULID),
  `server` (stdio command line, or the URL for http/sse/ws), `started_at`
  (ISO 8601 UTC, second precision — reuses the report renderers' formatter).
- **Frame**: `dir` (`c2s` | `s2c`), `elapsed_ms` (monotonic milliseconds from
  run start), `method` (present when parseable; `s2c` response frames carry
  the *request's* method, since wire responses have none), `body` (the raw
  JSON-RPC object **as a string**).
- `body` is a string, not embedded JSON: it preserves the exact wire bytes
  (whitespace, key order) and can represent future non-JSON payloads (T3.1
  raw/malformed frames) without a format bump.
- **Versioning**: readers reject any header `format` other than `mcp-trace/1`.
  Additive fields are allowed within `/1`; anything breaking bumps to
  `mcp-trace/2`.

### 3. Redaction: default ON

Values of keys that look secret-bearing — lowercase key **contains** any of
`secret`, `token`, `password`, `api_key`, `apikey`, `authorization` — inside
`params.arguments` of client→server frames are replaced with `"[REDACTED]"`,
recursively (nested objects/arrays included). Known v1 limitations, accepted:

- Server→client frames are **not** redacted, so an echo-style server can leak
  a secret back into the trace via its response. Documented; response-side
  redaction is future work.
- A frame that needed redaction is re-serialized, so its byte layout (key
  order/whitespace) is normalized. Frames with nothing to redact are written
  byte-for-byte.

**Decision point:** the library writer takes a `redact: bool`, but the CLI
deliberately does **not** expose a `--no-redact` opt-out yet — shipping that
flag needs explicit user sign-off (it turns the trace file into a
secret-bearing artifact).

### 4. Replay: raw frames through a fresh `Transport`, no `Session`

A `Session` performs its own `initialize` handshake and mints its own ids, so
it cannot reproduce the recorded conversation. `replay` instead pushes the
recorded client frames, in recorded order, through a bare freshly-connected
`Transport`:

- **Requests** (frames with an `id`) are re-sent with the id rewritten
  sequentially (1, 2, 3, …). Each response is diffed against the recorded one
  using `analysis::race_detector`'s canonical JSON comparison (sorted keys,
  preserved array order) with the top-level `id` stripped from both sides.
- **Notifications** are re-sent as-is (protocol-state fidelity, e.g.
  `notifications/initialized`) but produce nothing to diff.
- Transport errors and per-request timeouts count as divergence.
- A request with no recorded response (truncated trace) is re-sent but
  excluded from scoring.
- Result: `ReplayReport { total, matched, diverged }` with
  `matched + diverged.len() == total`; the CLI exits non-zero when any frame
  diverged.

### 5. Writer: `std::fs` behind a `Mutex`, flushed per frame

`TracingTransport` records from async transport methods, but the writer uses
`std::sync::Mutex<BufWriter<std::fs::File>>` with a flush per frame. This
deliberately bends the "no blocking I/O in async paths" rule: each write is a
single line appended to a local file (a page-cache write, microseconds), the
lock is held for exactly one line, and the run's other work is unaffected.
An explicitly requested trace is fail-closed. Creation/header failures fail
the run immediately. Because the transport decorator cannot return a separate
artifact error alongside a successful server response, the writer latches its
first frame serialization/write/flush failure, warns once, and drops later
frames. After every session has shut down, `Run::execute` final-flushes the
shared writer and returns an error if that latch is set. `Report::trace_path`
is therefore populated only after the complete trace has finalized.

## Alternatives considered

- **`crates/mcp-trace` workspace crate** (the plan's wording): dependency
  cycle with `Run::execute` wiring, see Decision 1.
- **Frames as embedded JSON objects** instead of strings: loses byte fidelity
  and can't hold future raw/malformed frames.
- **Replay through a full `Session`**: regenerates handshake/ids itself — it
  would no longer replay the recording.
- **Channel + dedicated writer task** (fully async writer): avoids blocking
  I/O but can silently lose tail frames on abort, which for a debugging
  artifact is worse than a micro-stall; also adds a task + channel for no
  measurable gain at MCP frame rates.
- **Raw string equality for the diff**: false divergences on key order and
  whitespace; the race detector's canonicalizer already solves this and stays
  the single source of truth for "same response".

## Consequences

- New public surface: `mcp_loadtest::trace` (format/writer/replay),
  `Run::with_trace`, `Report::trace_path` now populated when recording, CLI
  `run --trace <file>` and `replay <trace-file>`.
- Traces from runs that respawn sessions (`cold_start`, pools) interleave all
  sessions' frames into one file; replay drives them through **one** fresh
  transport, so such traces can legitimately diverge. Fine for v1; a
  per-session field is an additive `/1` extension if needed.
- W3C `traceparent`/`tracestate`/`baggage` (the plan's 2026-07-28 note) get no
  first-class fields yet — the raw `body` already captures them whenever the
  client sends `_meta`; promoting them to columns is additive within `/1`.
- Open questions: `--no-redact` CLI exposure (decision point above);
  response-side redaction; live proxy mode and trace visualization (out of
  scope for v1 per the plan).
