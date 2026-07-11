//! `Soak` scenario integration tests.
//!
//! Verifies the soak loop runs to its declared duration and emits the
//! expected number of periodic metric snapshots into `ScenarioOutcome.notes`.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::soak::{Soak, detect_leak};
use mcp_loadtest_engine::scenario::{RunContext, Scenario, ScenarioOutcome};
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

#[tokio::test]
async fn soak_runs_full_duration_and_emits_samples() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Soak {
        concurrent: 1,
        duration: Duration::from_secs(2),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
        sample_interval: Duration::from_millis(500),
        latency_drift_ms_per_sec: 5.0,
    };

    assert_eq!(scenario.name(), "soak");
    let _schema = scenario.config_schema();

    let ctx = make_ctx();
    let started = Instant::now();
    let outcome: ScenarioOutcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(1900),
        "soak should run for ~2s; ran for {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "soak should not greatly overshoot; ran for {elapsed:?}"
    );

    assert!(
        outcome.total_calls > 0,
        "expected >0 calls in 2s; got {outcome:?}"
    );
    assert_eq!(outcome.error_count, 0, "mock-normal should never error");

    // 2s window with 500ms interval → expect ~3 in-loop samples plus the
    // closing snapshot, i.e. 4 entries. Be tolerant: scheduling jitter on
    // Windows test runners can shave one off, and a slow first call could
    // delay the first crossing past the boundary.
    let sample_lines: Vec<&String> = outcome
        .notes
        .iter()
        .filter(|n| n.starts_with("soak.sample "))
        .collect();
    assert!(
        sample_lines.len() >= 3,
        "expected ~4 soak.sample notes; got {} — notes: {:?}",
        sample_lines.len(),
        outcome.notes
    );
    assert!(
        sample_lines.len() <= 6,
        "too many samples (clock drift?); got {} — notes: {:?}",
        sample_lines.len(),
        outcome.notes
    );

    // The summary line must be present.
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("soak: ") && n.contains("samples over")),
        "expected a soak summary note; got {:?}",
        outcome.notes
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn soak_observes_cancellation() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Soak {
        concurrent: 1,
        // Long duration; cancellation should cut it short.
        duration: Duration::from_secs(60),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
        sample_interval: Duration::from_millis(200),
        latency_drift_ms_per_sec: 5.0,
    };

    let ctx = make_ctx();
    let cancel = ctx.cancel_token.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });

    let started = Instant::now();
    let outcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "drive should return promptly after cancel; took {elapsed:?}"
    );
    assert!(
        outcome.total_calls > 0,
        "expected at least one call before cancel: {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Linear-regression leak detector — synthetic-sample regression test.
///
/// Drives [`detect_leak`] with three canonical shapes:
///   - empty / singleton → `None`
///   - flat RSS over time → `slope ≈ 0` (no leak)
///   - upward-trending RSS (1 MB/sec) → `slope ≈ 1.0`, below default threshold
///   - sharp 7 MB/sec leak → above the default 5 MB/sec threshold
#[test]
fn detect_leak_synthetic_shapes() {
    // Nothing to fit.
    assert!(detect_leak(&[]).is_none());
    assert!(detect_leak(&[(0.0, 100.0)]).is_none());

    // Steady-state 100 MB across 60s — no leak.
    let flat: Vec<(f64, f64)> = (0..6).map(|i| (i as f64 * 10.0, 100.0)).collect();
    let slope = detect_leak(&flat).expect("flat regression should succeed");
    assert!(
        slope.abs() < 1e-6,
        "steady-state RSS should regress to slope≈0, got {slope}"
    );

    // 1 MB/sec growth — sub-threshold for the 5 MB/sec default.
    let mild: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 100.0 + i as f64)).collect();
    let slope = detect_leak(&mild).expect("mild regression should succeed");
    assert!(
        (slope - 1.0).abs() < 1e-9,
        "1 MB/sec growth should regress to slope≈1.0, got {slope}"
    );
    assert!(
        slope < 5.0,
        "mild slope must stay below the 5 MB/sec default leak threshold"
    );

    // 7 MB/sec — clearly above threshold.
    let leaking: Vec<(f64, f64)> = (0..10)
        .map(|i| (i as f64, 100.0 + i as f64 * 7.0))
        .collect();
    let slope = detect_leak(&leaking).expect("leaking regression should succeed");
    assert!(
        (slope - 7.0).abs() < 1e-9,
        "7 MB/sec leak should regress to slope≈7.0, got {slope}"
    );
    assert!(
        slope > 5.0,
        "leaking slope must trip the 5 MB/sec default threshold"
    );
}
