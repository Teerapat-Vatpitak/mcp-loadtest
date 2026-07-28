//! Threshold evaluation — pure functions over [`Config`] + [`Report`].
//!
//! Split out of `run.rs` so the orchestrator stays focused on lifecycle.
//! See DESIGN.md §15.4 for the threshold semantics.

use std::time::Duration;

use mcp_loadtest_core::config::Config;
use mcp_loadtest_core::report::{ProcessSample, Report, ThresholdKind, ThresholdViolation};

/// Evaluate `config.thresholds` against the metrics in `report`. Returns the
/// list of violations (empty = pass). See DESIGN.md §15.4.
pub(super) fn evaluate_thresholds(config: &Config, report: &Report) -> Vec<ThresholdViolation> {
    let mut violations = Vec::new();
    let t = &config.thresholds;
    let m = &report.metrics;

    if let Some(p50) = t.p50_latency {
        check_latency(
            ThresholdKind::P50Latency,
            p50,
            m.latency.p50,
            m.latency.count,
            &mut violations,
        );
    }
    if let Some(p95) = t.p95_latency {
        check_latency(
            ThresholdKind::P95Latency,
            p95,
            m.latency.p95,
            m.latency.count,
            &mut violations,
        );
    }
    if let Some(p99) = t.p99_latency {
        check_latency(
            ThresholdKind::P99Latency,
            p99,
            m.latency.p99,
            m.latency.count,
            &mut violations,
        );
    }
    if let Some(p999) = t.p999_latency {
        check_latency(
            ThresholdKind::P999Latency,
            p999,
            m.latency.p999,
            m.latency.count,
            &mut violations,
        );
    }

    if let Some(max_rate) = t.error_rate {
        let total = m.throughput.total_requests;
        let success = m.throughput.successful_requests;
        if total == 0 {
            violations.push(ThresholdViolation {
                kind: ThresholdKind::ErrorRate,
                expected: format!("<= {max_rate}"),
                actual: "unavailable: no recorder request outcomes".to_owned(),
            });
        } else {
            // total > success because error_count == total - successful, but
            // remain defensive about underflow.
            let errors = total.saturating_sub(success);
            let actual = errors as f64 / total as f64;
            if actual > max_rate {
                violations.push(ThresholdViolation {
                    kind: ThresholdKind::ErrorRate,
                    expected: format!("<= {max_rate}"),
                    actual: format!("{actual:.4}"),
                });
            }
        }
    }

    // `memory_growth_mb` measures RSS growth above the start-of-run
    // baseline: `peak − baseline_rss_mb`. `baseline_rss_mb` is the first
    // sampled RSS (~one sample interval into the run). Using the peak (not
    // the final) RSS catches both a monotonic leak (peak == final, well
    // above baseline) and a transient spike that the process later frees —
    // either is "memory the server grew by during the run". A steady-state
    // high-RSS process (e.g. a 200 MB interpreter that never grows) sits at
    // peak ≈ baseline, so growth ≈ 0 and it doesn't false-positive.
    if let Some(max_growth) = t.memory_growth_mb {
        let baseline = report.process.baseline_rss_mb;
        let peak = report.process.peak_rss_mb;
        let expected =
            format!("<= {max_growth} MB (peak - baseline; requires finite process RSS samples)");
        let non_finite_sample = report
            .process
            .samples
            .iter()
            .position(|sample| !sample.rss_mb.is_finite());
        if report.process.samples.is_empty() {
            push_unavailable_process_violation(
                expected,
                "unavailable: no process RSS samples".to_owned(),
                &mut violations,
            );
        } else if !peak.is_finite() || !baseline.is_finite() || non_finite_sample.is_some() {
            let detail = if !baseline.is_finite() {
                format!("baseline RSS is {baseline}")
            } else if !peak.is_finite() {
                format!("peak RSS is {peak}")
            } else if let Some(index) = non_finite_sample {
                format!("RSS sample {index} is non-finite")
            } else {
                "RSS evidence is unavailable".to_owned()
            };
            push_unavailable_process_violation(
                expected,
                format!("unavailable: non-finite RSS evidence ({detail})"),
                &mut violations,
            );
        } else {
            let observed_growth = (peak - baseline).max(0.0);
            if observed_growth > max_growth {
                violations.push(ThresholdViolation {
                    kind: ThresholdKind::MemoryGrowthMb,
                    expected,
                    actual: format!("{observed_growth:.2} MB"),
                });
            }
        }
    }

    // `rss_leak_mb_per_sec` complements `memory_growth_mb` rather than
    // duplicating it: the least-squares slope over the whole RSS timeseries
    // catches a slow, steady leak that never clears the absolute-growth bar
    // within a single run (e.g. +0.3 MB/s over a 60s run is only +18 MB —
    // under a 50 MB budget — yet extrapolates to ~+1 GB/hour), while the
    // absolute peak-minus-baseline check catches a step-jump or transient
    // spike that a long flat-then-spike trajectory flattens out of the
    // fitted slope. Run both for full coverage.
    if let Some(max_slope) = t.rss_leak_mb_per_sec {
        check_rss_slope(max_slope, &report.process.samples, &mut violations);
    }

    violations
}

