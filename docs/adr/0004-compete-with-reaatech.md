# 4. Strategy: head-on competition with reaatech/mcp-load-test

Date: 2026-05-10
Status: Accepted

## Context

After publishing the initial commit + scaffolding to `Teerapat-Vatpitak/mcp-loadtest` on GitHub, a competitor search turned up [`reaatech/mcp-load-test`](https://github.com/reaatech/mcp-load-test) — a TypeScript monorepo that:

- Was last updated 2026-05-10 14:31 UTC, ~30 minutes before our initial push.
- Has 0 stars but ships a polished release pipeline (npm packages, biome/turbo/changesets, GitHub Actions CI).
- Advertises features that overlap directly with our intended first-release scope: 3 transports (HTTP/SSE/stdio), latency histograms, breaking point detection, perf grading A-F, soak, compare-baselines, realistic patterns, programmatic API.
- File sampling (138 total files, 77 TS) suggests they have ~50% of their README's claims actually implemented.

This is the only direct competitor; other repos found (`eval-sys/mcpmark`, `chrishayuk/mcp-performance-diagnostics`, `QuantGeekDev/mcp-performance-test`, etc.) are either different products entirely (model evaluation, not server stress-testing) or 0-2 star experiments.

## Decision

**Path A: head-on competition.** Build mcp-loadtest to match every reaatech feature *and* surface differentiators they don't have. Take the GitHub repo private until we ship the first release with feature parity + differentiators in place.

## Alternatives considered

| Path | Why rejected |
|---|---|
| **B — contribute to reaatech** | Author wanted ownership of the brand and Rust-perf positioning, not to ride someone else's TypeScript codebase. Their architecture is well-formed but locks us out of the Rust differentiator. |
| **C — reposition as bug detector only** | Throws away the work already done on load-testing scaffolding. Also narrows the addressable audience too much; load testing is the obvious use case people search for, bug detection is the upsell. |

## Consequences

**Positive:**
- Owns the Rust + perf-binary niche (no Node runtime needed).
- Owns the bug-detection differentiators (deadlock_probe, race_detector, fuzzer, coverage) — these are the strongest moats; reaatech's architecture doesn't naturally extend to them.
- Author's track record (HKUDS/Vibe-Trading PR #85 deadlock fix) gives credibility for the "we know the bug class because we found it in production" framing.

**Negative:**
- Original 3-week plan extends to 8 weeks (M1-M7) before re-publish. Significant time before public feedback loop reopens.
- We will reimplement features reaatech already has (HTTP/SSE transport, breaking-point, grading, etc.) — duplicate work.
- Competitor may add deadlock detection in those 8 weeks, eroding our differentiator.

**Mitigations:**
- Repo stays private only until M7. If competitor pulls ahead on deadlock detection mid-way, re-evaluate at M5 checkpoint.
- M1-M3 still produce a working internal release (against real Vibe-Trading bug) — proves the core thesis early, even before parity work.

## Open questions

- Whether to publicly share progress on social (Twitter/Mastodon) during private build, to start audience-building before re-publish. Default: silent until the first release.
- Whether to ship the first release with all M7 differentiators, or release earlier with parity-only and add differentiators in a follow-up. Default: full set at the first release (one big launch beats trickle).
