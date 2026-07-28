//! Integration tests for the M6 `RaceCheck` scenario + `race_detector`.
//!
//! 1. `race_check_against_mock_normal_no_divergence` — `mock-normal.py` echoes
//!    args deterministically, so identical calls must canonicalize to a
//!    single group and `outcome.notes` must stay clean of any divergence
//!    annotation.
//! 2. `race_check_records_metrics` — verify the scenario feeds the recorder.

mod helpers;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::race_detector::analyze;
use mcp_loadtest_engine::scenario::race_check::RaceCheck;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::transport::TransportError;
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use mcp_loadtest_protocol::{Session, SessionError, SessionFactory};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const TEST_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
}

fn fixture_factory(fixture: &str) -> SessionFactory {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    SessionFactory::new(move || {
        let mock = mock.clone();
        let py = py.clone();
        async move {
            Session::spawn_with_timeout(
                &py,
                [mock.as_os_str()],
                SpawnOptions::inherit(),
                TEST_STARTUP_TIMEOUT,
            )
            .await
        }
    })
}

fn partially_failing_factory(fixture: &str) -> SessionFactory {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    let attempts = Arc::new(AtomicU32::new(0));
    SessionFactory::new(move || {
        let mock = mock.clone();
        let py = py.clone();
        let attempts = Arc::clone(&attempts);
        async move {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(SessionError::Transport(TransportError::Other(
                    "injected worker setup failure".into(),
                )));
            }
            Session::spawn_with_timeout(
                &py,
                [mock.as_os_str()],
                SpawnOptions::inherit(),
                TEST_STARTUP_TIMEOUT,
            )
            .await
        }
    })
}

async fn spawn_fixture(fixture: &str) -> Session {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    Session::spawn_with_timeout(
        &py,
        [mock.as_os_str()],
        SpawnOptions::inherit(),
        TEST_STARTUP_TIMEOUT,
    )
    .await
    .expect("spawn failed")
}

#[tokio::test]
async fn race_check_against_mock_normal_no_divergence() {
    let mut session = spawn_fixture("mock-normal.py").await;

    let scenario = RaceCheck {
        concurrent: 4,
        tool: "echo".to_string(),
        args: json!({ "ticker": "AAPL", "n": 42 }),
    };
    assert_eq!(scenario.name(), "race_check");
    let _schema = scenario.config_schema();

    let ctx = make_ctx().with_session_factory(fixture_factory("mock-normal.py"));
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(outcome.total_calls, 4, "expected 4 calls; got {outcome:?}");
    assert_eq!(
        outcome.successful_calls, 4,
        "mock-normal should never error; got {outcome:?}"
    );
    assert_eq!(outcome.error_count, 0);

    // mock-normal echoes args verbatim, so all responses must canonicalize
    // identically — no divergence note should appear.
    assert!(
        !outcome.notes.iter().any(|n| n.contains("divergence")),
        "no divergence expected against mock-normal; got notes={:?}",
        outcome.notes
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn race_check_records_metrics_into_recorder() {
    let mut session = spawn_fixture("mock-normal.py").await;

    let scenario = RaceCheck {
        concurrent: 3,
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx().with_session_factory(fixture_factory("mock-normal.py"));
    let recorder = ctx.metrics.clone();
    let _outcome = scenario.drive(&mut session, &ctx).await;

    let snap = recorder.snapshot();
    assert_eq!(
        snap.outcomes.success, 3,
        "expected 3 successes in recorder; got {snap:?}"
    );
    assert_eq!(snap.throughput.total_requests, 3);

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn race_check_divergence_is_typed_and_ci_gateable() {
    let mut session = spawn_fixture("mock-process-id.py").await;
    let scenario = RaceCheck {
        concurrent: 3,
        tool: "process_id".to_string(),
        args: json!({}),
    };
    let ctx = make_ctx().with_session_factory(fixture_factory("mock-process-id.py"));
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(outcome.total_calls, 3, "got {outcome:?}");
    assert_eq!(outcome.successful_calls, 3, "got {outcome:?}");
    assert_eq!(outcome.divergence_count, 1, "got {outcome:?}");
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("divergence detected")),
        "got {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("pool: 3 workers (3 requested)")),
        "race check must use the synchronized pool: {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn race_check_incomplete_cohort_is_inconclusive() {
    let mut session = spawn_fixture("mock-normal.py").await;
    let scenario = RaceCheck {
        concurrent: 3,
        tool: "echo".to_string(),
        args: json!({"same": true}),
    };
    let ctx = make_ctx().with_session_factory(partially_failing_factory("mock-normal.py"));
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(outcome.total_calls, 2, "got {outcome:?}");
    assert_eq!(outcome.successful_calls, 2, "got {outcome:?}");
    assert_eq!(outcome.error_count, 1, "got {outcome:?}");
    assert_eq!(
        outcome.divergence_count, 0,
        "partial results must not be analyzed as divergence: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("inconclusive") && note.contains("2/3")),
        "got {outcome:?}"
    );

    session.shutdown().await.expect("shutdown errored");
}

#[tokio::test]
async fn race_check_rejects_sequential_fallback() {
    let mut session = spawn_fixture("mock-normal.py").await;
    let scenario = RaceCheck {
        concurrent: 2,
        tool: "echo".to_string(),
        args: json!({}),
    };

    let outcome = scenario.drive(&mut session, &make_ctx()).await;
    assert_eq!(outcome.total_calls, 0);
    assert_eq!(outcome.error_count, 1);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("session_factory")),
        "got {outcome:?}"
    );

    session.shutdown().await.expect("shutdown errored");
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
