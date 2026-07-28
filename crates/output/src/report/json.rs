//! JSON reporter — CI-friendly machine-readable output (DESIGN §17.2).
//!
//! Emits a pretty-printed JSON document of the [`Report`]. We layer a
//! human-friendly view on top of the locked `Report` struct so that
//! durations come out as integer milliseconds and timestamps as ISO 8601 —
//! both more useful to downstream tooling than the raw `Duration` /
//! `SystemTime` serde shapes.
//!
//! The on-wire JSON aligns with the schema sketched in DESIGN.md §17.2.

use std::time::Duration;

use serde::Serialize;

use crate::report::{
    ProcessStats, Report, ReportError, Reporter, ServerInfo, ThresholdViolation, format_iso8601_utc,
};
use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};

/// JSON reporter.
///
/// Stateless and zero-cost; clone freely. Output is pretty-printed JSON
/// matching DESIGN.md §17.2. See module docs for the precise schema.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonReporter;

impl Reporter for JsonReporter {
    fn render(&self, report: &Report) -> Result<String, ReportError> {
        let view = ReportView::from(report);
        Ok(serde_json::to_string_pretty(&view)?)
    }
}

// ---- view structs --------------------------------------------------------
//
// These mirror the locked `Report` field-for-field but rewrite Durations as
// integer milliseconds and SystemTime as ISO 8601. We don't touch the locked
// types — the wire format lives entirely here.

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    run_id: &'a str,
    started_at: String,
    duration_secs: f64,
    scenario: ScenarioView<'a>,
    server: ServerView<'a>,
    latency_ms: LatencyView,
    throughput: ThroughputView<'a>,
    errors: ErrorsView,
    process: ProcessView<'a>,
    deadlock_count: u32,
    hang_count: u32,
    #[serde(skip_serializing_if = "is_zero")]
    divergence_count: u64,
    #[serde(skip_serializing_if = "is_zero")]
    incomplete_worker_count: u64,
    #[serde(skip_serializing_if = "is_zero")]
    teardown_failure_count: u64,
    #[serde(skip_serializing_if = "is_zero")]
    expected_rejection_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_path: Option<String>,
    threshold_violations: Vec<ThresholdView<'a>>,
    passed: bool,
}

impl<'a> From<&'a Report> for ReportView<'a> {
    fn from(r: &'a Report) -> Self {
        Self {
            run_id: &r.run_id,
            started_at: format_iso8601_utc(r.started_at),
            duration_secs: r.duration.as_secs_f64(),
            scenario: ScenarioView {
                name: &r.scenario_name,
            },
            server: ServerView::from(&r.server_info),
            latency_ms: LatencyView::from(&r.metrics.latency),
            throughput: ThroughputView::from(&r.metrics.throughput),
            errors: ErrorsView::from((&r.metrics.outcomes, &r.metrics)),
            process: ProcessView::from(&r.process),
            deadlock_count: r.scenario_outcome.deadlock_count,
            hang_count: r.scenario_outcome.hang_count,
            divergence_count: r.scenario_outcome.divergence_count,
            incomplete_worker_count: r.scenario_outcome.incomplete_worker_count,
            teardown_failure_count: r.scenario_outcome.teardown_failure_count,
            expected_rejection_count: r.metrics.outcomes.expected_rejection,
            trace_path: r.trace_path.as_ref().map(|p| p.display().to_string()),
            threshold_violations: r
                .threshold_violations
                .iter()
                .map(ThresholdView::from)
                .collect(),
            passed: r.passed(),
        }
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Serialize)]
struct ScenarioView<'a> {
    name: &'a str,
}

#[derive(Debug, Serialize)]
struct ServerView<'a> {
    command: &'a str,
    args: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<&'a str>,
}

impl<'a> From<&'a ServerInfo> for ServerView<'a> {
    fn from(s: &'a ServerInfo) -> Self {
        Self {
            command: &s.command,
            args: &s.args,
            pid: s.pid,
            protocol_version: s.protocol_version.as_deref(),
        }
    }
}

