# 1. Language: Rust

Date: 2026-05-10
Status: Accepted

## Context

`mcp-loadtest` stress-tests MCP servers. The driver must:
- Handle 1000+ concurrent stdio sessions without degrading server perf measurements.
- Have very low per-request overhead (target < 50µs) so the tool itself doesn't dominate measurements.
- Be cross-platform — author runs Windows; users will be on Linux/macOS too.
- Distribute as a single static binary for `cargo install` simplicity.
- Support precise async cancellation (for hang/deadlock detection scenarios).

## Decision

Rust + tokio.

## Alternatives considered

| Lang | Why rejected |
|---|---|
| **Go** | Simpler, but lacks the precise control over async cancellation we want for hang detection. Goroutine leak prevention is also less ergonomic than tokio's `JoinHandle` pattern. |
| **Python + asyncio** | Slower than Rust by ~10× on stdio framing throughput. GIL caps real concurrency under load. Would also force users to install Python — friction for the "one binary" goal. |
| **Node.js** | Weaker async story for subprocess management; Node's stdio handling has known edge cases on Windows. Single-threaded event loop is also a limit. |
| **C++** | Concurrency primitives less ergonomic; build/distribution story worse than Rust for OSS. |

## Consequences

**Positive:**
- Zero-cost async, predictable cancellation, ergonomic error handling via `Result`.
- Single static binary distribution via `cargo install`.
- Strong ecosystem for the deps we'll need: `tokio`, `serde`, `hyper` (M5), `hdrhistogram`.

**Negative:**
- Rust learning curve for contributors. Mitigated by clear, documented project conventions (CONTRIBUTING.md).
- PyO3 binding for Python users deferred to a later release (Python users can use the CLI binary in the meantime).

**Open:**
- Whether to commit to MSRV stable-2 or stable-latest. Decided: stable-2 (currently 1.85). See `rust-toolchain.toml`.
