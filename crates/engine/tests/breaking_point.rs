//! Integration tests for the M5 `Ramp` scenario + `BreakingPointDetector`.
//!
//! 1. `breaking_point_detector_finds_p99_violation` — pure unit-style test:
//!    feed three [`ScenarioMetrics`] samples with rising p99 and confirm the
//!    detector reports the third one's concurrency.
//! 2. `ramp_against_mock_normal_completes_full_ramp` — drive [`Ramp`] from 1
//!    to 5 against `mock-normal.py` (no breaking-point config). Should
//!    complete every step with zero errors.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::{
    LatencyStats, OutcomeCounts, Recorder, ScenarioMetrics, ThroughputStats,
};
use mcp_loadtest_engine::breaking_point::{BreakingPointConfig, BreakingPointDetector};
use mcp_loadtest_engine::scenario::ramp::Ramp;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
}

fn synthetic_metrics(p99_ms: u64, total: u64, successful: u64) -> ScenarioMetrics {
    ScenarioMetrics {
        latency: LatencyStats {
            p50: Duration::from_millis(p99_ms / 2),
            p95: Duration::from_millis(p99_ms),
            p99: Duration::from_millis(p99_ms),
            p999: Duration::from_millis(p99_ms),
            mean: Duration::from_millis(p99_ms / 2),
            min: Duration::ZERO,
            max: Duration::from_millis(p99_ms),
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
fn breaking_point_detector_finds_p99_violation() {
    let cfg = BreakingPointConfig {
        max_p99_latency: Duration::from_millis(100),
        max_error_rate: 1.0,
        window_secs: 1.0,
    };
    let mut det = BreakingPointDetector::new(cfg);

    // Concurrency 1 — fast.
    det.observe(1, synthetic_metrics(10, 100, 100));
    // Concurrency 5 — still under the 100ms budget.
    det.observe(5, synthetic_metrics(50, 100, 100));
    // Concurrency 10 — p99 of 200ms exceeds the 100ms budget.
    det.observe(10, synthetic_metrics(200, 100, 100));

    let report = det.breaking_point();
    assert_eq!(
        report.broke_at_concurrent,
        Some(10),
        "expected break at concurrency=10; got {report:?}",
    );
    assert_eq!(report.last_known_good, Some(5));
    assert!(
        report.trigger.as_deref().unwrap_or("").contains("p99"),
        "trigger should mention p99: {:?}",
        report.trigger
    );
    assert_eq!(report.samples.len(), 3);
}

#[tokio::test]
async fn ramp_against_mock_normal_completes_full_ramp() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Ramp {
        from_concurrent: 1,
        to_concurrent: 5,
        // Keep step_duration tight so the test wraps in a few seconds.
        step_duration: Duration::from_millis(150),
        step_increment: 1,
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
        breaking_point: None,
    };

    assert_eq!(scenario.name(), "ramp");
    let _schema = scenario.config_schema();

    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(
        outcome.error_count, 0,
        "mock-normal should never error: {outcome:?}",
    );
    assert!(
        outcome.total_calls >= 5,
        "expected ≥5 total calls (one per step minimum); got {outcome:?}",
    );
    assert_eq!(
        outcome.successful_calls, outcome.total_calls,
        "every call against mock-normal should succeed: {outcome:?}",
    );
    assert_eq!(outcome.deadlock_count, 0);
    assert_eq!(outcome.hang_count, 0);

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
