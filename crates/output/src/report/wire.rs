//! Owned representation of the versioned `metrics.json` wire contract.
//!
//! The runtime [`Report`] deliberately uses Rust-native [`Duration`] and
//! [`std::time::SystemTime`] values. `MetricsDocumentV1` is the stable, owned boundary
//! used by JSON output and post-run consumers: timestamps are ISO 8601,
//! durations are fractional milliseconds or seconds, and optional additive
//! fields retain the v1 omission rules.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::report::{
    ProcessStats, Report, ReportError, ServerInfo, ThresholdViolation, format_iso8601_utc,
};
use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};

/// Owned `metrics.json` document for the v1 wire format.
///
/// This shape intentionally matches `docs/schema/metrics.v1.json`. New
/// optional fields may be added without changing the version, but removing or
/// renaming an existing field requires a new wire version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsDocumentV1 {
    /// ULID identifying the run.
    pub run_id: String,
    /// Run start in ISO 8601 UTC.
    pub started_at: String,
    /// Full run lifecycle duration in seconds.
    pub duration_secs: f64,
    /// Scenario identity.
    pub scenario: ScenarioDocumentV1,
    /// Server identity and negotiated protocol revision.
    pub server: ServerDocumentV1,
    /// Aggregate latency statistics in fractional milliseconds.
    pub latency_ms: LatencyDocumentV1,
    /// Aggregate throughput statistics.
    pub throughput: ThroughputDocumentV1,
    /// Aggregate error count and outcome breakdown.
    pub errors: ErrorsDocumentV1,
    /// Process resource summary.
    pub process: ProcessDocumentV1,
    /// Scenario-level deadlock count.
    pub deadlock_count: u32,
    /// Scenario-level hang count.
    pub hang_count: u32,
    /// Response-divergence count. Omitted when zero for v1 compatibility.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub divergence_count: u64,
    /// Incomplete pooled-worker count. Omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub incomplete_worker_count: u64,
    /// Failed or timed-out teardown count. Omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub teardown_failure_count: u64,
    /// Expected protocol-fuzzer rejection count. Omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub expected_rejection_count: u64,
    /// Trace artifact path, when tracing was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_path: Option<String>,
    /// Configured threshold violations.
    #[serde(default)]
    pub threshold_violations: Vec<ThresholdDocumentV1>,
    /// Overall correctness and threshold verdict.
    #[serde(default)]
    pub passed: bool,
}

impl MetricsDocumentV1 {
    /// Parse a v1 `metrics.json` document.
    pub fn from_json_str(input: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(input)
    }

    /// Serialize this document as deterministic, pretty-printed JSON.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

impl From<&Report> for MetricsDocumentV1 {
    fn from(report: &Report) -> Self {
        Self {
            run_id: report.run_id.clone(),
            started_at: format_iso8601_utc(report.started_at),
            duration_secs: report.duration.as_secs_f64(),
            scenario: ScenarioDocumentV1 {
                name: report.scenario_name.clone(),
            },
            server: ServerDocumentV1::from(&report.server_info),
            latency_ms: LatencyDocumentV1::from(&report.metrics.latency),
            throughput: ThroughputDocumentV1::from(&report.metrics.throughput),
            errors: ErrorsDocumentV1::from((&report.metrics.outcomes, &report.metrics)),
            process: ProcessDocumentV1::from(&report.process),
            deadlock_count: report.scenario_outcome.deadlock_count,
            hang_count: report.scenario_outcome.hang_count,
            divergence_count: report.scenario_outcome.divergence_count,
            incomplete_worker_count: report.scenario_outcome.incomplete_worker_count,
            teardown_failure_count: report.scenario_outcome.teardown_failure_count,
            expected_rejection_count: report.metrics.outcomes.expected_rejection,
            trace_path: report
                .trace_path
                .as_ref()
                .map(|path| path.display().to_string()),
            threshold_violations: report
                .threshold_violations
                .iter()
                .map(ThresholdDocumentV1::from)
                .collect(),
            passed: report.passed(),
        }
    }
}

/// Scenario block in [`MetricsDocumentV1`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioDocumentV1 {
    /// Stable scenario name.
    pub name: String,
}

/// Server block in [`MetricsDocumentV1`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDocumentV1 {
    /// Command name or sanitized remote endpoint identity.
    pub command: String,
    /// Command arguments for stdio runs.
    pub args: Vec<String>,
    /// Historical server PID, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Negotiated MCP protocol revision, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
}

impl From<&ServerInfo> for ServerDocumentV1 {
    fn from(server: &ServerInfo) -> Self {
        Self {
            command: server.command.clone(),
            args: server.args.clone(),
            pid: server.pid,
            protocol_version: server.protocol_version.clone(),
        }
    }
}

/// Latency block in [`MetricsDocumentV1`], expressed in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyDocumentV1 {
    /// Median latency in milliseconds.
    pub p50: f64,
    /// 95th-percentile latency in milliseconds.
    pub p95: f64,
    /// 99th-percentile latency in milliseconds.
    pub p99: f64,
    /// 99.9th-percentile latency in milliseconds.
    pub p999: f64,
    /// Minimum latency in milliseconds.
    pub min: f64,
    /// Maximum latency in milliseconds.
    pub max: f64,
    /// Arithmetic mean latency in milliseconds.
    pub mean: f64,
    /// Number of latency samples.
    pub count: u64,
}

