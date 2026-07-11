//! `Spike` scenario integration tests.
//!
//! Verifies the spike scenario runs through warmup → spike → cooldown phases
//! against a real (mock-normal) MCP server and emits the expected summary
//! notes the report layer reads.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::spike::Spike;
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
async fn spike_happy_path_drives_all_three_phases() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Spike {
        baseline_concurrent: 1,
        spike_concurrent: 4,
        warmup: Duration::from_millis(200),
        spike_duration: Duration::from_millis(200),
        cooldown: Duration::from_millis(200),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };

    assert_eq!(scenario.name(), "spike");
    let _schema = scenario.config_schema();

    let ctx = make_ctx();
    let started = Instant::now();
    let outcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "spike with 3x200ms phases should finish well under 3s; took {elapsed:?}"
    );

    assert!(
        outcome.total_calls > 0,
        "expected >0 calls across warmup+spike+cooldown: {outcome:?}"
    );
    assert_eq!(
        outcome.error_count, 0,
        "mock-normal should never error: {outcome:?}"
    );

    assert!(
        outcome.notes.iter().any(|n| n.contains("spike")),
        "expected at least one note to mention 'spike': notes={:?}",
        outcome.notes,
    );
    assert!(
        outcome.notes.iter().any(|n| n.starts_with("spike.warmup:")),
        "expected a warmup phase note: notes={:?}",
        outcome.notes,
    );
    assert!(
        outcome.notes.iter().any(|n| n.starts_with("spike.spike:")),
        "expected a spike phase note: notes={:?}",
        outcome.notes,
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike.cooldown:")),
        "expected a cooldown phase note: notes={:?}",
        outcome.notes,
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike: peak=") && n.contains("warmup=")),
        "expected the final plan-summary note: notes={:?}",
        outcome.notes,
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn spike_against_crashing_server_survives_without_hang() {
    // Failure-mode coverage for `Spike`: mock-crash.py randomly exits during
    // a call (~1% probability per call). The test asserts the **survival**
    // properties — the scenario doesn't hang, completes within reasonable
    // time, and records its outcome — rather than a strict `error_count > 0`
    // because the crash is stochastic: with ~80 calls × 1% crash rate, a
    // strict assertion flakes ~45% of the time. The expected error path is
    // exercised by `deadlock.rs::mock_broken_detects_deadlock` and the
    // fuzzer integration test; here we just need to know spike survives.
    let mock = helpers::fixture_path("mock-crash.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // Longer phases push call count up to ~300+, making a crash statistically
    // near-certain (P(no crash) = 0.99^300 ≈ 5%) — but the assertion below
    // doesn't depend on the crash actually firing.
    let scenario = Spike {
        baseline_concurrent: 1,
        spike_concurrent: 4,
        warmup: Duration::from_millis(300),
        spike_duration: Duration::from_millis(300),
        cooldown: Duration::from_millis(300),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };

    let ctx = make_ctx();
    let started = Instant::now();
    let outcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "spike should not hang on a crashed server; took {elapsed:?}"
    );
    assert!(
        outcome.total_calls > 0,
        "expected >0 calls before/at crash: {outcome:?}"
    );
    // A crash that happens mid-phase MUST NOT be misclassified as a deadlock.
    assert_eq!(
        outcome.deadlock_count, 0,
        "crashes are not deadlocks: {outcome:?}"
    );
    // If errors did fire, that's the expected path; if they didn't, the run
    // happened to dodge the 1% crash dice — both prove "spike survives a
    // crash-prone server", which is the property under test.
    if outcome.error_count == 0 {
        eprintln!(
            "note: mock-crash dodged in this run ({} calls, no crash). \
             Survival path still verified by elapsed-time bound above.",
            outcome.total_calls
        );
    }

    // Best-effort shutdown — the server may already be gone after a crash.
    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;
}

/// M8 pooled path: with a session factory attached, each phase must drive
/// its declared concurrency through a worker pool (burst phase = the big
/// worker count) and disclose the real pool size per phase. Parallelism
/// itself is proven by counts in `tests/pool_concurrency.rs`; here we assert
/// per-phase pool engagement and sane counters against the fast mock-normal
/// fixture. Phase durations are generous because each phase deadline is
/// anchored before its spawn phase — no wall-clock assertions.
#[tokio::test]
async fn spike_pooled_path_drives_phases_through_worker_pools() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    // Orchestrator-style initial session — stays idle on the pooled path.
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Spike {
        baseline_concurrent: 1,
        spike_concurrent: 2,
        warmup: Duration::from_secs(2),
        spike_duration: Duration::from_secs(2),
        cooldown: Duration::from_secs(2),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };

    let ctx = make_ctx().with_session_factory(fixture_factory("mock-normal.py"));
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(
        outcome.notes.iter().any(|n| n.contains("spike: pooled")),
        "pooled disclosure note expected: {outcome:?}"
    );
    // Per-phase summary notes must state the real worker counts.
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike.warmup:") && n.contains("workers=1")),
        "warmup phase note with workers=1 expected: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike.spike:") && n.contains("workers=2")),
        "burst phase note with workers=2 expected: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike.cooldown:") && n.contains("workers=1")),
        "cooldown phase note with workers=1 expected: {outcome:?}"
    );
    // The burst phase must have engaged a real 2-worker pool.
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike.spike:") && n.contains("pool: 2 workers (2 requested)")),
        "burst phase pool-size disclosure expected: {outcome:?}"
    );
    assert!(
        !outcome
            .notes
            .iter()
            .any(|n| n.contains("iterations-per-tick")),
        "sequential-fallback note must not appear on the pooled path: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.starts_with("spike: peak=") && n.contains("warmup=")),
        "final plan-summary note expected: {outcome:?}"
    );
    // No hard `total_calls > 0`: phase deadlines are anchored before the
    // spawn phase (disclosed pool semantics), so under cross-binary CPU
    // contention the Python spawns can eat an entire phase window and
    // legitimately yield zero calls. Real-parallelism-by-count is
    // `tests/pool_concurrency.rs`'s job; this test pins that the pooled
    // path engages and discloses honestly. When calls did happen,
    // mock-normal must never error.
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

/// Regression: the sequential phase loop used to call both
/// `Recorder::record` and `Recorder::record_tool` per call, but
/// `record_tool` already bumps the global aggregate internally — so every
/// sequential spike call was double-counted in the global outcome counters
/// and latency histogram (per-tool counts were correct). Pin global ==
/// outcome so the two recording paths can't drift apart again.
#[tokio::test]
async fn spike_sequential_records_each_call_exactly_once() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Spike {
        baseline_concurrent: 1,
        spike_concurrent: 2,
        warmup: Duration::from_millis(150),
        spike_duration: Duration::from_millis(150),
        cooldown: Duration::from_millis(150),
        tool: "echo".to_string(),
        args: json!({ "msg": "once" }),
    };

    // No factory on this ctx → sequential fallback path.
    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(outcome.total_calls > 0, "expected calls: {outcome:?}");
    let snap = ctx.metrics.snapshot();
    assert_eq!(
        snap.outcomes.success, outcome.successful_calls,
        "global success count must equal the outcome's (no double count); \
         snapshot={snap:?} outcome={outcome:?}"
    );
    assert_eq!(
        snap.throughput.total_requests, outcome.total_calls,
        "global request count must equal the outcome's (no double count); \
         snapshot={snap:?} outcome={outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