#[derive(Debug, Serialize)]
struct LatencyView {
    p50: f64,
    p95: f64,
    p99: f64,
    p999: f64,
    min: f64,
    max: f64,
    mean: f64,
    count: u64,
}

impl From<&LatencyStats> for LatencyView {
    fn from(l: &LatencyStats) -> Self {
        Self {
            p50: ms(l.p50),
            p95: ms(l.p95),
            p99: ms(l.p99),
            p999: ms(l.p999),
            min: ms(l.min),
            max: ms(l.max),
            mean: ms(l.mean),
            count: l.count,
        }
    }
}

#[derive(Debug, Serialize)]
struct ThroughputView<'a> {
    total_requests: u64,
    successful_requests: u64,
    requests_per_sec: f64,
    #[serde(skip)]
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> From<&'a ThroughputStats> for ThroughputView<'a> {
    fn from(t: &'a ThroughputStats) -> Self {
        Self {
            total_requests: t.total_requests,
            successful_requests: t.successful_requests,
            requests_per_sec: t.requests_per_sec,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorsView {
    total: u64,
    by_category: ErrorBreakdown,
}

#[derive(Debug, Serialize)]
struct ErrorBreakdown {
    #[serde(rename = "Hang")]
    hang: u64,
    #[serde(rename = "Timeout")]
    timeout: u64,
    #[serde(rename = "ServerError")]
    server_error: u64,
    #[serde(rename = "ProtocolError")]
    protocol_error: u64,
    #[serde(rename = "Crash")]
    crash: u64,
    #[serde(rename = "Malformed")]
    malformed: u64,
    #[serde(rename = "Disconnected")]
    disconnected: u64,
    #[serde(rename = "Cancelled")]
    cancelled: u64,
}

impl From<(&OutcomeCounts, &ScenarioMetrics)> for ErrorsView {
    fn from((o, m): (&OutcomeCounts, &ScenarioMetrics)) -> Self {
        let total = m
            .throughput
            .total_requests
            .saturating_sub(m.throughput.successful_requests);
        Self {
            total,
            by_category: ErrorBreakdown {
                hang: o.hang,
                timeout: o.timeout,
                server_error: o.server_error,
                protocol_error: o.protocol_error,
                crash: o.crash,
                malformed: o.malformed,
                disconnected: o.disconnected,
                cancelled: o.cancelled,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ProcessView<'a> {
    peak_rss_mb: f64,
    final_rss_mb: f64,
    avg_cpu_pct: f64,
    #[serde(skip)]
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> From<&'a ProcessStats> for ProcessView<'a> {
    fn from(p: &'a ProcessStats) -> Self {
        Self {
            peak_rss_mb: p.peak_rss_mb,
            final_rss_mb: p.final_rss_mb,
            avg_cpu_pct: p.avg_cpu_pct,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, Serialize)]
struct ThresholdView<'a> {
    metric: &'a str,
    expected: &'a str,
    actual: &'a str,
}

impl<'a> From<&'a ThresholdViolation> for ThresholdView<'a> {
    fn from(v: &'a ThresholdViolation) -> Self {
        Self {
            // Wire format keeps the legacy snake_case string key; ThresholdKind's
            // `name()` was deliberately chosen to match what the previous
            // free-form `metric: String` field stored.
            metric: v.kind.name(),
            expected: &v.expected,
            actual: &v.actual,
        }
    }
}

/// Convert a `Duration` to fractional milliseconds (matches DESIGN.md §17.2 schema).
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms_zero_and_known() {
        assert_eq!(ms(Duration::ZERO), 0.0);
        assert!((ms(Duration::from_micros(12_300)) - 12.3).abs() < 1e-9);
        assert!((ms(Duration::from_secs(1)) - 1_000.0).abs() < 1e-9);
    }
}
