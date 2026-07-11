//! Breaking-point detector — find the concurrency at which a server's
//! latency or error-rate budget blows.
//!
//! See DESIGN.md §10.5 (parity row "Breaking point detection") and §8 entry
//! for the `ramp` scenario. The detector is fed sample-by-sample as the ramp
//! steps through concurrency levels; the first sample that violates either
//! threshold marks the break point.
//!
//! # Algorithm
//!
//! For each [`ScenarioMetrics`] sample observed at a given concurrency:
//!
//! 1. Compute `error_rate = (total_requests - successful_requests) / total_requests`.
//!    Empty samples (no requests) contribute `0.0` so a quiet step doesn't
//!    falsely look like a 100% failure.
//! 2. If `metrics.latency.p99 > config.max_p99_latency` **or**
//!    `error_rate > config.max_error_rate`, mark this sample as the
//!    **first violator**. `last_known_good` is the previous sample's
//!    concurrency (or `None` if the very first step already violated).
//!
//! `window_secs` is reserved for future windowed-average extensions; the
//! current pass uses **first-violator semantics**, which is simpler, cheaper,
//! and matches the reaatech/mcp-load-test behavior. Window-averaged
//! detection (smooth out a single noisy step) is a future addition.
//!
//! # Why first-violator and not "averaged over window_secs"?
//!
//! - Each ramp step already aggregates `step_duration` worth of calls into
//!   one [`ScenarioMetrics`] sample, so per-sample p99 is already smoothed.
//! - First-violator gives a deterministic answer without needing a
//!   second-order tunable; users can raise `step_duration` if they're seeing
//!   noisy single-step false positives.
//! - The `window_secs` field is kept on the config so a later refinement
//!   (e.g., "violation only if 2 of last 3 samples exceed budget") doesn't
//!   require a breaking schema change.

use std::time::Duration;

use mcp_loadtest_core::metrics::ScenarioMetrics;

/// Configuration for the breaking-point detector.
///
/// Both thresholds must be set; passing `Duration::MAX` / `f64::INFINITY`
/// effectively disables one side. `window_secs` is forward-compatible and
/// not yet consulted in M5 (see module docs).
#[derive(Debug, Clone)]
pub struct BreakingPointConfig {
    /// Latency budget — first sample whose `metrics.latency.p99` exceeds
    /// this is the break point.
    pub max_p99_latency: Duration,
    /// Error-rate budget in `[0.0, 1.0]`. First sample whose computed
    /// `(total - successful) / total` exceeds this is the break point.
    pub max_error_rate: f64,
    /// Reserved for future windowed-average detection. Recommended default
    /// is `step_duration.as_secs_f64()` (= one step), which matches the
    /// current first-violator pass.
    pub window_secs: f64,
}

/// Streaming detector — fed one [`ScenarioMetrics`] per ramp step.
///
/// Cheap to construct; samples accumulate in insertion order. Call
/// [`Self::breaking_point`] at any time to get a [`BreakingPointReport`].
pub struct BreakingPointDetector {
    config: BreakingPointConfig,
    samples: Vec<(u32, ScenarioMetrics)>,
}

/// Result of running the detector over its accumulated samples.
#[derive(Debug, Clone)]
pub struct BreakingPointReport {
    /// Concurrency at the first sample that violated either threshold,
    /// or `None` if no sample exceeded the budgets.
    pub broke_at_concurrent: Option<u32>,
    /// Concurrency of the previous (non-violating) sample. `None` if the
    /// very first sample already violated, or no samples were recorded.
    pub last_known_good: Option<u32>,
    /// Every sample observed, in observation order. Useful for plots /
    /// downstream report rendering.
    pub samples: Vec<(u32, ScenarioMetrics)>,
    /// Human-readable description of which threshold tripped, e.g.
    /// `"p99 latency 234ms > 100ms budget"` or
    /// `"error rate 5.2% > 1.0% budget"`. `None` if no break point was
    /// found.
    pub trigger: Option<String>,
}

impl BreakingPointDetector {
    /// Construct a fresh detector. Configuration is owned so callers can
    /// drop their original.
    pub fn new(config: BreakingPointConfig) -> Self {
        Self {
            config,
            samples: Vec::new(),
        }
    }

    /// Append one observation for ramp-level `concurrent`.
    ///
    /// Samples are stored in observation order — the order the ramp scenario
    /// fed them in. Out-of-order calls are recorded as-is (the detector
    /// doesn't sort), so callers driving stepped ramps should observe in
    /// monotonically increasing concurrency.
    pub fn observe(&mut self, concurrent: u32, metrics: ScenarioMetrics) {
        self.samples.push((concurrent, metrics));
    }

    /// Build a [`BreakingPointReport`] from the samples observed so far.
    ///
    /// Scans samples in order; the first one whose `latency.p99` or computed
    /// `error_rate` exceeds the configured budget marks the break point.
    /// The returned report owns a copy of every sample observed.
    pub fn breaking_point(&self) -> BreakingPointReport {
        let mut last_known_good: Option<u32> = None;

        for (concurrent, metrics) in &self.samples {
            // Latency check — only trip if there are samples to draw a p99
            // from; an empty histogram has p99 = 0 which would never
            // exceed any sane budget anyway, but be defensive.
            if metrics.latency.count > 0 && metrics.latency.p99 > self.config.max_p99_latency {
                let trigger = format!(
                    "p99 latency {}ms > {}ms budget at concurrency={}",
                    metrics.latency.p99.as_millis(),
                    self.config.max_p99_latency.as_millis(),
                    concurrent,
                );
                return BreakingPointReport {
                    broke_at_concurrent: Some(*concurrent),
                    last_known_good,
                    samples: self.samples.clone(),
                    trigger: Some(trigger),
                };
            }

            let error_rate = compute_error_rate(metrics);
            if error_rate > self.config.max_error_rate {
                let trigger = format!(
                    "error rate {:.2}% > {:.2}% budget at concurrency={}",
                    error_rate * 100.0,
                    self.config.max_error_rate * 100.0,
                    concurrent,
                );
                return BreakingPointReport {
                    broke_at_concurrent: Some(*concurrent),
                    last_known_good,
                    samples: self.samples.clone(),
                    trigger: Some(trigger),
                };
            }

            last_known_good = Some(*concurrent);
        }

        BreakingPointReport {
            broke_at_concurrent: None,
            last_known_good,
            samples: self.samples.clone(),
            trigger: None,
        }
    }
}