/// Minimum number of process samples before the `rss_leak_mb_per_sec`
/// threshold is evaluated. Two points always fit a line *exactly*, so a
/// single tick of sampling jitter would read as a "leak"; three is the
/// smallest series where the regression has a degree of freedom left over
/// to average noise out. A configured threshold fails closed when fewer
/// samples are available (or when they cannot produce a finite slope).
const MIN_RSS_SLOPE_SAMPLES: usize = 3;

/// Evaluate the `rss_leak_mb_per_sec` threshold: fit a least-squares line
/// to the chronological `(at_secs, rss_mb)` timeseries via
/// [`crate::scenario::soak::detect_leak`] and push a violation when the
/// slope exceeds `max_slope` MB/s.
///
/// `at_secs` offsets are relative to sampler start rather than to the
/// first sample, but a least-squares slope is invariant under time
/// translation, so the offsets are used as-is.
///
/// The violation reuses [`ThresholdKind::MemoryGrowthMb`] — the same
/// precedent as [`evaluate_tool_slos`] reusing `P99Latency` — because the
/// serialized kind-slug set is a `metrics.json` compatibility surface; the
/// `expected` string ("least-squares RSS slope" vs "peak - baseline")
/// disambiguates which RSS check tripped.
fn check_rss_slope(
    max_slope: f64,
    samples: &[ProcessSample],
    violations: &mut Vec<ThresholdViolation>,
) {
    let expected = format!(
        "<= {max_slope} MB/s (least-squares RSS slope; requires at least \
         {MIN_RSS_SLOPE_SAMPLES} finite samples with a non-zero time span)"
    );
    if samples.is_empty() {
        push_unavailable_process_violation(
            expected,
            "unavailable: no process RSS samples".to_owned(),
            violations,
        );
        return;
    }
    if let Some((index, sample)) = samples
        .iter()
        .enumerate()
        .find(|(_, sample)| !sample.at_secs.is_finite() || !sample.rss_mb.is_finite())
    {
        let fields = if !sample.at_secs.is_finite() && !sample.rss_mb.is_finite() {
            "timestamp and RSS"
        } else if !sample.at_secs.is_finite() {
            "timestamp"
        } else {
            "RSS"
        };
        push_unavailable_process_violation(
            expected,
            format!("unavailable: sample {index} has non-finite {fields}"),
            violations,
        );
        return;
    }
    if samples.len() < MIN_RSS_SLOPE_SAMPLES {
        push_unavailable_process_violation(
            expected,
            format!(
                "unavailable: {} process RSS sample(s); need at least \
                 {MIN_RSS_SLOPE_SAMPLES}",
                samples.len()
            ),
            violations,
        );
        return;
    }
    let series: Vec<(f64, f64)> = samples.iter().map(|s| (s.at_secs, s.rss_mb)).collect();
    // `detect_leak` returns None when every timestamp coincides (zero time
    // span) — a slope can't be fitted to a vertical line.
    let Some(slope) = crate::scenario::soak::detect_leak(&series) else {
        push_unavailable_process_violation(
            expected,
            "unavailable: process RSS samples have no usable time span".to_owned(),
            violations,
        );
        return;
    };
    if !slope.is_finite() {
        push_unavailable_process_violation(
            expected,
            format!("unavailable: fitted RSS slope is non-finite ({slope})"),
            violations,
        );
        return;
    }
    if slope > max_slope {
        violations.push(ThresholdViolation {
            kind: ThresholdKind::MemoryGrowthMb,
            expected,
            actual: format!("{slope:.4} MB/s"),
        });
    }
}

