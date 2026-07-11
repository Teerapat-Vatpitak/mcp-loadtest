# 9. Regression threshold defaults: 10% p99 / 0.5pp error rate

Date: 2026-05-11
Status: Accepted

## Context

The `compare` subcommand (CLI) and the in-process `compare_runs` MCP tool (serve mode) both decide whether a run regressed against a baseline. The rule, applied identically in both:

- **p99 latency** grew by more than **10%**.
- **Error rate** grew by more than **0.5 percentage points**.
- **Deadlock count** increased by any amount.

Before commit `1ba3805`, these numbers were duplicated as local constants in `cmd_compare.rs` and `serve/tools.rs`. Two consumers, two copies, no enforcement — a future tweak to the CLI would silently drift from the MCP tool, and a user comparing CLI output against an agent's `compare_runs` call would see disagreement on borderline runs.

The thresholds themselves are pragmatic defaults derived from typical CI regression budgets:

- **10% p99**: strict enough to catch real perf regressions, loose enough to absorb run-to-run jitter at the 10-1000 sample sizes we expect in CI. Tighter (5%) flags too much noise; looser (20%) misses meaningful regressions.
- **0.5pp error rate**: meaningful change for any reliability-critical tool, but doesn't false-positive on a single transient failure in a 200-call run (0.5% of 200 = 1 call).
- **Deadlock count increase**: zero tolerance. A deadlock is always a bug.

## Decision

Centralize the thresholds in a single module:

```rust
// crates/mcp-loadtest/src/analysis/regression.rs
pub const P99_REGRESSION_PCT: f64 = 10.0;
pub const ERROR_RATE_REGRESSION_PP: f64 = 0.5;
```

Both `cmd_compare.rs` (CLI) and `serve/tools.rs` (MCP) import from `analysis::regression`. The deadlock-count rule lives next to them so the three rules are visibly grouped.

## Alternatives considered

| Option                                                            | Why rejected                                                                                                                                          |
| ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Operator-configurable thresholds via CLI flag / MCP tool arg**  | Useful but premature — we don't yet know which knobs operators actually want to turn. Adding them later in `analysis::regression` is straightforward. |
| **Per-tool overrides (e.g. tighter threshold for stdio vs HTTP)** | Same reasoning — defer until empirical evidence shows the defaults are wrong for a category.                                                          |
| **Tighter defaults (5% / 0.2pp)**                                 | False-positive rate too high at the sample sizes typical for `mcp-loadtest` runs.                                                                     |

## Consequences

**Positive:**

- CLI and MCP consumers stay in sync by construction. Cargo's module system enforces it.
- Future tuning lands in one place; reviewers know exactly which constants control the policy.
- The numbers are pragmatic defaults, not gospel — documented here so they can be challenged later with evidence rather than re-derived from scratch.

**Negative:**

- A user who wants different thresholds today has to fork or pre-process the metrics themselves. Workaround until configurable thresholds land.

**Open:**

- ~~Whether to expose the constants as a `RegressionConfig` struct that `compare` and `compare_runs` both accept.~~ **Resolved 2026-05-16 — see Update below.**
- Whether deadlock-count rule belongs in the same module or in a dedicated `analysis::deadlock`. Defer; current single-module home is fine at this scale.

## Update (2026-05-16): thresholds are now operator-configurable

The "expose as a config struct" open question is resolved. `analysis::regression` now also exports:

```rust
pub struct RegressionThresholds {
    pub p99_pct: f64,                 // default P99_REGRESSION_PCT (10.0)
    pub error_rate_pp: f64,           // default ERROR_RATE_REGRESSION_PP (0.5)
    pub deadlock_zero_tolerance: bool, // default true
}
```

`RegressionThresholds::default()` reproduces the historical hard-coded policy **exactly**, so existing CI gates are unaffected unless they opt in. The two constants stay as the documented defaults (and the `Default` impl's single source of truth).

Overrides are surfaced where operators actually work:

- **CLI:** `mcp-loadtest compare --max-p99-regression-pct <pct> --max-error-rate-regression-pp <pp> [--allow-deadlock-increase]`.
- **MCP tool:** optional `compare_runs` args `max_p99_regression_pct` / `max_error_rate_regression_pp` / `allow_deadlock_increase`.

Status stays **Accepted** — the defaults and rationale above are unchanged; this only adds an opt-in override path that the original "Alternatives considered" row deferred until a user asked for it. A user asked.
