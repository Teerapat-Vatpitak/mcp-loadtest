//! Basic scenario integration tests.
//!
//! Drives the real `Sustained` scenario against `mock-normal.py` end-to-end:
//! spawn server, build a `RunContext`, call `scenario.drive(...)`, assert on
//! the returned `ScenarioOutcome`.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::cold_start::{ColdStart, HANDSHAKE_METRIC};
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::scenario::{RunContext, Scenario, ScenarioOutcome};
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::SessionFactory;
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
async fn sustained_against_mock_normal() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_secs(2),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    assert_eq!(scenario.name(), "sustained");
    let _schema = scenario.config_schema();

    let ctx = make_ctx();
    let outcome: ScenarioOutcome = scenario.drive(&mut session, &ctx).await;

    assert!(
        outcome.total_calls > 0,
        "expected >0 calls in 2s; got {outcome:?}"
    );
    assert_eq!(
        outcome.error_count, 0,
        "mock-normal should never error; got {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, outcome.total_calls,
        "every call against mock-normal should succeed; got {outcome:?}"
    );
    assert_eq!(outcome.hang_count, 0);
    assert_eq!(outcome.deadlock_count, 0);

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn sustained_observes_cancellation() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Sustained {
        concurrent: 1,
        // Long duration; cancellation should cut it short.
        duration: Duration::from_secs(60),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx();
    let cancel = ctx.cancel_token.clone();

    // Cancel after 250ms so the loop has time to record at least one call.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
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

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Factory that respawns `mock-normal.py` — what `Run::execute` builds from
/// the config, reproduced here for direct-scenario tests.
fn mock_normal_factory() -> SessionFactory {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();
    SessionFactory::new(move || {
        let py = py.clone();
        let mock = mock.clone();
        async move { Session::spawn(&py, [mock.as_os_str()]).await }
    })
}

#[tokio::test]
async fn cold_start_measures_handshake_per_fresh_session() {
    // ColdStart is a real scenario (DESIGN §13.1 item 1) — it respawns a
    // fresh session per iteration via ctx.session_factory and records the
    // spawn→initialize handshake under HANDSHAKE_METRIC.
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    // Orchestrator-style initial session — intentionally unused by ColdStart
    // (it would be a warm measurement).
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = ColdStart {
        iterations: 3,
        warmup: true,
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };
    assert_eq!(scenario.name(), "cold_start");
    let _schema = scenario.config_schema();

    let ctx = make_ctx().with_session_factory(mock_normal_factory());
    let outcome = scenario.drive(&mut session, &ctx).await;

    // Outcome counters are honest: all 3 iterations ran (warmup included).
    assert_eq!(outcome.total_calls, 3, "got {outcome:?}");
    assert_eq!(outcome.successful_calls, 3, "got {outcome:?}");
    assert_eq!(outcome.error_count, 0, "got {outcome:?}");
    assert_eq!(outcome.hang_count, 0, "got {outcome:?}");
    assert_eq!(outcome.deadlock_count, 0, "got {outcome:?}");
    assert!(
        outcome.notes.iter().any(|n| n.contains("warmup")),
        "warmup discard should be noted: {outcome:?}"
    );

    // Metrics exclude the warmup iteration: 2 handshake samples + 2 first
    // calls, each under its own per-tool histogram row.
    let per_tool = ctx.metrics.snapshot_per_tool();
    let handshake = per_tool
        .get(HANDSHAKE_METRIC)
        .expect("handshake metric row missing");
    assert_eq!(
        handshake.throughput.total_requests, 2,
        "warmup handshake must be discarded; per_tool={per_tool:?}"
    );
    assert_eq!(handshake.throughput.successful_requests, 2);
    let echo = per_tool.get("echo").expect("echo metric row missing");
    assert_eq!(
        echo.throughput.total_requests, 2,
        "warmup first-call must be discarded; per_tool={per_tool:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn cold_start_without_factory_is_a_safe_noop() {
    // Direct-library callers building a bare RunContext (no factory) must
    // get an honest note, not a panic — and the provided session must stay
    // untouched/usable.
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = ColdStart {
        iterations: 3,
        warmup: true,
        tool: "echo".to_string(),
        args: json!({}),
    };

    let ctx = make_ctx(); // no session_factory attached
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(outcome.total_calls, 0, "got {outcome:?}");
    assert_eq!(outcome.error_count, 0, "got {outcome:?}");
    assert!(
        outcome.notes.iter().any(|n| n.contains("session_factory")),
        "missing-factory note expected: {outcome:?}"
    );
    assert!(
        ctx.metrics.snapshot_per_tool().is_empty(),
        "no-op must record nothing"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn cold_start_observes_cancellation() {
    // Cancel before drive → zero iterations, prompt return, nothing recorded.
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = ColdStart {
        iterations: 50,
        warmup: true,
        tool: "echo".to_string(),
        args: json!({}),
    };

    let ctx = make_ctx().with_session_factory(mock_normal_factory());
    ctx.cancel_token.cancel();

    let started = Instant::now();
    let outcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "drive should return promptly when pre-cancelled; took {elapsed:?}"
    );
    assert_eq!(outcome.total_calls, 0, "got {outcome:?}");
    assert!(
        outcome.notes.iter().any(|n| n.contains("cancelled")),
        "cancellation note expected: {outcome:?}"
    );
    assert!(
        ctx.metrics.snapshot_per_tool().is_empty(),
        "cancelled run must record nothing"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