/// Record that a configured process threshold could not be evaluated.
///
/// Unavailable evidence is itself a typed threshold violation: silently
/// omitting the check would turn remote/no-PID runs, short runs, and invalid
/// sampler output into false-green reports.
fn push_unavailable_process_violation(
    expected: String,
    actual: String,
    violations: &mut Vec<ThresholdViolation>,
) {
    tracing::warn!(
        expected = %expected,
        actual = %actual,
        "configured process threshold could not be evaluated; failing closed"
    );
    violations.push(ThresholdViolation {
        kind: ThresholdKind::MemoryGrowthMb,
        expected,
        actual,
    });
}

fn check_latency(
    kind: ThresholdKind,
    budget: Duration,
    actual: Duration,
    sample_count: u64,
    violations: &mut Vec<ThresholdViolation>,
) {
    if sample_count == 0 {
        violations.push(ThresholdViolation {
            kind,
            expected: format!("<= {}", format_duration_ms(budget)),
            actual: "unavailable: no recorder latency samples".to_owned(),
        });
    } else if actual > budget {
        violations.push(ThresholdViolation {
            kind,
            expected: format!("<= {}", format_duration_ms(budget)),
            actual: format_duration_ms(actual),
        });
    }
}

/// Evaluate `config.thresholds.tool_slos` against the per-tool metrics
/// snapshot. Returns one [`ThresholdViolation`] for each tool whose p99
/// exceeded the configured budget or whose required latency evidence is
/// unavailable. Coverage is informative, but it is not itself a pass gate;
/// silently skipping an unexercised configured SLO would be a false green.
pub(super) fn evaluate_tool_slos(
    config: &Config,
    per_tool: &std::collections::BTreeMap<String, mcp_loadtest_core::metrics::ScenarioMetrics>,
) -> Vec<ThresholdViolation> {
    let mut violations = Vec::new();
    for slo in &config.thresholds.tool_slos {
        let expected = format!(
            "<= {} for tool `{}`",
            format_duration_ms(slo.p99_latency),
            slo.tool
        );
        let Some(metrics) = per_tool.get(&slo.tool) else {
            violations.push(ThresholdViolation {
                kind: ThresholdKind::P99Latency,
                expected,
                actual: format!(
                    "unavailable: no recorder metrics for configured tool `{}`",
                    slo.tool
                ),
            });
            continue;
        };
        if metrics.latency.count == 0 {
            violations.push(ThresholdViolation {
                kind: ThresholdKind::P99Latency,
                expected,
                actual: format!(
                    "unavailable: no latency samples for configured tool `{}`",
                    slo.tool
                ),
            });
            continue;
        }
        if metrics.latency.p99 > slo.p99_latency {
            violations.push(ThresholdViolation {
                kind: ThresholdKind::P99Latency,
                expected,
                actual: format!(
                    "{} for tool `{}`",
                    format_duration_ms(metrics.latency.p99),
                    slo.tool
                ),
            });
        }
    }
    violations
}

