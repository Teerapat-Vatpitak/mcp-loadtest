//! Integration tests for the M6 `RaceCheck` scenario + `race_detector`.
//!
//! 1. `race_check_against_mock_normal_no_divergence` — `mock-normal.py` echoes
//!    args deterministically, so 10 identical calls must canonicalize to a
//!    single group and `outcome.notes` must stay clean of any divergence
//!    annotation.
//! 2. `race_check_records_metrics` — verify the scenario feeds the recorder.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::race_detector::analyze;
use mcp_loadtest_engine::scenario::race_check::RaceCheck;
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

#[tokio::test]
async fn race_check_against_mock_normal_no_divergence() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = RaceCheck {
        concurrent: 10,
        tool: "echo".to_string(),
        args: json!({ "ticker": "AAPL", "n": 42 }),
    };
    assert_eq!(scenario.name(), "race_check");
    let _schema = scenario.config_schema();

    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(
        outcome.total_calls, 10,
        "expected 10 calls; got {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, 10,
        "mock-normal should never error; got {outcome:?}"
    );
    assert_eq!(outcome.error_count, 0);

    // mock-normal echoes args verbatim, so all 10 responses must canonicalize
    // identically — no divergence note should appear.
    assert!(
        !outcome.notes.iter().any(|n| n.contains("divergence")),
        "no divergence expected against mock-normal; got notes={:?}",
        outcome.notes
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn race_check_records_metrics_into_recorder() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = RaceCheck {
        concurrent: 5,
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx();
    let recorder = ctx.metrics.clone();
    let _outcome = scenario.drive(&mut session, &ctx).await;

    let snap = recorder.snapshot();
    assert_eq!(
        snap.outcomes.success, 5,
        "expected 5 successes in recorder; got {snap:?}"
    );
    assert_eq!(snap.throughput.total_requests, 5);

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[test]
fn analyze_detects_divergence_unit() {
    // Smoke check the detector independently — duplicates the analyze tests
    // inside the lib but kept here so the integration suite exercises the
    // public re-export path too.
    let responses = vec![json!({"answer": 42}), json!({"answer": 43})];
    let report = analyze(&responses);
    assert_eq!(report.total_responses, 2);
    assert_eq!(report.unique_responses, 2);
    assert!(report.diverged);
}

#[test]
fn analyze_groups_identical_unit() {
    let responses: Vec<_> = (0..5).map(|_| json!({"x": 1})).collect();
    let report = analyze(&responses);
    assert_eq!(report.total_responses, 5);
    assert_eq!(report.unique_responses, 1);
    assert!(!report.diverged);
    assert_eq!(report.samples[0].0, 5);
}

#[test]
fn analyze_canonicalizes_key_order_unit() {
    // Same shape, different key insertion order — must canonicalize to one
    // group, no divergence flag.
    let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
    let b: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
    let report = analyze(&[a, b]);
    assert_eq!(report.unique_responses, 1);
    assert!(!report.diverged);
}
