//! Prometheus text-format reporter tests.

use std::time::{Duration, SystemTime};

use mcp_loadtest_core::coverage::CoverageReport;
use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{ProcessStats, Report, Reporter, ServerInfo};
use mcp_loadtest_output::report::prometheus::PrometheusReporter;

const SECRET: &str = "PROMETHEUS_SECRET_SENTINEL";

fn sample_report() -> Report {
    Report {
        run_id: "01PROM00000000000000000000".to_owned(),
        started_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_secs(2),
        scenario_name: "sustained\"load\n".to_owned(),
        server_info: ServerInfo {
            command: SECRET.to_owned(),
            args: vec![SECRET.to_owned()],
            pid: None,
            protocol_version: Some("2025-11-25".to_owned()),
        },
        metrics: ScenarioMetrics {
            latency: LatencyStats {
                p50: Duration::from_millis(10),
                p95: Duration::from_millis(20),
                p99: Duration::from_millis(30),
                p999: Duration::from_millis(40),
                mean: Duration::from_millis(15),
                min: Duration::from_millis(1),
                max: Duration::from_millis(50),
                count: 4,
            },
            throughput: ThroughputStats {
                total_requests: 5,
                successful_requests: 4,
                requests_per_sec: 2.5,
            },
            outcomes: OutcomeCounts {
                success: 4,
                server_error: 1,
                ..OutcomeCounts::default()
            },
        },
        process: ProcessStats {
            baseline_rss_mb: 10.0,
            peak_rss_mb: 12.0,
            final_rss_mb: 11.0,
            avg_cpu_pct: 5.0,
            peak_fd: 8,
            final_fd: 6,
            peak_threads: 4,
            final_threads: 3,
            samples: Vec::new(),
        },
        scenario_outcome: ScenarioOutcome {
            total_calls: 5,
            successful_calls: 4,
            error_count: 1,
            ..ScenarioOutcome::default()
        },
        trace_path: None,
        threshold_violations: Vec::new(),
        coverage: Some(CoverageReport::build(
            vec!["echo".to_owned(), "other".to_owned()],
            [("echo".to_owned(), 4)].into_iter().collect(),
        )),
    }
}

#[test]
fn exposition_is_deterministic_complete_and_secret_free() {
    let text = PrometheusReporter
        .render(&sample_report())
        .expect("render prometheus");
    assert!(text.ends_with('\n'));
    assert!(text.contains("# TYPE mcp_loadtest_call_latency_seconds summary\n"));
    assert!(text.contains("mcp_loadtest_call_latency_seconds{quantile=\"0.99\"} 0.03\n"));
    assert!(text.contains("mcp_loadtest_call_latency_seconds_sum 0.06\n"));
    assert!(text.contains("mcp_loadtest_call_latency_seconds_count 4\n"));
    assert!(text.contains("mcp_loadtest_requests_total{outcome=\"server_error\"} 1\n"));
    assert!(text.contains("mcp_loadtest_tool_coverage_ratio 0.5\n"));
    assert!(text.contains("scenario=\"sustained\\\"load\\n\""));
    assert!(!text.contains(SECRET));
    assert!(
        !text.contains("01PROM"),
        "run-id labels are high-cardinality"
    );
}

#[test]
fn every_sample_has_metadata_before_its_first_occurrence() {
    let text = PrometheusReporter
        .render(&sample_report())
        .expect("render prometheus");
    for name in [
        "mcp_loadtest_info",
        "mcp_loadtest_run_passed",
        "mcp_loadtest_run_duration_seconds",
        "mcp_loadtest_requests_total",
        "mcp_loadtest_requests_per_second",
        "mcp_loadtest_call_latency_seconds",
        "mcp_loadtest_process_resident_memory_bytes",
        "mcp_loadtest_process_cpu_percent",
        "mcp_loadtest_process_open_file_descriptors",
        "mcp_loadtest_process_threads",
        "mcp_loadtest_correctness_events_total",
        "mcp_loadtest_threshold_violations",
        "mcp_loadtest_tool_coverage_ratio",
    ] {
        let type_position = text
            .lines()
            .position(|line| line.starts_with(&format!("# TYPE {name} ")))
            .unwrap_or_else(|| panic!("missing TYPE for {name}"));
        let sample_position = text
            .lines()
            .position(|line| {
                line.starts_with(&format!("{name}{{")) || line.starts_with(&format!("{name} "))
            })
            .unwrap_or_else(|| panic!("missing sample for {name}"));
        assert!(type_position < sample_position, "{name}");
    }
}

#[test]
fn non_finite_metrics_fail_closed() {
    let mut report = sample_report();
    report.process.avg_cpu_pct = f64::NAN;
    let error = PrometheusReporter
        .render(&report)
        .expect_err("NaN must not enter exposition");
    assert!(error.to_string().contains("non-finite"));
    assert!(!error.to_string().contains(SECRET));
}