/// Format a Duration as a millisecond-precision string (e.g. `"234.500ms"`).
/// Self-contained so we don't take a hard dep on `humantime`'s formatter.
fn format_duration_ms(d: Duration) -> String {
    let total_ms = d.as_secs_f64() * 1000.0;
    format!("{total_ms:.3}ms")
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::scenario::ScenarioOutcome;
    use mcp_loadtest_core::config::{ScenarioConfig, ServerConfig, ThresholdsConfig, ToolSlo};
    use mcp_loadtest_core::metrics::{
        LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats,
    };
    use mcp_loadtest_core::report::{ProcessStats, ServerInfo};

    fn empty_metrics() -> ScenarioMetrics {
        ScenarioMetrics {
            latency: LatencyStats::default(),
            throughput: ThroughputStats::default(),
            outcomes: OutcomeCounts::default(),
        }
    }

    fn make_report(metrics: ScenarioMetrics, process: ProcessStats) -> Report {
        Report {
            run_id: "01TEST".to_string(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_secs(1),
            scenario_name: "sustained".to_string(),
            server_info: ServerInfo {
                command: "python".to_string(),
                args: vec!["-m".to_string(), "x".to_string()],
                pid: None,
                protocol_version: Some("2025-03-26".to_string()),
            },
            metrics,
            process,
            scenario_outcome: ScenarioOutcome::default(),
            trace_path: None,
            threshold_violations: Vec::new(),
            coverage: None,
        }
    }

    fn make_config(thresholds: ThresholdsConfig) -> Config {
        // `Config`/`ServerConfig`/`ScenarioConfig` are `#[non_exhaustive]` and
        // now live in `mcp-loadtest-core` — build via the constructors +
        // builders rather than an exhaustive struct literal (that syntax is
        // rejected across the crate boundary).
        Config::new(
            ServerConfig::stdio("python".to_string(), vec![]),
            ScenarioConfig::new("sustained", serde_json::json!({})),
        )
        .with_thresholds(thresholds)
    }

    /// `ThresholdsConfig` is `#[non_exhaustive]` and now lives in
    /// `mcp-loadtest-core`, so cross-crate struct-literal syntax (even with
    /// `..Default::default()`) is rejected; mutate a default instance
    /// instead.
    fn thresholds_with(f: impl FnOnce(&mut ThresholdsConfig)) -> ThresholdsConfig {
        let mut t = ThresholdsConfig::default();
        f(&mut t);
        t
    }

    #[test]
    fn evaluate_thresholds_no_constraints_returns_empty() {
        let cfg = make_config(ThresholdsConfig::default());
        let report = make_report(empty_metrics(), ProcessStats::default());
        let v = evaluate_thresholds(&cfg, &report);
        assert!(v.is_empty());
    }

    #[test]
    fn evaluate_thresholds_p99_violation_reported() {
        let cfg = make_config(thresholds_with(|t| {
            t.p99_latency = Some(Duration::from_millis(100));
        }));

        let metrics = ScenarioMetrics {
            latency: LatencyStats {
                p99: Duration::from_millis(500),
                count: 1,
                ..Default::default()
            },
            ..empty_metrics()
        };
        let report = make_report(metrics, ProcessStats::default());

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ThresholdKind::P99Latency);
    }

    #[test]
    fn evaluate_thresholds_p99_within_budget_no_violation() {
        let cfg = make_config(thresholds_with(|t| {
            t.p99_latency = Some(Duration::from_millis(500));
        }));

        let metrics = ScenarioMetrics {
            latency: LatencyStats {
                p99: Duration::from_millis(100),
                count: 1,
                ..Default::default()
            },
            ..empty_metrics()
        };
        let report = make_report(metrics, ProcessStats::default());

        let v = evaluate_thresholds(&cfg, &report);
        assert!(v.is_empty());
    }

    #[test]
    fn evaluate_thresholds_error_rate_violation() {
        let cfg = make_config(thresholds_with(|t| {
            t.error_rate = Some(0.05);
        }));

        let metrics = ScenarioMetrics {
            throughput: ThroughputStats {
                total_requests: 100,
                successful_requests: 80, // 20% error rate
                ..Default::default()
            },
            ..empty_metrics()
        };
        let report = make_report(metrics, ProcessStats::default());

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ThresholdKind::ErrorRate);
    }

    #[test]
    fn evaluate_thresholds_memory_growth_violation() {
        let cfg = make_config(thresholds_with(|t| {
            t.memory_growth_mb = Some(50.0);
        }));

        let mut process = process_with_rss_samples(&[(0.5, 20.0), (1.0, 100.0)]);
        process.baseline_rss_mb = 20.0;
        process.peak_rss_mb = 100.0;
        process.final_rss_mb = 100.0;
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ThresholdKind::MemoryGrowthMb);
        assert_eq!(v[0].actual, "80.00 MB");
    }

    #[test]
    fn evaluate_thresholds_memory_growth_monotonic_leak_trips() {
        // Regression for the (peak - final) bug: a monotonic leak ends at
        // its peak (peak == final), so the old formula computed ~0 growth
        // and silently PASSED. (peak - baseline) must catch it.
        let cfg = make_config(thresholds_with(|t| {
            t.memory_growth_mb = Some(50.0);
        }));
        let mut process = process_with_rss_samples(&[(0.5, 20.0), (1.0, 70.0), (1.5, 120.0)]);
        process.baseline_rss_mb = 20.0;
        process.peak_rss_mb = 120.0;
        process.final_rss_mb = 120.0; // leaked monotonically — final == peak
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(
            v.len(),
            1,
            "monotonic leak of +100MB over baseline must trip (peak==final hid it before)"
        );
        assert_eq!(v[0].kind, ThresholdKind::MemoryGrowthMb);
    }

    #[test]
    fn evaluate_thresholds_memory_growth_steady_state_passes() {
        // A steady-state high-RSS process (peak ≈ baseline) must NOT
        // false-positive: growth is measured above the baseline, not the
        // absolute RSS.
        let cfg = make_config(thresholds_with(|t| {
            t.memory_growth_mb = Some(50.0);
        }));
        let mut process = process_with_rss_samples(&[(0.5, 200.0), (1.0, 205.0), (1.5, 201.0)]);
        process.baseline_rss_mb = 200.0;
        process.peak_rss_mb = 205.0;
        process.final_rss_mb = 201.0;
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert!(
            v.is_empty(),
            "5MB growth on a 200MB baseline must not trip a 50MB threshold"
        );
    }

    #[test]
    fn configured_process_thresholds_fail_closed_without_samples() {
        let cfg = make_config(thresholds_with(|t| {
            t.memory_growth_mb = Some(50.0);
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let mut report = make_report(empty_metrics(), ProcessStats::default());
        report.scenario_outcome.total_calls = 1;
        report.scenario_outcome.successful_calls = 1;

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(
            v.len(),
            2,
            "each configured process threshold must fail closed independently: {v:?}"
        );
        assert!(v.iter().all(|x| x.kind == ThresholdKind::MemoryGrowthMb));
        assert!(
            v.iter().any(|x| x.expected.contains("peak - baseline")),
            "absolute-growth violation must remain identifiable: {v:?}"
        );
        assert!(
            v.iter()
                .any(|x| x.expected.contains("least-squares RSS slope")),
            "slope violation must remain identifiable: {v:?}"
        );
        assert!(
            v.iter()
                .all(|x| x.actual == "unavailable: no process RSS samples"),
            "missing evidence must be explicit: {v:?}"
        );
        report.threshold_violations = v;
        assert!(
            !report.passed(),
            "typed unavailable-evidence violations must gate the report"
        );
    }

    #[test]
    fn memory_growth_non_finite_evidence_fails_closed() {
        let cfg = make_config(thresholds_with(|t| {
            t.memory_growth_mb = Some(50.0);
        }));
        let mut process = process_with_rss_samples(&[(0.5, 20.0), (1.0, 30.0)]);
        process.baseline_rss_mb = f64::NAN;
        process.peak_rss_mb = 30.0;
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1, "non-finite aggregate must fail closed: {v:?}");
        assert_eq!(v[0].kind, ThresholdKind::MemoryGrowthMb);
        assert!(
            v[0].actual.contains("non-finite RSS evidence"),
            "actual must explain why measurement is unavailable: {v:?}"
        );
    }

    #[test]
    fn configured_error_rate_without_request_evidence_fails_closed() {
        let cfg = make_config(thresholds_with(|t| {
            t.error_rate = Some(0.01);
        }));

        let report = make_report(empty_metrics(), ProcessStats::default());
        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1, "missing request evidence must gate: {v:?}");
        assert_eq!(v[0].kind, ThresholdKind::ErrorRate);
        assert!(v[0].actual.contains("no recorder request outcomes"));
    }

    #[test]
    fn configured_latency_budgets_without_samples_each_fail_closed() {
        let cfg = make_config(thresholds_with(|t| {
            t.p50_latency = Some(Duration::from_millis(10));
            t.p95_latency = Some(Duration::from_millis(20));
            t.p99_latency = Some(Duration::from_millis(30));
            t.p999_latency = Some(Duration::from_millis(40));
        }));
        let report = make_report(empty_metrics(), ProcessStats::default());

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 4, "every configured percentile must gate: {v:?}");
        assert!(
            v.iter()
                .all(|violation| violation.actual.contains("no recorder latency samples")),
            "missing-evidence diagnostics must be explicit: {v:?}"
        );
    }

    #[test]
    fn configured_tool_slo_requires_tool_metrics_and_latency_samples() {
        let cfg = make_config(thresholds_with(|t| {
            t.tool_slos.push(ToolSlo {
                tool: "required_tool".to_owned(),
                p99_latency: Duration::from_millis(50),
            });
        }));

        let missing = evaluate_tool_slos(&cfg, &std::collections::BTreeMap::new());
        assert_eq!(missing.len(), 1);
        assert!(missing[0].actual.contains("no recorder metrics"));

        let mut zero_sample = std::collections::BTreeMap::new();
        zero_sample.insert("required_tool".to_owned(), empty_metrics());
        let zero_sample = evaluate_tool_slos(&cfg, &zero_sample);
        assert_eq!(zero_sample.len(), 1);
        assert!(zero_sample[0].actual.contains("no latency samples"));
    }

    /// Build a `ProcessStats` whose `samples` carry the given
    /// `(at_secs, rss_mb)` points (cpu/fd/threads zeroed — irrelevant to
    /// the slope check).
    fn process_with_rss_samples(points: &[(f64, f64)]) -> ProcessStats {
        ProcessStats {
            samples: points
                .iter()
                .map(|&(at_secs, rss_mb)| ProcessSample {
                    at_secs,
                    rss_mb,
                    cpu_pct: 0.0,
                    fd: 0,
                    threads: 0,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn rss_leak_monotonic_leak_trips_threshold() {
        // 1.0 MB/s leak against a 0.5 MB/s budget → violation.
        let cfg = make_config(thresholds_with(|t| {
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let process =
            process_with_rss_samples(&[(0.5, 100.0), (1.5, 101.0), (2.5, 102.0), (3.5, 103.0)]);
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1, "1.0 MB/s slope must trip a 0.5 MB/s budget");
        assert_eq!(v[0].kind, ThresholdKind::MemoryGrowthMb);
        assert!(
            v[0].expected.contains("least-squares RSS slope"),
            "expected string must disambiguate the slope check from the \
             absolute-growth check: {:?}",
            v[0].expected
        );
        assert!(
            v[0].actual.contains("MB/s"),
            "actual must carry MB/s units: {:?}",
            v[0].actual
        );
    }

    #[test]
    fn rss_leak_steady_state_passes() {
        // Flat RSS (slope ≈ 0) stays under any positive budget — a
        // steady-state high-RSS process must not false-positive.
        let cfg = make_config(thresholds_with(|t| {
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let process =
            process_with_rss_samples(&[(0.5, 200.0), (1.5, 200.0), (2.5, 200.0), (3.5, 200.0)]);
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert!(
            v.is_empty(),
            "flat RSS must not trip the slope check: {v:?}"
        );
    }

    #[test]
    fn rss_leak_two_samples_fail_closed() {
        // Two points always fit a line exactly — even a wild apparent
        // slope is insufficient evidence below MIN_RSS_SLOPE_SAMPLES.
        let cfg = make_config(thresholds_with(|t| {
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let process = process_with_rss_samples(&[(0.5, 100.0), (1.5, 200.0)]); // "100 MB/s"
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(
            v.len(),
            1,
            "a configured gate must fail closed on a 2-sample series: {v:?}"
        );
        assert!(
            v[0].actual
                .contains("2 process RSS sample(s); need at least 3"),
            "actual must explain the insufficient evidence: {v:?}"
        );
    }

    #[test]
    fn rss_leak_threshold_none_never_evaluates() {
        // No rss_leak_mb_per_sec configured → leaking samples are ignored.
        let cfg = make_config(ThresholdsConfig::default());
        let process =
            process_with_rss_samples(&[(0.5, 100.0), (1.5, 200.0), (2.5, 300.0), (3.5, 400.0)]);
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert!(v.is_empty(), "unset threshold must never evaluate: {v:?}");
    }

    #[test]
    fn rss_leak_zero_time_span_fails_closed() {
        // Three samples with identical timestamps — detect_leak returns
        // None (degenerate t); the configured gate must fail closed.
        let cfg = make_config(thresholds_with(|t| {
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let process = process_with_rss_samples(&[(1.0, 100.0), (1.0, 200.0), (1.0, 300.0)]);
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1, "zero time span must fail closed: {v:?}");
        assert!(
            v[0].actual.contains("no usable time span"),
            "actual must explain the degenerate series: {v:?}"
        );
    }

    #[test]
    fn rss_leak_non_finite_sample_fails_closed() {
        // One NaN sample poisons the fit (slope would be NaN, silently
        // passing `>`); the configured gate must fail closed instead.
        let cfg = make_config(thresholds_with(|t| {
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let process =
            process_with_rss_samples(&[(0.5, 100.0), (1.5, f64::NAN), (2.5, 300.0), (3.5, 400.0)]);
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1, "non-finite RSS must fail closed: {v:?}");
        assert!(
            v[0].actual.contains("sample 1 has non-finite RSS"),
            "actual must identify the invalid sample: {v:?}"
        );
    }

    #[test]
    fn rss_leak_non_finite_fit_fails_closed() {
        // Every input is individually finite, but extreme values overflow
        // the regression arithmetic. The resulting non-finite fit must not
        // fall through the `slope > max_slope` comparison as a false pass.
        let cfg = make_config(thresholds_with(|t| {
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let process =
            process_with_rss_samples(&[(0.0, f64::MAX), (1.0, f64::MAX / 2.0), (2.0, f64::MAX)]);
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 1, "non-finite fit must fail closed: {v:?}");
        assert!(
            v[0].actual.contains("fitted RSS slope is non-finite"),
            "actual must identify regression overflow: {v:?}"
        );
    }

    #[test]
    fn rss_leak_and_memory_growth_can_both_trip() {
        // The two RSS checks are independent: a fast monotonic leak trips
        // both the absolute-growth bar and the slope budget, yielding two
        // distinguishable violations.
        let cfg = make_config(thresholds_with(|t| {
            t.memory_growth_mb = Some(50.0);
            t.rss_leak_mb_per_sec = Some(0.5);
        }));
        let mut process =
            process_with_rss_samples(&[(0.5, 20.0), (30.5, 80.0), (60.5, 140.0), (90.5, 200.0)]);
        process.baseline_rss_mb = 20.0;
        process.peak_rss_mb = 200.0;
        process.final_rss_mb = 200.0;
        let report = make_report(empty_metrics(), process);

        let v = evaluate_thresholds(&cfg, &report);
        assert_eq!(v.len(), 2, "expected growth + slope violations: {v:?}");
        assert!(v.iter().all(|x| x.kind == ThresholdKind::MemoryGrowthMb));
        assert!(v.iter().any(|x| x.expected.contains("peak - baseline")));
        assert!(
            v.iter()
                .any(|x| x.expected.contains("least-squares RSS slope"))
        );
    }
}
