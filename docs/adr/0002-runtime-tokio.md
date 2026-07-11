# 2. Async runtime: tokio

Date: 2026-05-10
Status: Accepted

## Context

Given Rust (ADR 0001), we need an async runtime. The driver runs N concurrent worker tasks, each maintaining a stdio session with a child process. We need:
- Subprocess management (`tokio::process`)
- Stdio framing (`tokio::io::AsyncBufReadExt` lines)
- Timers / timeouts (`tokio::time::timeout`)
- Cancellation tokens (for graceful shutdown)
- Worker scaling 1k+ tasks

## Decision

`tokio` with the `full` feature flag.

## Alternatives considered

| Runtime | Why rejected |
|---|---|
| **async-std** | Smaller ecosystem; `tokio::process` has battle-tested cross-platform behavior we'd otherwise reimplement. |
| **smol** | Lightweight but lacks high-level subprocess utilities; for a load tester driving many child processes, the convenience of tokio outweighs the size cost. |
| **pollster** (single-threaded) | Inadequate for 1k+ concurrent workers. |

## Consequences

**Positive:**
- Stable, well-documented, broadly compatible with the rest of the Rust ecosystem.
- `tokio_util::sync::CancellationToken` makes graceful shutdown ergonomic.
- Multi-threaded scheduler scales worker count without manual thread pool management.

**Negative:**
- Larger compile time + binary size than smol/async-std. Acceptable for a development tool.
- Locks the project into tokio's executor — switching later is non-trivial.

**Open:**
- Whether to expose `tokio::Handle` in the public API. Decided: **no** — keep tokio an implementation detail; users construct `Run` and call `.execute().await` from their own runtime. Re-evaluate if users ask.
