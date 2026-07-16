//! `compare` subcommand — diff two `metrics.json` reports for regressions.
//!
//! Reads two metric reports written by [`mcp_loadtest::report::json::JsonReporter`]
//! (see DESIGN.md §17.2 for the schema) and produces a regression-focused
//! diff, either as Markdown (default, human-readable) or as JSON (CI-friendly).
//!
//! ## Regression rules
//!
//! A metric is flagged as a regression when **any** of these hold:
//! - latency p99 grew by more than `thresholds.p99_pct` percent;
//! - error rate (errors / total) grew by more than
//!   `thresholds.error_rate_pp` percentage points;
//! - deadlock count went up at all, when `deadlock_zero_tolerance` is set.
//!
//! [`RegressionThresholds::default`] reproduces the historical policy
//! (10% p99 / 0.5pp error rate / deadlock zero-tolerance); the `compare`
//! CLI flags and the `compare_runs` MCP tool args override it (ADR 0009).
//! Improvements (the same deltas in the other direction) get the up arrow;
//! everything else is informational.
//!
//! ## Why we re-deserialize the JSON shape locally
//!
//! The on-disk JSON is the wire format produced by the JSON reporter — it's
//! not the locked `Report` struct (durations come out as ms, timestamps as
//! ISO 8601, etc.). Rather than expose a "ReportView" type from the library
//! just for the consumer side, we keep a small `ComparableReport` here that
//! mirrors only the fields we actually diff. Adding a comparison dimension
//! means adding a field here, not changing the library API.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

mod diff;
mod render;
mod types;

pub use diff::build_report;
pub use types::{
    ComparableErrors, ComparableLatency, ComparableReport, ComparableThroughput, CompareReport,
    Direction, ERROR_RATE_REGRESSION_PP, MetricDiff, P99_REGRESSION_PCT, RegressionThresholds,
    ScenarioView,
};

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareFormat {
    /// Human-readable Markdown (default).
    Markdown,
    /// Machine-readable JSON (for CI gates).
    Json,
}

impl CompareFormat {
    /// Parse the `--format` flag.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "markdown" | "md" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            other => Err(anyhow::anyhow!(
                "unknown --format `{other}` (expected markdown|json)"
            )),
        }
    }
}

// ---- entry point --------------------------------------------------------

/// What the dispatch arm needs to print and exit on.
#[derive(Debug)]
pub struct CompareOutcome {
    /// Diff rendered in the requested format.
    pub rendered: String,
    /// The underlying diff — `has_regression` drives the exit code.
    pub report: CompareReport,
}

/// Run the `compare` subcommand. Reads two JSON files, builds a diff using
/// `thresholds` (pass [`RegressionThresholds::default`] for the historical
/// policy), and renders it in the requested format. The caller prints
/// `rendered` and then applies [`gate`] so regressions exit non-zero.
pub fn run(
    baseline: &Path,
    current: &Path,
    format: CompareFormat,
    thresholds: &RegressionThresholds,
) -> Result<CompareOutcome> {
    let base = read_report(baseline)?;
    let cur = read_report(current)?;
    let report = build_report(&base, &cur, thresholds);
    let rendered = match format {
        CompareFormat::Markdown => render::render_markdown(&report, &base, &cur),
        CompareFormat::Json => {
            serde_json::to_string_pretty(&report).context("serializing compare report to json")?
        }
    };
    Ok(CompareOutcome { rendered, report })
}

/// CI gate (DESIGN.md §15.4): error — and therefore exit non-zero — when any
/// regression flag fired. Called after the diff has been printed, mirroring
/// how `run` bails on threshold violations.
pub fn gate(report: &CompareReport) -> Result<()> {
    if report.has_regression {
        let metrics: Vec<&str> = report
            .regressions
            .iter()
            .map(|m| m.metric.as_str())
            .collect();
        anyhow::bail!(
            "{} regression flag(s) fired: {} — see diff above",
            report.regressions.len(),
            metrics.join(", ")
        );
    }
    Ok(())
}

fn read_report(path: &Path) -> Result<ComparableReport> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading metrics.json at {}", path.display()))?;
    let report: ComparableReport = serde_json::from_str(&raw)
        .with_context(|| format!("parsing metrics.json at {}", path.display()))?;
    Ok(report)
}

// ---- CLI struct ---------------------------------------------------------

/// Parsed CLI args for the subcommand.
#[derive(Debug)]
pub struct CompareArgs {
    /// Path to the baseline metrics.json.
    pub baseline: PathBuf,
    /// Path to the current metrics.json.
    pub current: PathBuf,
    /// Output format.
    pub format: CompareFormat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd_compare::diff::{classify_error_rate, classify_p99};
    use crate::cmd_compare::render::render_markdown;
    use crate::cmd_compare::types::ARROW_REGRESSION;

    fn baseline_report() -> ComparableReport {
        ComparableReport {
            run_id: "01BASE".into(),
            started_at: "2026-05-10T07:30:00Z".into(),
            duration_secs: 60.0,
            scenario: ScenarioView {
                name: "sustained".into(),
            },
            latency_ms: ComparableLatency {
                p50: 10.0,
                p95: 50.0,
                p99: 100.0,
                count: 1000,
            },
            throughput: ComparableThroughput {
                total_requests: 1000,
                successful_requests: 1000,
                requests_per_sec: 16.7,
            },
            errors: ComparableErrors { total: 0 },
            deadlock_count: 0,
            hang_count: 0,
            passed: true,
        }
    }

