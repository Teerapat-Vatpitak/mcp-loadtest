//! JUnit reporter compatibility and redaction tests.

use std::time::{Duration, SystemTime};

use mcp_loadtest_core::metrics::{LatencyStats, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{
    ProcessStats, Report, Reporter, ServerInfo, ThresholdKind, ThresholdViolation,
};
use mcp_loadtest_output::report::junit::JunitReporter;

const SECRET: &str = "JUNIT_SECRET_SENTINEL";

fn report(passed: bool) -> Report {
    let total_calls = if passed { 1 } else { 2 };
    let successful_calls = 1;
    Report {
        run_id: "01JUNIT0000000000000000000".to_owned(),
        started_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_millis(250),
        scenario_name: "sustained<&\u{0}".to_owned(),
        server_info: ServerInfo {
            command: SECRET.to_owned(),
            args: vec![SECRET.to_owned()],
            pid: None,
            protocol_version: None,
        },
        metrics: ScenarioMetrics {
            latency: LatencyStats {
                p99: Duration::from_millis(25),
                count: total_calls,
                ..LatencyStats::default()
            },
            throughput: ThroughputStats {
                total_requests: total_calls,
                successful_requests: successful_calls,
                requests_per_sec: 4.0,
            },
            ..ScenarioMetrics::default()
        },
        process: ProcessStats::default(),
        scenario_outcome: ScenarioOutcome {
            total_calls,
            successful_calls,
            notes: vec![SECRET.to_owned()],
            ..ScenarioOutcome::default()
        },
        trace_path: None,
        threshold_violations: if passed {
            Vec::new()
        } else {
            vec![ThresholdViolation {
                kind: ThresholdKind::P99Latency,
                expected: "<= 10ms".to_owned(),
                actual: "25ms".to_owned(),
            }]
        },
        coverage: None,
    }
}

#[test]
fn passing_run_is_one_passing_testcase() {
    let xml = JunitReporter
        .render(&report(true))
        .expect("render passing junit");
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(xml.contains("tests=\"1\" failures=\"0\" errors=\"0\""));
    assert!(!xml.contains("<failure"));
    assert!(xml.ends_with("</testsuites>\n"));
}

#[test]
fn failure_is_typed_escaped_and_secret_free() {
    let xml = JunitReporter
        .render(&report(false))
        .expect("render failing junit");
    assert!(xml.contains("failures=\"1\""));
    assert!(xml.contains("<failure type=\"mcp-loadtest.correctness\""));
    assert!(xml.contains("threshold p99_latency"));
    assert!(xml.contains("sustained&lt;&amp;�"));
    assert!(!xml.contains('\u{0}'));
    assert!(
        !xml.contains(SECRET),
        "server identity or free-form notes leaked into JUnit XML"
    );
}
