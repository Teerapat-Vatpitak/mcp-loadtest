//! Regression thresholds shared between the CLI `compare` subcommand and the
//! in-process `compare_runs` tool handler.
//!
//! Centralizing here keeps the two diff implementations in sync — a future
//! refactor that moves `cmd_compare`'s pure-diff core into the lib will keep
//! these constants as the canonical source.

/// p99 latency growth, in percent, that flips a comparison into the
/// "regressed" bucket. Mirrors the rule documented in `mcp-loadtest compare`.
pub const P99_REGRESSION_PCT: f64 = 10.0;

/// Error-rate growth, in percentage points, that flips a comparison into the
/// "regressed" bucket.
pub const ERROR_RATE_REGRESSION_PP: f64 = 0.5;

/// Operator-tunable regression policy shared by the `compare` subcommand and
/// the `compare_runs` MCP tool.
///
/// [`RegressionThresholds::default`] reproduces the historical hard-coded
/// behaviour exactly (`P99_REGRESSION_PCT`, `ERROR_RATE_REGRESSION_PP`,
/// deadlock zero-tolerance), so existing callers and CI gates are unaffected
/// unless they opt in to overrides. This resolves the "expose the constants
/// as a config struct" open question in ADR 0009.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegressionThresholds {
    /// p99 latency growth, in percent, that counts as a regression.
    pub p99_pct: f64,
    /// Error-rate growth, in percentage points, that counts as a regression.
    pub error_rate_pp: f64,
    /// When `true` (the default), any increase in deadlock count is a
    /// regression. Set `false` to stop gating on deadlock upticks (the diff
    /// still reports the change, it just no longer flips `has_regression`).
    pub deadlock_zero_tolerance: bool,
}

impl Default for RegressionThresholds {
    fn default() -> Self {
        Self {
            p99_pct: P99_REGRESSION_PCT,
            error_rate_pp: ERROR_RATE_REGRESSION_PP,
            deadlock_zero_tolerance: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_the_historical_constants() {
        let t = RegressionThresholds::default();
        assert_eq!(t.p99_pct, P99_REGRESSION_PCT);
        assert_eq!(t.error_rate_pp, ERROR_RATE_REGRESSION_PP);
        assert!(t.deadlock_zero_tolerance);
    }
}