    #[test]
    fn classify_p99_flags_regression_above_threshold() {
        // 100ms → 115ms = 15% growth, above the 10% default threshold.
        assert_eq!(
            classify_p99(100.0, 115.0, P99_REGRESSION_PCT),
            Direction::Regressed
        );
    }

    #[test]
    fn classify_p99_neutral_below_threshold() {
        // 100ms → 105ms = 5% growth, below threshold.
        assert_eq!(
            classify_p99(100.0, 105.0, P99_REGRESSION_PCT),
            Direction::Neutral
        );
    }

    #[test]
    fn classify_p99_flags_improvement() {
        // 100ms → 80ms = 20% drop.
        assert_eq!(
            classify_p99(100.0, 80.0, P99_REGRESSION_PCT),
            Direction::Improved
        );
    }

    #[test]
    fn classify_error_rate_pp_threshold() {
        // 0.1pp jump — neutral.
        assert_eq!(
            classify_error_rate(0.0, 0.1, ERROR_RATE_REGRESSION_PP),
            Direction::Neutral
        );
        // 1.0pp jump — regression.
        assert_eq!(
            classify_error_rate(0.0, 1.0, ERROR_RATE_REGRESSION_PP),
            Direction::Regressed
        );
    }

    #[test]
    fn build_report_flags_p99_regression() {
        let base = baseline_report();
        let mut cur = baseline_report();
        cur.run_id = "01CUR".into();
        cur.latency_ms.p99 = 200.0; // 100% growth — regression.

        let cmp = build_report(&base, &cur, &RegressionThresholds::default());
        assert!(cmp.has_regression);
        assert!(
            cmp.regressions.iter().any(|m| m.metric == "latency_p99_ms"),
            "expected latency_p99_ms in regressions: {:?}",
            cmp.regressions
        );
    }

    #[test]
    fn build_report_flags_deadlock_uptick() {
        let base = baseline_report();
        let mut cur = baseline_report();
        cur.deadlock_count = 1;

        let cmp = build_report(&base, &cur, &RegressionThresholds::default());
        assert!(cmp.has_regression);
        assert!(cmp.regressions.iter().any(|m| m.metric == "deadlock_count"));
    }

    #[test]
    fn build_report_flags_error_rate_jump() {
        let base = baseline_report();
        let mut cur = baseline_report();
        cur.errors.total = 50; // 5% errors over 1000 requests = +5pp regression.
        cur.throughput.successful_requests = 950;

        let cmp = build_report(&base, &cur, &RegressionThresholds::default());
        assert!(cmp.has_regression);
        assert!(cmp.regressions.iter().any(|m| m.metric == "error_rate_pct"));
    }

    #[test]
    fn build_report_no_regression_when_clean() {
        let base = baseline_report();
        let cur = baseline_report();
        let cmp = build_report(&base, &cur, &RegressionThresholds::default());
        assert!(!cmp.has_regression);
        assert!(cmp.regressions.is_empty());
    }

    #[test]
    fn custom_thresholds_change_the_pass_fail_bucket() {
        // Same inputs, different policy → different verdict (B1 contract).
        let base = baseline_report();

        // (a) p99 +15%: regression under the default 10%, clean under 20%.
        let mut cur = baseline_report();
        cur.latency_ms.p99 = 115.0;
        assert!(build_report(&base, &cur, &RegressionThresholds::default()).has_regression);
        let lax_p99 = RegressionThresholds {
            p99_pct: 20.0,
            ..RegressionThresholds::default()
        };
        assert!(!build_report(&base, &cur, &lax_p99).has_regression);

        // (b) deadlock uptick: regression by default, ignored when
        //     zero-tolerance is turned off.
        let mut dl = baseline_report();
        dl.deadlock_count = 3;
        assert!(build_report(&base, &dl, &RegressionThresholds::default()).has_regression);
        let allow_dl = RegressionThresholds {
            deadlock_zero_tolerance: false,
            ..RegressionThresholds::default()
        };
        assert!(!build_report(&base, &dl, &allow_dl).has_regression);
    }

    #[test]
    fn render_markdown_mentions_regression() {
        let base = baseline_report();
        let mut cur = baseline_report();
        cur.run_id = "01CUR".into();
        cur.latency_ms.p99 = 250.0;
        let cmp = build_report(&base, &cur, &RegressionThresholds::default());
        let md = render_markdown(&cmp, &base, &cur);
        assert!(md.contains("REGRESSION"));
        assert!(md.contains("latency_p99_ms"));
        assert!(md.contains(ARROW_REGRESSION));
    }

    #[test]
    fn compare_format_parses() {
        assert_eq!(
            CompareFormat::parse("markdown").unwrap(),
            CompareFormat::Markdown
        );
        assert_eq!(CompareFormat::parse("md").unwrap(), CompareFormat::Markdown);
        assert_eq!(CompareFormat::parse("json").unwrap(), CompareFormat::Json);
        assert!(CompareFormat::parse("yaml").is_err());
    }
}
