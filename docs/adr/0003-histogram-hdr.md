# 3. Latency histogram: hdrhistogram

Date: 2026-05-10
Status: Accepted

## Context

We need to record per-request latency from possibly-millions of requests across worker threads, and compute accurate p50/p95/p99/p999 at the end. Naive `Vec<Duration>` + sort works but uses O(N) memory and locks badly under concurrent writes.

## Decision

[`hdrhistogram`](https://crates.io/crates/hdrhistogram) — High Dynamic Range histograms with bounded memory and tunable precision.

Per-worker histograms (no shared state on the hot path), merged at scenario end.

## Alternatives considered

| Approach | Why rejected |
|---|---|
| **Vec<Duration> + sort** | O(N) memory; doesn't scale past ~1M samples. Locking under contention hurts perf. |
| **t-digest** | Better for streaming quantiles in distributed systems, but more complex API and less precise for our latency range. |
| **Custom fixed-bucket histogram** | Reinventing hdrhistogram, badly. |

## Consequences

**Positive:**
- Bounded memory regardless of N (default config: ~64 KB per histogram).
- Quantile queries are O(log buckets), effectively constant.
- Lock-free per-worker recording; merge happens once at end.

**Negative:**
- hdrhistogram API exposes `u64` (not `Duration`) — wrap in our own `LatencyHistogram` newtype.
- Records integers; we'll record microseconds (sufficient resolution for our range).

**Open:**
- Whether to expose the underlying `hdrhistogram::Histogram` in the public API. Decided: **yes** in `LatencyStats` so power users can do custom percentile queries.
