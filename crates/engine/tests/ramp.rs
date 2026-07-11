//! `Ramp` scenario integration test.
//!
//! Verifies the ramp scenario steps concurrency end-to-end against a real
//! (mock-normal) MCP server and records calls without error.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::ramp::Ramp;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::SessionFactory;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
}

/// Hand-rolled factory wrapping `Session::spawn` for a fixture — same shape
/// `Run::execute` builds from the config (see `tests/pool_concurrency.rs`).
fn fixture_factory(fixture: &str) -> SessionFactory {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    SessionFactory::new(move || {
        let py = py.clone();
        let mock = mock.clone();
        async move { Session::spawn(&py, [mock.as_os_str()]).await }
    })
}

#[tokio::test]
async fn ramp_happy_path_increases_concurrency() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Ramp {
        from_concurrent: 1,
        to_concurrent: 4,
        step_duration: Duration::from_millis(150),
        step_increment: 1,
        tool: "echo".to_string(),
        args: json!({}),
        breaking_point: None,
    };

    assert_eq!(scenario.name(), "ramp");
    let _schema = scenario.config_schema();

    let ctx = make_ctx();
    let started = Instant::now();
    let outcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "ramp with 4x150ms steps should finish well under 3s; took {elapsed:?}"
    );
    assert!(
        outcome.total_calls > 0,
        "expected >0 calls across ramp steps: {outcome:?}"
    );
    assert_eq!(
        outcome.error_count, 0,
        "mock-normal should never error: {outcome:?}"
    );
    assert!(
        outcome.notes.iter().any(|n| n.contains("ramp")),
        "expected at least one note to mention 'ramp': notes={:?}",
        outcome.notes,
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// M8 pooled path: with a session factory attached, every step must drive
/// its declared level through a worker pool and disclose the real worker
/// count per step. Parallelism itself is proven by counts in
/// `tests/pool_concurrency.rs`; here we assert the per-step pool engagement
/// and sane counters against the fast mock-normal fixture. Step durations
/// are generous because the step deadline is anchored before the spawn
/// phase and Python spawns are slow under parallel-suite CPU contention —
/// no wall-clock assertions.
#[tokio::test]
async fn ramp_pooled_path_drives_each_step_through_worker_pool() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    // Orchestrator-style initial session — stays idle on the pooled path.
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Ramp {
        from_concurrent: 1,
        to_concurrent: 2,
        step_duration: Duration::from_secs(3),
        step_increment: 1,
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
        breaking_point: None,
    };

    let ctx = make_ctx().with_session_factory(fixture_factory("mock-normal.py"));
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(
        outcome.notes.iter().any(|n| n.contains("ramp: pooled")),
        "pooled disclosure note expected: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("pool: 1 workers (1 requested)")),
        "step 1 must disclose its pool size: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("pool: 2 workers (2 requested)")),
        "step 2 must disclose its pool size: {outcome:?}"
    );
    assert!(
        !outcome
            .notes
            .iter()
            .any(|n| n.contains("iterations-per-step")),
        "sequential-fallback note must not appear on the pooled path: {outcome:?}"
    );
    // No hard `total_calls > 0`: step deadlines are anchored before the
    // spawn phase (disclosed pool semantics), so under cross-binary CPU
    // contention the Python spawns can eat a whole step window and
    // legitimately yield zero calls — see tests/pool_concurrency.rs for the
    // real-parallelism-by-count proof. When calls did happen, mock-normal
    // must never error.
    assert_eq!(
        outcome.error_count, 0,
        "mock-normal should never error: {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, outcome.total_calls,
        "got {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
