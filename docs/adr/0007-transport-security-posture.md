# 7. Transport security posture: redirects, frame caps, path validation

Date: 2026-05-11
Status: Accepted

## Context

The pre-publish security review (commit `bae92c2`) identified three high-severity surfaces on the transport / serve layer:

- **SSRF via redirect-following.** HTTP, SSE, and WS transports inherited `reqwest`'s default `Policy::limited(10)`. An operator pointing the tool at a server that 30x-redirected to `http://169.254.169.254/...` would silently hit the cloud metadata endpoint and exfiltrate IAM creds into the load test's request stream.
- **OOM via unbounded JSON-RPC frame.** `serve` mode read JSON-RPC frames over stdin with `read_line` — no byte cap. A single multi-GB line would consume all RAM before the parser ever saw a complete frame.
- **Read-anything via `compare_runs`.** The MCP tool accepted any caller-supplied path. An agent that asked `compare_runs(baseline="~/.ssh/id_rsa", ...)` would happily ingest the file as JSON, fail to parse, and surface a chunk of the contents in the error message.

A fourth, lower-severity issue: per-transport pending-notification VecDeques were unbounded. A misbehaving server flooding notifications could grow them without limit.

## Decision

- **Redirect policy: none.** All transports set `Policy::none()`. Operators pointing at a host that legitimately redirects must resolve the final endpoint themselves.
- **Frame cap: 16 MB.** `serve` mode reads frames via `read_bounded_line` with `MAX_LINE_BYTES = 16 * 1024 * 1024`. Frames over the cap are rejected with a JSON-RPC error, not silently truncated.
- **Path validation on `compare_runs`.** `validate_metrics_path` rejects `..`, canonicalizes the input, and requires the result to be either (a) under `<cwd>/runs/` or (b) an absolute path ending in `metrics.json`. The second branch exists so an operator can hand-edit a path on the CLI without being forced into the `runs/` jail.
- **Pending notification cap: 256.** Per-transport `VecDeque` is capped; overflow drops the oldest notification with a warning log.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Host allowlist instead of `Policy::none()`** | More flexible but needs operator-facing config; deferred to a follow-up (see CHANGELOG `[Unreleased]` Notes). `Policy::none()` is the safe default in the meantime. |
| **No frame cap, rely on OS / cgroup limits** | Defense-in-depth dictates the application enforce its own bound; a 16 MB cap is generous for any sane MCP message. |
| **`compare_runs` jail strictly under `runs/`** | Too strict — operators legitimately compare runs stored elsewhere. The `metrics.json` filename suffix is a reasonable compromise. |

## Consequences

**Positive:**
- Closes three high-severity surfaces before the first release ships.
- Frame cap and redirect policy are documented in transport rustdoc so users hit the limits as a visible error rather than a mysterious failure.

**Negative:**
- Operators pointing at a redirecting host must resolve the final endpoint themselves. Documented in transport rustdoc.
- The 16 MB frame cap is project-wide. Configurable per-call cap is deferred until someone has a legitimate need.

**Open:**
- Host allowlist for SSRF defense (follow-up, operator-facing config). Recorded so the deferral isn't forgotten.
- Whether `compare_runs` should also restrict by file size in addition to path. Defer until an abuse case shows up.