/// `(total - successful) / total`, with `0.0` for empty windows so a step
/// that recorded zero calls doesn't masquerade as 100% failure.
fn compute_error_rate(metrics: &ScenarioMetrics) -> f64 {
    let total = metrics.throughput.total_requests;
    if total == 0 {
        return 0.0;
    }
    let failed = total.saturating_sub(metrics.throughput.successful_requests);
    failed as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_loadtest_core::metrics::{
        LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats,
    };

    fn make_metrics(p99_us: u64, total: u64, successful: u64) -> ScenarioMetrics {
        ScenarioMetrics {
            latency: LatencyStats {
                p50: Duration::from_micros(p99_us / 2),
                p95: Duration::from_micros(p99_us),
                p99: Duration::from_micros(p99_us),
                p999: Duration::from_micros(p99_us),
                mean: Duration::from_micros(p99_us / 2),
                min: Duration::from_micros(0),
                max: Duration::from_micros(p99_us),
                count: total,
            },
            throughput: ThroughputStats {
                total_requests: total,
                successful_requests: successful,
                requests_per_sec: total as f64,
            },
            outcomes: OutcomeCounts {
                success: successful,
                ..OutcomeCounts::default()
            },
        }
    }

    #[test]
    fn no_violation_returns_no_break_point() {
        let cfg = BreakingPointConfig {
            max_p99_latency: Duration::from_millis(100),
            max_error_rate: 0.01,
            window_secs: 1.0,
        };
        let mut det = BreakingPointDetector::new(cfg);
        det.observe(1, make_metrics(1_000, 100, 100));
        det.observe(5, make_metrics(2_000, 100, 100));
        det.observe(10, make_metrics(5_000, 100, 100));

        let report = det.breaking_point();
        assert!(report.broke_at_concurrent.is_none());
        assert_eq!(report.last_known_good, Some(10));
        assert!(report.trigger.is_none());
        assert_eq!(report.samples.len(), 3);
    }

    #[test]
    fn p99_violation_marks_first_offender() {
        let cfg = BreakingPointConfig {
            max_p99_latency: Duration::from_millis(50),
            max_error_rate: 1.0,
            window_secs: 1.0,
        };
        let mut det = BreakingPointDetector::new(cfg);
        det.observe(1, make_metrics(1_000, 100, 100)); // 1ms p99 — fine
        det.observe(5, make_metrics(10_000, 100, 100)); // 10ms p99 — fine
        det.observe(10, make_metrics(60_000, 100, 100)); // 60ms — break

        let report = det.breaking_point();
        assert_eq!(report.broke_at_concurrent, Some(10));
        assert_eq!(report.last_known_good, Some(5));
        assert!(report.trigger.is_some());
        assert!(report.trigger.unwrap().contains("p99 latency"));
    }

    #[test]
    fn first_sample_violation_has_no_last_known_good() {
        let cfg = BreakingPointConfig {
            max_p99_latency: Duration::from_millis(1),
            max_error_rate: 1.0,
            window_secs: 1.0,
        };
        let mut det = BreakingPointDetector::new(cfg);
        det.observe(1, make_metrics(60_000, 10, 10)); // already over budget

        let report = det.breaking_point();
        assert_eq!(report.broke_at_concurrent, Some(1));
        assert_eq!(report.last_known_good, None);
    }

    #[test]
    fn error_rate_violation_marks_first_offender() {
        let cfg = BreakingPointConfig {
            max_p99_latency: Duration::from_secs(60),
            max_error_rate: 0.05,
            window_secs: 1.0,
        };
        let mut det = BreakingPointDetector::new(cfg);
        det.observe(1, make_metrics(500, 100, 100)); // 0% errors
        det.observe(5, make_metrics(500, 100, 95)); // 5% — equal, not over
        det.observe(10, make_metrics(500, 100, 90)); // 10% — break

        let report = det.breaking_point();
        assert_eq!(report.broke_at_concurrent, Some(10));
        assert_eq!(report.last_known_good, Some(5));
        assert!(report.trigger.unwrap().contains("error rate"));
    }

    #[test]
    fn empty_window_does_not_trip_error_rate() {
        let cfg = BreakingPointConfig {
            max_p99_latency: Duration::from_secs(60),
            max_error_rate: 0.0,
            window_secs: 1.0,
        };
        let mut det = BreakingPointDetector::new(cfg);
        det.observe(1, make_metrics(0, 0, 0));

        let report = det.breaking_point();
        assert!(report.broke_at_concurrent.is_none());
    }

    #[test]
    fn empty_detector_reports_no_break_point() {
        let cfg = BreakingPointConfig {
            max_p99_latency: Duration::from_millis(100),
            max_error_rate: 0.01,
            window_secs: 1.0,
        };
        let det = BreakingPointDetector::new(cfg);
        let report = det.breaking_point();
        assert!(report.broke_at_concurrent.is_none());
        assert!(report.last_known_good.is_none());
        assert!(report.samples.is_empty());
    }
}
