//! Run report — what `Run::execute()` returns and what `Reporter`s render.
//!
//! See DESIGN.md §14.3 (types) + §17 (output formats).
//!
//! **M3 ownership:** Agent F (markdown + json reporters), Agent G (terminal
//! reporter), Agent H (Run orchestrator that builds Report). Other agents
//! consume via the LOCKED types below.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::coverage::CoverageReport;
use crate::metrics::ScenarioMetrics;
use crate::outcome::ScenarioOutcome;

/// One-stop report produced by `Run::execute()`. Reporters render this.
///
/// **Locked for M3.** Field additions are non-breaking; removal/rename require sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// ULID identifying this run; used for `runs/<id>/` directory naming.
    pub run_id: String,
    /// When the run started (wall clock).
    pub started_at: SystemTime,
    /// Total wall-clock duration from spawn to shutdown.
    pub duration: Duration,
    /// Scenario name (`Scenario::name()`).
    pub scenario_name: String,
    /// Server invocation that was tested.
    pub server_info: ServerInfo,
    /// Latency / throughput / outcome aggregates.
    pub metrics: ScenarioMetrics,
    /// Process-level resource samples (RSS / CPU over time).
    pub process: ProcessStats,
    /// Per-call structured outcome from the scenario itself.
    pub scenario_outcome: ScenarioOutcome,
    /// Path to the per-run trace.jsonl (relative to runs dir).
    pub trace_path: Option<PathBuf>,
    /// Threshold violations (empty = pass).
    pub threshold_violations: Vec<ThresholdViolation>,
    /// Tool coverage — which tools were `tools/list`-registered vs. actually
    /// exercised during the run. `None` when coverage tracking wasn't wired
    /// up (e.g. older runs deserialized from disk). M7 differentiator.
    #[serde(default)]
    pub coverage: Option<CoverageReport>,
}

impl Report {
    /// True if the run is overall successful.
    ///
    /// Configured thresholds remain the policy for partial application-level
    /// errors. A few conditions are unconditional correctness failures:
    ///
    /// - the scenario attempted no calls;
    /// - no attempted call produced a scenario-level success;
    /// - the scenario claimed more successes than attempts;
    /// - a deadlock or response divergence was detected;
    /// - any pooled worker failed to complete, so requested concurrency was
    ///   not actually exercised;
    /// - any session/transport teardown failed or exceeded its lifecycle
    ///   deadline;
    /// - a race check, deadlock probe, or fuzzer produced any unexpected
    ///   error or breached the configured hang threshold (a diagnostic cohort
    ///   cannot be called healthy when one member errored or responded late);
    ///   or
    /// - the recorder observed a deadlock, timeout, protocol/malformed
    ///   response, crash, disconnect, or cancellation.
    ///
    /// These guardrails prevent an invalid/no-op workload or a dead session
    /// from producing a green CI result merely because no thresholds were
    /// configured.
    pub fn passed(&self) -> bool {
        let outcome = &self.scenario_outcome;
        let recorded = &self.metrics.outcomes;
        // Use a predicate instead of summing untrusted/deserialized counters:
        // a wrapping release-mode sum could otherwise land back on zero.
        let has_hard_recorded_failure = [
            recorded.deadlock,
            recorded.timeout,
            recorded.protocol_error,
            recorded.crash,
            recorded.malformed,
            recorded.disconnected,
            recorded.cancelled,
        ]
        .into_iter()
        .any(|count| count > 0);
        let diagnostic_complete =
            !matches!(
                self.scenario_name.as_str(),
                "race_check" | "deadlock_probe" | "fuzzer"
            ) || (outcome.error_count == 0 && outcome.hang_count == 0 && recorded.hang == 0);

        self.threshold_violations.is_empty()
            && outcome.total_calls > 0
            && outcome.successful_calls > 0
            && outcome.successful_calls <= outcome.total_calls
            && outcome.deadlock_count == 0
            && outcome.divergence_count == 0
            && outcome.incomplete_worker_count == 0
            && outcome.teardown_failure_count == 0
            && diagnostic_complete
            && !has_hard_recorded_failure
    }
}

