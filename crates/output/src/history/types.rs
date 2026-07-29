//! Stable history sample and trend-report value types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::regression::RegressionThresholds;
use crate::report::wire::MetricsDocumentV1;

/// Current on-disk history sample schema version.
pub const HISTORY_SAMPLE_SCHEMA_VERSION: u32 = 1;

/// Compact, mergeable record derived from one `metrics.json` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistorySampleV1 {
    /// History sample schema version; currently always `1`.
    pub schema_version: u32,
    /// Operator-chosen benchmark series.
    pub series: String,
    /// Source run ULID.
    pub run_id: String,
    /// Source run start in ISO 8601 UTC.
    pub started_at: String,
    /// Scenario name.
    pub scenario: String,
    /// Negotiated MCP protocol revision, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Optional stable execution-topology fingerprint.
    ///
    /// Distributed and local runs should not share a baseline unless their
    /// topology is intentionally declared equivalent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_fingerprint: Option<String>,
    /// Median latency in milliseconds.
    pub p50_ms: f64,
    /// 95th-percentile latency in milliseconds.
    pub p95_ms: f64,
    /// 99th-percentile latency in milliseconds.
    pub p99_ms: f64,
    /// Aggregate requests per second.
    pub requests_per_sec: f64,
    /// Error rate in percentage points.
    pub error_rate_pct: f64,
    /// Scenario-level deadlock count.
    pub deadlock_count: u32,
    /// Scenario-level hang count.
    pub hang_count: u32,
    /// Absolute correctness/threshold verdict of the source run.
    pub passed: bool,
}

impl HistorySampleV1 {
    /// Derive a history sample from the canonical v1 metrics document.
    pub fn from_metrics(
        series: impl Into<String>,
        document: &MetricsDocumentV1,
        execution_fingerprint: Option<String>,
    ) -> Result<Self, HistoryError> {
        let total = document.throughput.total_requests;
        let error_rate_pct = if total == 0 {
            0.0
        } else {
            document.errors.total as f64 / total as f64 * 100.0
        };
        let sample = Self {
            schema_version: HISTORY_SAMPLE_SCHEMA_VERSION,
            series: series.into(),
            run_id: document.run_id.clone(),
            started_at: document.started_at.clone(),
            scenario: document.scenario.name.clone(),
            protocol_version: document.server.protocol_version.clone(),
            execution_fingerprint,
            p50_ms: document.latency_ms.p50,
            p95_ms: document.latency_ms.p95,
            p99_ms: document.latency_ms.p99,
            requests_per_sec: document.throughput.requests_per_sec,
            error_rate_pct,
            deadlock_count: document.deadlock_count,
            hang_count: document.hang_count,
            passed: document.passed,
        };
        sample.validate()?;
        Ok(sample)
    }

    /// Validate the stable sample invariants.
    pub fn validate(&self) -> Result<(), HistoryError> {
        if self.schema_version != HISTORY_SAMPLE_SCHEMA_VERSION {
            return Err(HistoryError::UnsupportedSchema(self.schema_version));
        }
        validate_series_name(&self.series)?;
        validate_run_id(&self.run_id)?;
        if self.started_at.is_empty() {
            return Err(HistoryError::InvalidSample("started_at is empty"));
        }
        if self.scenario.is_empty() {
            return Err(HistoryError::InvalidSample("scenario is empty"));
        }
        if self
            .execution_fingerprint
            .as_deref()
            .is_some_and(str::is_empty)
        {
            return Err(HistoryError::InvalidSample(
                "execution_fingerprint is empty",
            ));
        }
        for (name, value) in [
            ("p50_ms", self.p50_ms),
            ("p95_ms", self.p95_ms),
            ("p99_ms", self.p99_ms),
            ("requests_per_sec", self.requests_per_sec),
            ("error_rate_pct", self.error_rate_pct),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(HistoryError::InvalidNumeric(name));
            }
        }
        if self.error_rate_pct > 100.0 {
            return Err(HistoryError::InvalidNumeric("error_rate_pct"));
        }
        Ok(())
    }

    /// Whether two samples belong to the same comparable cohort.
    pub fn same_cohort(&self, other: &Self) -> bool {
        self.series == other.series
            && self.scenario == other.scenario
            && self.protocol_version == other.protocol_version
            && self.execution_fingerprint == other.execution_fingerprint
    }
}

/// Policy for a rolling multi-run baseline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrendPolicy {
    /// Maximum number of latest eligible samples in the median baseline.
    pub window: usize,
    /// Minimum eligible samples required before relative gates activate.
    pub min_samples: usize,
    /// Existing p99/error/deadlock comparison policy.
    pub regression: RegressionThresholds,
    /// Optional throughput-drop threshold in percent.
    pub max_rps_drop_pct: Option<f64>,
}

impl Default for TrendPolicy {
    fn default() -> Self {
        Self {
            window: 10,
            min_samples: 3,
            regression: RegressionThresholds::default(),
            max_rps_drop_pct: Some(10.0),
        }
    }
}