impl From<&LatencyStats> for LatencyDocumentV1 {
    fn from(latency: &LatencyStats) -> Self {
        Self {
            p50: milliseconds(latency.p50),
            p95: milliseconds(latency.p95),
            p99: milliseconds(latency.p99),
            p999: milliseconds(latency.p999),
            min: milliseconds(latency.min),
            max: milliseconds(latency.max),
            mean: milliseconds(latency.mean),
            count: latency.count,
        }
    }
}

/// Throughput block in [`MetricsDocumentV1`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThroughputDocumentV1 {
    /// Total requests observed by the recorder.
    pub total_requests: u64,
    /// Successful requests, including expected fuzzer rejections.
    pub successful_requests: u64,
    /// Mean requests per second.
    pub requests_per_sec: f64,
}

impl From<&ThroughputStats> for ThroughputDocumentV1 {
    fn from(throughput: &ThroughputStats) -> Self {
        Self {
            total_requests: throughput.total_requests,
            successful_requests: throughput.successful_requests,
            requests_per_sec: throughput.requests_per_sec,
        }
    }
}

/// Error block in [`MetricsDocumentV1`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorsDocumentV1 {
    /// Total requests that were not successful.
    pub total: u64,
    /// Counts by stable error-category label.
    pub by_category: ErrorBreakdownDocumentV1,
}

impl From<(&OutcomeCounts, &ScenarioMetrics)> for ErrorsDocumentV1 {
    fn from((outcomes, metrics): (&OutcomeCounts, &ScenarioMetrics)) -> Self {
        Self {
            total: metrics
                .throughput
                .total_requests
                .saturating_sub(metrics.throughput.successful_requests),
            by_category: ErrorBreakdownDocumentV1::from(outcomes),
        }
    }
}

/// Stable error-category labels in the v1 JSON contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBreakdownDocumentV1 {
    /// Calls that exceeded the hang threshold but recovered.
    #[serde(rename = "Hang")]
    pub hang: u64,
    /// Client-side timeout count.
    #[serde(rename = "Timeout")]
    pub timeout: u64,
    /// Server error count.
    #[serde(rename = "ServerError")]
    pub server_error: u64,
    /// MCP/JSON-RPC protocol error count.
    #[serde(rename = "ProtocolError")]
    pub protocol_error: u64,
    /// Server crash count.
    #[serde(rename = "Crash")]
    pub crash: u64,
    /// Malformed response count.
    #[serde(rename = "Malformed")]
    pub malformed: u64,
    /// Mid-request disconnect count.
    #[serde(rename = "Disconnected")]
    pub disconnected: u64,
    /// Caller-side cancellation count.
    #[serde(rename = "Cancelled")]
    pub cancelled: u64,
}

impl From<&OutcomeCounts> for ErrorBreakdownDocumentV1 {
    fn from(outcomes: &OutcomeCounts) -> Self {
        Self {
            hang: outcomes.hang,
            timeout: outcomes.timeout,
            server_error: outcomes.server_error,
            protocol_error: outcomes.protocol_error,
            crash: outcomes.crash,
            malformed: outcomes.malformed,
            disconnected: outcomes.disconnected,
            cancelled: outcomes.cancelled,
        }
    }
}

/// Process block in [`MetricsDocumentV1`].
///
/// This intentionally retains the original v1 surface. Richer process fields
/// remain available on [`Report`] and may be introduced as optional v1 fields
/// in a future additive change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessDocumentV1 {
    /// Peak resident memory in megabytes.
    pub peak_rss_mb: f64,
    /// Final resident memory in megabytes.
    pub final_rss_mb: f64,
    /// Mean CPU percentage.
    pub avg_cpu_pct: f64,
}

impl From<&ProcessStats> for ProcessDocumentV1 {
    fn from(process: &ProcessStats) -> Self {
        Self {
            peak_rss_mb: process.peak_rss_mb,
            final_rss_mb: process.final_rss_mb,
            avg_cpu_pct: process.avg_cpu_pct,
        }
    }
}

/// Threshold-violation row in [`MetricsDocumentV1`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThresholdDocumentV1 {
    /// Stable threshold slug.
    pub metric: String,
    /// Configured expectation.
    pub expected: String,
    /// Observed value.
    pub actual: String,
}

impl From<&ThresholdViolation> for ThresholdDocumentV1 {
    fn from(violation: &ThresholdViolation) -> Self {
        Self {
            metric: violation.kind.name().to_owned(),
            expected: violation.expected.clone(),
            actual: violation.actual.clone(),
        }
    }
}

/// Render a report through the canonical v1 wire model.
pub fn render_pretty_json(report: &Report) -> Result<String, ReportError> {
    Ok(MetricsDocumentV1::from(report).to_pretty_json()?)
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}