/// Identifies the server-under-test in a Report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Command name (e.g., `python`).
    pub command: String,
    /// Args to the command (e.g., `["-m", "my_mcp"]`).
    pub args: Vec<String>,
    /// Process id while running (None after shutdown).
    pub pid: Option<u32>,
    /// Server's reported `protocolVersion` from initialize, if any.
    pub protocol_version: Option<String>,
}

/// Process-level resource metrics over the run lifetime.
///
/// **M3 Agent G** populated `peak_rss_mb` / `final_rss_mb` / `avg_cpu_pct`
/// via `sysinfo`. **M6 Agent T** added the fd / thread peak + final pair
/// to drive the soak-leak heuristic.
///
/// The fd fields are best-effort: on platforms where sysinfo can't see
/// open file descriptors (Windows, sometimes macOS) they stay at `0`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProcessStats {
    /// Largest RSS observed (MB).
    pub peak_rss_mb: f64,
    /// RSS at end of run (MB).
    pub final_rss_mb: f64,
    /// RSS at the first sample (~one sample interval into the run), used as
    /// the start-of-run baseline for the `memory_growth_mb` threshold. `0`
    /// when no samples were collected. `#[serde(default)]` keeps older
    /// `metrics.json` (written before this field existed) deserializable.
    #[serde(default)]
    pub baseline_rss_mb: f64,
    /// Mean CPU% across all samples.
    pub avg_cpu_pct: f64,
    /// Largest open-file-descriptor count observed. `0` if unavailable on
    /// this platform.
    #[serde(default)]
    pub peak_fd: u64,
    /// File-descriptor count at the final sample. `0` if unavailable.
    #[serde(default)]
    pub final_fd: u64,
    /// Largest thread count observed. `0` if unavailable on this
    /// platform (sysinfo only exposes thread lists on Linux).
    #[serde(default)]
    pub peak_threads: u64,
    /// Thread count at the final sample. `0` if unavailable.
    #[serde(default)]
    pub final_threads: u64,
    /// Per-sample timeseries.
    pub samples: Vec<ProcessSample>,
}

/// Single point-in-time sample of the server process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSample {
    /// Offset from run start (seconds, fractional).
    pub at_secs: f64,
    /// Resident set size in megabytes.
    pub rss_mb: f64,
    /// CPU usage percentage (0.0..=100.0 per logical core).
    pub cpu_pct: f64,
    /// Open-file-descriptor count at this tick. `0` when sysinfo can't
    /// expose it on the current platform (Windows in particular).
    #[serde(default)]
    pub fd: u64,
    /// Thread count at this tick. `0` when sysinfo can't expose it
    /// (anything other than Linux today).
    #[serde(default)]
    pub threads: u64,
}

/// Which configured threshold a [`ThresholdViolation`] refers to.
///
/// Replaces the earlier free-form `metric: String` field — keeping the set
/// of valid threshold kinds in one place removes the typo class where
/// `evaluate_thresholds` and the reporters disagreed silently. Serialized
/// names match the original strings (`"p99_latency"`, `"error_rate"`, ...)
/// so `metrics.json` consumers don't break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ThresholdKind {
    /// `thresholds.p50_latency` — 50th percentile latency budget.
    #[serde(rename = "p50_latency")]
    P50Latency,
    /// `thresholds.p95_latency` — 95th percentile latency budget.
    #[serde(rename = "p95_latency")]
    P95Latency,
    /// `thresholds.p99_latency` — 99th percentile latency budget.
    #[serde(rename = "p99_latency")]
    P99Latency,
    /// `thresholds.p999_latency` — 99.9th percentile latency budget.
    #[serde(rename = "p999_latency")]
    P999Latency,
    /// `thresholds.error_rate` — max acceptable failure fraction (0.0..=1.0).
    #[serde(rename = "error_rate")]
    ErrorRate,
    /// `thresholds.memory_growth_mb` — RSS growth ceiling.
    #[serde(rename = "memory_growth_mb")]
    MemoryGrowthMb,
}