impl TrendPolicy {
    /// Validate policy bounds before analyzing history.
    pub fn validate(&self) -> Result<(), HistoryError> {
        if self.window == 0 {
            return Err(HistoryError::InvalidPolicy(
                "window must be greater than zero",
            ));
        }
        if self.min_samples == 0 || self.min_samples > self.window {
            return Err(HistoryError::InvalidPolicy(
                "min_samples must be in 1..=window",
            ));
        }
        if !self.regression.p99_pct.is_finite() || self.regression.p99_pct <= 0.0 {
            return Err(HistoryError::InvalidPolicy(
                "p99 regression threshold must be finite and greater than zero",
            ));
        }
        if !self.regression.error_rate_pp.is_finite() || self.regression.error_rate_pp <= 0.0 {
            return Err(HistoryError::InvalidPolicy(
                "error-rate regression threshold must be finite and greater than zero",
            ));
        }
        if let Some(threshold) = self.max_rps_drop_pct
            && (!threshold.is_finite() || threshold <= 0.0)
        {
            return Err(HistoryError::InvalidPolicy(
                "throughput regression threshold must be finite and greater than zero",
            ));
        }
        Ok(())
    }
}

/// Overall result of trend analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendStatus {
    /// Not enough comparable passing samples to activate relative gates.
    WarmingUp,
    /// Baseline is ready and no relative gate fired.
    Clean,
    /// At least one relative gate fired.
    Regressed,
}

/// Direction assigned to one trend metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    /// Current value is meaningfully worse.
    Regressed,
    /// Current value is meaningfully better.
    Improved,
    /// Current change is inside policy or purely informational.
    Neutral,
}

/// One baseline-to-current metric comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendMetric {
    /// Stable metric slug.
    pub metric: String,
    /// Median value across the eligible baseline window.
    pub baseline: f64,
    /// Value from the current run.
    pub current: f64,
    /// Signed arithmetic delta (`current - baseline`).
    pub change: f64,
    /// Signed percentage delta when the baseline is non-zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_pct: Option<f64>,
    /// Classified direction.
    pub direction: TrendDirection,
    /// Whether this metric participates in the regression gate.
    pub gating: bool,
}

/// Structured rolling-baseline result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendReport {
    /// Benchmark series.
    pub series: String,
    /// Current source run id.
    pub current_run_id: String,
    /// Analysis status.
    pub status: TrendStatus,
    /// Comparable passing samples used in the baseline.
    pub baseline_sample_count: usize,
    /// Samples required before gates activate.
    pub required_sample_count: usize,
    /// All available metric comparisons.
    pub metrics: Vec<TrendMetric>,
    /// Gating metrics classified as regressions.
    pub regressions: Vec<TrendMetric>,
    /// Convenience verdict used by CLI gates.
    pub has_regression: bool,
}

/// History storage and analysis errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistoryError {
    /// Series names are deliberately path-safe and cross-platform.
    #[error("invalid history series name: {0}")]
    InvalidSeries(&'static str),
    /// Run ids become filenames and must be path-safe.
    #[error("invalid history run id")]
    InvalidRunId,
    /// A sample field violates the stable contract.
    #[error("invalid history sample: {0}")]
    InvalidSample(&'static str),
    /// A numeric sample is negative or non-finite.
    #[error("invalid history numeric field `{0}`")]
    InvalidNumeric(&'static str),
    /// The store contains a future or unsupported schema.
    #[error("unsupported history sample schema version {0}")]
    UnsupportedSchema(u32),
    /// Trend-policy bounds are invalid.
    #[error("invalid trend policy: {0}")]
    InvalidPolicy(&'static str),
    /// A history file exceeded the configured safety limit.
    #[error("history sample exceeds the configured file-size limit")]
    SampleTooLarge,
    /// The store exceeded the configured sample-count safety limit.
    #[error("history store exceeds the configured sample-count limit")]
    TooManySamples,
    /// The same run id was recorded with different content.
    #[error("history store contains a conflicting duplicate run id")]
    ConflictingDuplicate,
    /// Filesystem access failed.
    #[error("history {operation} failed at {path}")]
    Io {
        /// Sanitized operation label.
        operation: &'static str,
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Original I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A stored JSON sample was malformed.
    #[error("history JSON parse failed at {path}")]
    Json {
        /// Path of the malformed sample.
        path: PathBuf,
        /// JSON parser diagnostic (line/column, never file contents).
        #[source]
        source: serde_json::Error,
    },
}

/// Validate a history series name without silently slugifying it.
pub fn validate_series_name(value: &str) -> Result<(), HistoryError> {
    if value.is_empty() || value.len() > 64 {
        return Err(HistoryError::InvalidSeries(
            "must contain between 1 and 64 ASCII characters",
        ));
    }
    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(HistoryError::InvalidSeries(
            "must start with an alphanumeric and contain only alphanumerics, '.', '_' or '-'",
        ));
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number))
        || stem
            .strip_prefix("LPT")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number));
    if reserved {
        return Err(HistoryError::InvalidSeries(
            "is a reserved Windows device name",
        ));
    }
    Ok(())
}

pub(super) fn validate_run_id(value: &str) -> Result<(), HistoryError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(HistoryError::InvalidRunId);
    }
    Ok(())
}
