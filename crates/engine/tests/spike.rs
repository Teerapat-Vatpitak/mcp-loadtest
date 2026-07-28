//! `Spike` scenario integration tests.
//!
//! Verifies the spike scenario runs through warmup → spike → cooldown phases
//! against a real (mock-normal) MCP server and emits the expected summary
//! notes the report layer reads.

mod helpers;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_core::report::{ProcessStats, Report, ServerInfo};
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

async fn pre_spawned_sessions(fixture: &str, count: usize) -> Arc<Mutex<Vec<Session>>> {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    let mut sessions = Vec::with_capacity(count);
    for _ in 0..count {
        sessions.push(
            Session::spawn(&py, [mock.as_os_str()])
                .await
                .expect("pre-spawn failed"),
        );
    }
    Arc::new(Mutex::new(sessions))
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

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn spike_against_crashing_server_survives_without_hang() {
    // Deterministic terminal-path coverage: the fixture exits before replying
    // to the first tools/call. Random mode remains available for realism, but
    // an adversarial regression must prove that the crash actually occurred.
    let mock = helpers::fixture_path("mock-crash.py");
    let py = helpers::python();

    let mut session = Session::spawn(
        &py,
        [mock.as_os_str(), "--crash-after".as_ref(), "1".as_ref()],
    )
    .await
    .expect("spawn failed");

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
    assert!(
        outcome.error_count >= 1,
        "deterministic crash must be recorded as an error: {outcome:?}"
    );
    let counts = ctx.metrics.snapshot().outcomes;
    assert!(
        counts.crash + counts.disconnected >= 1,
        "deterministic process exit must be classified as crash/disconnected: {counts:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
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

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Regression: successful warmup/cooldown calls must not hide a spike phase
/// whose sessions finished spawning only after that phase's deadline. Before
/// the pool-level zero-call guard, the aggregate had successful calls and no
/// typed incompleteness, so `Report::passed()` could return true even though
/// none of the two requested spike workers exercised a call.
#[tokio::test]
async fn delayed_spike_phase_workers_fail_closed_when_they_make_no_calls() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // One session for warmup, two for spike, one for cooldown. Returning the
    // spike sessions is deliberately delayed past the 50ms phase deadline;
    // warmup and cooldown still exercise calls, preventing the report's
    // general zero-call guard from being the reason this run fails.
    let sessions = pre_spawned_sessions("mock-normal.py", 4).await;
    let invocation = Arc::new(AtomicU32::new(0));
    let factory = {
        let sessions = Arc::clone(&sessions);
        let invocation = Arc::clone(&invocation);
        SessionFactory::new(move || {
            let sessions = Arc::clone(&sessions);
            let invocation = Arc::clone(&invocation);
            async move {
                let phase_slot = invocation.fetch_add(1, Ordering::SeqCst);
                let worker_session = sessions
                    .lock()
                    .expect("session queue lock poisoned")
                    .pop()
                    .expect("one pre-spawned session per phase worker");
                if matches!(phase_slot, 1 | 2) {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                Ok(worker_session)
            }
        })
    };

    let scenario = Spike {
        baseline_concurrent: 1,
        spike_concurrent: 2,
        warmup: Duration::from_millis(200),
        spike_duration: Duration::from_millis(50),
        cooldown: Duration::from_millis(200),
        tool: "echo".to_owned(),
        args: json!({ "msg": "phase-completeness" }),
    };
    let ctx = make_ctx().with_session_factory(factory);
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(
        outcome.total_calls > 0 && outcome.successful_calls > 0,
        "warmup/cooldown must provide otherwise-green evidence: {outcome:?}"
    );
    assert_eq!(outcome.error_count, 0, "mock-normal must stay healthy");
    assert_eq!(
        outcome.incomplete_worker_count, 2,
        "both spawned spike workers missed their call window: {outcome:?}"
    );
    assert_eq!(
        outcome
            .notes
            .iter()
            .filter(|note| {
                note.starts_with("spike.spike:")
                    && note.contains("completed without exercising a call")
            })
            .count(),
        2,
        "each zero-call spike worker must be attributable: {outcome:?}"
    );

    let report = Report {
        run_id: "zero-call-spike-regression".to_owned(),
        started_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_secs(1),
        scenario_name: "spike".to_owned(),
        server_info: ServerInfo {
            command: "mock-normal.py".to_owned(),
            args: Vec::new(),
            pid: None,
            protocol_version: None,
        },
        metrics: ctx.metrics.snapshot(),
        process: ProcessStats::default(),
        scenario_outcome: outcome,
        trace_path: None,
        threshold_violations: Vec::new(),
        coverage: None,
    };
    assert!(
        !report.passed(),
        "requested spike concurrency was not exercised and must fail closed"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// A zero-length warmup/cooldown means "omit this optional phase", not
/// "spawn workers that can never make a call". The latter used to trip the
/// pool completeness guard and make a healthy spike-only run fail.
#[tokio::test]
async fn zero_length_optional_phases_are_skipped_without_false_failure() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let sessions = pre_spawned_sessions("mock-normal.py", 2).await;
    let invocation = Arc::new(AtomicU32::new(0));
    let factory = {
        let sessions = Arc::clone(&sessions);
        let invocation = Arc::clone(&invocation);
        SessionFactory::new(move || {
            let sessions = Arc::clone(&sessions);
            let invocation = Arc::clone(&invocation);
            async move {
                invocation.fetch_add(1, Ordering::SeqCst);
                Ok(sessions
                    .lock()
                    .expect("session queue lock poisoned")
                    .pop()
                    .expect("one pre-spawned session per spike worker"))
            }
        })
    };

    let scenario = Spike {
        baseline_concurrent: 3,
        spike_concurrent: 2,
        warmup: Duration::ZERO,
        spike_duration: Duration::from_millis(300),
        cooldown: Duration::ZERO,
        tool: "echo".to_owned(),
        args: json!({ "msg": "spike-only" }),
    };
    let ctx = make_ctx().with_session_factory(factory);
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(outcome.total_calls > 0, "spike phase must run: {outcome:?}");
    assert_eq!(outcome.error_count, 0, "mock-normal must stay healthy");
    assert_eq!(
        outcome.incomplete_worker_count, 0,
        "omitted phases must not manufacture incomplete workers: {outcome:?}"
    );
    assert_eq!(
        invocation.load(Ordering::SeqCst),
        2,
        "only the two spike workers should be spawned"
    );
    for phase in ["warmup", "cooldown"] {
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note == &format!("spike.{phase}: skipped because duration is 0")),
            "missing explicit {phase} skip note: {outcome:?}"
        );
    }

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
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

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