impl ThresholdKind {
    /// Display string used in reports + `metrics.json`. Stable across
    /// patch releases.
    pub fn name(&self) -> &'static str {
        match self {
            Self::P50Latency => "p50_latency",
            Self::P95Latency => "p95_latency",
            Self::P99Latency => "p99_latency",
            Self::P999Latency => "p999_latency",
            Self::ErrorRate => "error_rate",
            Self::MemoryGrowthMb => "memory_growth_mb",
        }
    }
}

impl std::fmt::Display for ThresholdKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One threshold violation. Built by `Thresholds::evaluate(&report)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdViolation {
    /// Which threshold was violated.
    #[serde(rename = "metric")]
    pub kind: ThresholdKind,
    /// Expected condition as a string (e.g., `"<= 500ms"`).
    pub expected: String,
    /// Actual measured value (e.g., `"812ms"`).
    pub actual: String,
}

/// Errors a Reporter may surface during render.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReportError {
    /// I/O writing the rendered output.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization failed (json reporter).
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Other format-specific failure.
    #[error("{0}")]
    Other(String),
}

/// Render a [`Report`] in some format (markdown / json / terminal).
///
/// **Locked for M3.** Implementations live in sibling modules.
pub trait Reporter {
    /// Render the report into a `String`. Streaming-to-Writer is a future API.
    fn render(&self, report: &Report) -> Result<String, ReportError>;
}

/// Format a `SystemTime` as ISO 8601 UTC at second precision
/// (e.g., `2026-05-10T07:30:00Z`).
///
/// Hand-rolled to avoid pulling in `chrono` / `time` crates for one timestamp.
pub fn format_iso8601_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|e| -(e.duration().as_secs() as i64));

    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert a Unix-epoch second count into a (Y, M, D, h, m, s) tuple in UTC.
///
/// Algorithm: Howard Hinnant's days_from_civil / civil_from_days, adapted to
/// stay in `i64` arithmetic. Handles negative epoch (pre-1970) correctly,
/// though we don't expect that in practice for run timestamps.
fn epoch_to_ymdhms(epoch_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let secs_per_day: i64 = 86_400;

    let days = epoch_secs.div_euclid(secs_per_day);
    let day_secs = epoch_secs.rem_euclid(secs_per_day);

    let h = (day_secs / 3600) as u32;
    let mi = ((day_secs % 3600) / 60) as u32;
    let s = (day_secs % 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y } as i32;

    (year, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    fn skeleton_report() -> Report {
        Report {
            run_id: "01TEST".into(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_secs(1),
            scenario_name: "test".into(),
            server_info: ServerInfo {
                command: "true".into(),
                args: Vec::new(),
                pid: None,
                protocol_version: None,
            },
            metrics: ScenarioMetrics::default(),
            process: ProcessStats::default(),
            scenario_outcome: ScenarioOutcome::default(),
            trace_path: None,
            threshold_violations: Vec::new(),
            coverage: None,
        }
    }

    #[test]
    fn passed_true_when_clean() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 1;
        r.scenario_outcome.successful_calls = 1;
        assert!(r.passed());
    }

    #[test]
    fn passed_false_when_no_calls_were_attempted() {
        let r = skeleton_report();
        assert!(!r.passed());
    }

    #[test]
    fn passed_false_when_every_attempt_failed() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 3;
        r.scenario_outcome.error_count = 3;
        assert!(!r.passed());
    }

    #[test]
    fn passed_false_when_teardown_failed_after_successful_calls() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 3;
        r.scenario_outcome.successful_calls = 3;
        r.scenario_outcome.teardown_failure_count = 1;
        assert!(
            !r.passed(),
            "successful calls must not hide an unclean server lifecycle"
        );
    }

    #[test]
    fn passed_false_when_success_count_exceeds_attempts() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 1;
        r.scenario_outcome.successful_calls = 2;
        assert!(!r.passed());
    }

    #[test]
    fn passed_allows_partial_application_errors_with_no_threshold_violation() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 10;
        r.scenario_outcome.successful_calls = 9;
        r.scenario_outcome.error_count = 1;
        r.metrics.outcomes.server_error = 1;
        assert!(
            r.passed(),
            "partial application errors remain governed by error-rate thresholds"
        );
    }

    #[test]
    fn passed_rejects_incomplete_race_check_even_for_nonterminal_error() {
        let mut r = skeleton_report();
        r.scenario_name = "race_check".into();
        r.scenario_outcome.total_calls = 2;
        r.scenario_outcome.successful_calls = 2;
        r.scenario_outcome.error_count = 1;
        r.metrics.outcomes.server_error = 1;
        assert!(
            !r.passed(),
            "a partial race cohort must not pass as a clean comparison"
        );
    }

    #[test]
    fn passed_rejects_deadlock_probe_error_even_with_surviving_successes() {
        let mut r = skeleton_report();
        r.scenario_name = "deadlock_probe".into();
        r.scenario_outcome.total_calls = 4;
        r.scenario_outcome.successful_calls = 3;
        r.scenario_outcome.error_count = 1;
        r.metrics.outcomes.server_error = 1;
        assert!(
            !r.passed(),
            "a diagnostic probe must not render PASS when one call errored"
        );
    }

    #[test]
    fn passed_rejects_mixed_success_and_slow_diagnostic_cohorts() {
        for scenario_name in ["race_check", "deadlock_probe", "fuzzer"] {
            let mut r = skeleton_report();
            r.scenario_name = scenario_name.into();
            r.scenario_outcome.total_calls = 2;
            r.scenario_outcome.successful_calls = 1;
            r.scenario_outcome.hang_count = 1;
            r.metrics.outcomes.success = 1;
            r.metrics.outcomes.hang = 1;
            assert!(
                !r.passed(),
                "{scenario_name} must fail when one diagnostic probe breaches the hang threshold"
            );
        }
    }

    #[test]
    fn passed_keeps_slow_load_calls_under_threshold_policy() {
        let mut r = skeleton_report();
        r.scenario_name = "sustained".into();
        r.scenario_outcome.total_calls = 2;
        r.scenario_outcome.successful_calls = 1;
        r.scenario_outcome.hang_count = 1;
        r.metrics.outcomes.success = 1;
        r.metrics.outcomes.hang = 1;
        assert!(
            r.passed(),
            "non-diagnostic load scenarios keep partial slow calls under configured threshold policy"
        );
    }

    #[test]
    fn passed_accepts_expected_fuzzer_rejections_but_rejects_unexpected_errors() {
        let mut r = skeleton_report();
        r.scenario_name = "fuzzer".into();
        r.scenario_outcome.total_calls = 2;
        r.scenario_outcome.successful_calls = 2;
        r.metrics.outcomes.expected_rejection = 2;
        assert!(
            r.passed(),
            "expected server rejections are healthy fuzz probes"
        );

        r.scenario_outcome.total_calls = 3;
        r.scenario_outcome.error_count = 1;
        r.metrics.outcomes.server_error = 1;
        assert!(!r.passed(), "unexpected fuzzer errors must fail closed");
    }

    #[test]
    fn passed_rejects_silently_downgraded_pool_for_every_scenario() {
        let mut r = skeleton_report();
        r.scenario_name = "sustained".into();
        r.scenario_outcome.total_calls = 20;
        r.scenario_outcome.successful_calls = 20;
        r.scenario_outcome.error_count = 1;
        r.scenario_outcome.incomplete_worker_count = 1;
        r.metrics.outcomes.server_error = 1;
        assert!(
            !r.passed(),
            "missing a requested worker must fail even when survivors succeeded"
        );
    }

    #[test]
    fn passed_false_when_thresholds_violated() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 1;
        r.scenario_outcome.successful_calls = 1;
        r.threshold_violations.push(ThresholdViolation {
            kind: ThresholdKind::P99Latency,
            expected: "<= 100ms".into(),
            actual: "234ms".into(),
        });
        assert!(!r.passed());
    }

    #[test]
    fn passed_false_when_deadlock_detected_even_with_clean_thresholds() {
        // Regression for QF-1: previously `passed()` only checked threshold
        // violations, so a deadlock-probe run with no thresholds configured
        // would print PASS while exiting non-zero. Now deadlocks always fail.
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 1;
        r.scenario_outcome.deadlock_count = 1;
        assert!(!r.passed());
    }

    #[test]
    fn passed_false_when_responses_diverge() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 4;
        r.scenario_outcome.successful_calls = 4;
        r.scenario_outcome.divergence_count = 1;
        assert!(!r.passed());
    }

    #[test]
    fn passed_false_on_terminal_session_outcome_even_with_successes() {
        for set_terminal in [
            |m: &mut ScenarioMetrics| m.outcomes.timeout = 1,
            |m: &mut ScenarioMetrics| m.outcomes.crash = 1,
            |m: &mut ScenarioMetrics| m.outcomes.disconnected = 1,
            |m: &mut ScenarioMetrics| m.outcomes.cancelled = 1,
        ] {
            let mut r = skeleton_report();
            r.scenario_outcome.total_calls = 2;
            r.scenario_outcome.successful_calls = 1;
            r.scenario_outcome.error_count = 1;
            set_terminal(&mut r.metrics);
            assert!(!r.passed());
        }
    }

    #[test]
    fn passed_false_on_recorded_protocol_correctness_failure() {
        for set_failure in [
            |m: &mut ScenarioMetrics| m.outcomes.deadlock = 1,
            |m: &mut ScenarioMetrics| m.outcomes.protocol_error = 1,
            |m: &mut ScenarioMetrics| m.outcomes.malformed = 1,
        ] {
            let mut r = skeleton_report();
            r.scenario_outcome.total_calls = 2;
            r.scenario_outcome.successful_calls = 1;
            r.scenario_outcome.error_count = 1;
            set_failure(&mut r.metrics);
            assert!(!r.passed());
        }
    }

    #[test]
    fn passed_cannot_wrap_adversarial_failure_counters_back_to_zero() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = u64::MAX;
        r.scenario_outcome.successful_calls = 1;
        r.metrics.outcomes.deadlock = u64::MAX;
        r.metrics.outcomes.timeout = 1;
        assert!(
            !r.passed(),
            "deserialized failure counters must be tested independently, not summed with wrapping arithmetic"
        );
    }

    #[test]
    fn passed_false_when_both_signals_fail() {
        let mut r = skeleton_report();
        r.scenario_outcome.total_calls = 1;
        r.scenario_outcome.deadlock_count = 1;
        r.threshold_violations.push(ThresholdViolation {
            kind: ThresholdKind::ErrorRate,
            expected: "<= 1%".into(),
            actual: "100%".into(),
        });
        assert!(!r.passed());
    }

    #[test]
    fn threshold_kind_name_round_trips_to_json() {
        let v = ThresholdViolation {
            kind: ThresholdKind::P99Latency,
            expected: "<= 100ms".into(),
            actual: "234ms".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(
            json.contains(r#""metric":"p99_latency""#),
            "metrics.json wire format should keep the legacy `metric` key + snake_case name; got {json}"
        );
        let back: ThresholdViolation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, ThresholdKind::P99Latency);
    }

    #[test]
    fn iso8601_unix_epoch() {
        assert_eq!(format_iso8601_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_known_date() {
        // 2026-05-10T07:30:00Z. Verified via the original test before the
        // helper moved here.
        let t = UNIX_EPOCH + Duration::from_secs(1_778_398_200);
        assert_eq!(format_iso8601_utc(t), "2026-05-10T07:30:00Z");
    }

    #[test]
    fn iso8601_y2k() {
        // 2000-03-01T00:00:00Z — exercises the leap-year edge.
        let t = UNIX_EPOCH + Duration::from_secs(951_868_800);
        assert_eq!(format_iso8601_utc(t), "2000-03-01T00:00:00Z");
    }
}
