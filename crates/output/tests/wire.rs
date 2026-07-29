//! Canonical metrics-document serialization tests.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{
    ProcessStats, Report, ServerInfo, ThresholdKind, ThresholdViolation,
};
use mcp_loadtest_output::report::wire::{MetricsDocumentV1, render_pretty_json};

fn sample_report() -> Report {
    Report {
        run_id: "01WIRE00000000000000000000".to_owned(),
        started_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_millis(1_500),
        scenario_name: "sustained".to_owned(),
        server_info: ServerInfo {
            command: "python".to_owned(),
            args: vec!["-m".to_owned(), "fixture".to_owned()],
            pid: Some(42),
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
                count: 10,
            },
            throughput: ThroughputStats {
                total_requests: 10,
                successful_requests: 9,
                requests_per_sec: 6.5,
            },
            outcomes: OutcomeCounts {
                success: 9,
                server_error: 1,
                ..OutcomeCounts::default()
            },
        },
        process: ProcessStats {
            peak_rss_mb: 20.0,
            final_rss_mb: 18.0,
            avg_cpu_pct: 4.5,
            ..ProcessStats::default()
        },
        scenario_outcome: ScenarioOutcome {
            total_calls: 10,
            successful_calls: 9,
            error_count: 1,
            ..ScenarioOutcome::default()
        },
        trace_path: Some(PathBuf::from("trace.jsonl")),
        threshold_violations: vec![ThresholdViolation {
            kind: ThresholdKind::ErrorRate,
            expected: "<= 0.01".to_owned(),
            actual: "0.10".to_owned(),
        }],
        coverage: None,
    }
}

#[test]
fn wire_document_preserves_v1_field_names_and_units() {
    let report = sample_report();
    let document = MetricsDocumentV1::from(&report);
    assert_eq!(document.started_at, "1970-01-01T00:00:00Z");
    assert_eq!(document.duration_secs, 1.5);
    assert_eq!(document.latency_ms.p99, 30.0);
    assert_eq!(document.errors.total, 1);
    assert_eq!(document.errors.by_category.server_error, 1);
    assert_eq!(document.threshold_violations[0].metric, "error_rate");
    assert!(!document.passed);

    let json = render_pretty_json(&report).expect("render v1 metrics");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse rendered metrics");
    assert_eq!(value["errors"]["by_category"]["ServerError"], 1);
    assert_eq!(value["latency_ms"]["p99"], 30.0);
    assert!(value.get("divergence_count").is_none());
    assert!(value.get("incomplete_worker_count").is_none());
    assert!(value.get("teardown_failure_count").is_none());
}

#[test]
fn wire_document_round_trips_owned_json() {
    let document = MetricsDocumentV1::from(&sample_report());
    let encoded = document.to_pretty_json().expect("serialize");
    let decoded = MetricsDocumentV1::from_json_str(&encoded).expect("deserialize");
    assert_eq!(decoded, document);
}
