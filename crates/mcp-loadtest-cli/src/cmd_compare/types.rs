//! Data structures shared across the `compare` subcommand.
//!
//! Contains the on-disk report shape (subset of the JSON reporter's output)
//! and the diff types that feed both the markdown renderer and the JSON
//! output mode.

use serde::{Deserialize, Serialize};

// Re-export the canonical regression thresholds from the core library so the
// `serve` tool handler and `cmd_compare` agree by construction. The `pub`
// surface stays stable for downstream callers that imported these symbols.
pub use mcp_loadtest::analysis::regression::{
    ERROR_RATE_REGRESSION_PP, P99_REGRESSION_PCT, RegressionThresholds,
};

/// Symbol used in markdown output for a regressed metric.
pub(super) const ARROW_REGRESSION: &str = "🔻";
/// Symbol used in markdown output for an improved metric.
pub(super) const ARROW_IMPROVEMENT: &str = "🔼";

// ---- on-disk report shape (subset) --------------------------------------

/// Minimal deserialization view of `metrics.json`. Mirrors the fields the
/// JSON reporter writes (DESIGN.md §17.2). We only model what the
/// comparison touches; anything else in the file is silently ignored.
#[derive(Debug, Deserialize)]
pub struct ComparableReport {
    /// ULID of the run.
    pub run_id: String,
    /// Wall-clock start time as ISO 8601.
    #[serde(default)]
    pub started_at: String,
    /// Total run duration, seconds.
    #[serde(default)]
    pub duration_secs: f64,
    /// Scenario block — we only need the name.
    pub scenario: ScenarioView,
    /// Latency percentiles in milliseconds.
    pub latency_ms: ComparableLatency,
    /// Throughput aggregates.
    pub throughput: ComparableThroughput,
    /// Error breakdown.
    pub errors: ComparableErrors,
    /// Deadlock count from the scenario outcome.
    #[serde(default)]
    pub deadlock_count: u32,
    /// Hang count from the scenario outcome.
    #[serde(default)]
    pub hang_count: u32,
    /// Whether the run met all its thresholds.
    #[serde(default)]
    pub passed: bool,
}

/// Mirror of the `scenario` object in the wire format.
#[derive(Debug, Deserialize)]
pub struct ScenarioView {
    /// Scenario `name` from `Scenario::name()`.
    pub name: String,
}

/// Mirror of the `latency_ms` block (durations in ms).
#[derive(Debug, Deserialize, Clone)]
pub struct ComparableLatency {
    /// 50th percentile latency, ms.
    #[serde(default)]
    pub p50: f64,
    /// 95th percentile latency, ms.
    #[serde(default)]
    pub p95: f64,
    /// 99th percentile latency, ms — primary regression gate.
    pub p99: f64,
    /// Total samples in the histogram.
    pub count: u64,
}

/// Mirror of the `throughput` block.
#[derive(Debug, Deserialize, Clone)]
pub struct ComparableThroughput {
    /// Total tool calls attempted (success + errors).
    pub total_requests: u64,
    /// Successful tool calls.
    pub successful_requests: u64,
    /// Mean requests-per-second.
    pub requests_per_sec: f64,
}

/// Mirror of the `errors` block.
#[derive(Debug, Deserialize, Clone)]
pub struct ComparableErrors {
    /// Total error count across all categories.
    pub total: u64,
}

// ---- diff types (also the `--format json` output shape) -----------------

/// Direction of a metric change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Metric got worse (regression).
    Regressed,
    /// Metric got better.
    Improved,
    /// Metric did not change in a meaningful way.
    Neutral,
}

/// One diffed metric.
#[derive(Debug, Clone, Serialize)]
pub struct MetricDiff {
    /// Human-readable metric label, e.g. `"latency_p99_ms"`.
    pub metric: String,
    /// Baseline value as a string (so we can format ms / counts uniformly).
    pub baseline: String,
    /// Current value as a string.
    pub current: String,
    /// Signed numeric change (current - baseline) for downstream tooling.
    pub change: f64,
    /// Direction (regressed / improved / neutral).
    pub direction: Direction,
}

/// Aggregate output of `compare`.
#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    /// Run id of the baseline.
    pub baseline_run_id: String,
    /// Run id of the current run.
    pub current_run_id: String,
    /// Scenario name (taken from current; baseline shown if it differs).
    pub scenario: String,
    /// All diffed metrics, in display order.
    pub metrics: Vec<MetricDiff>,
    /// Subset of `metrics` flagged as regressions.
    pub regressions: Vec<MetricDiff>,
    /// True if any regression was detected.
    pub has_regression: bool,
}
