//! Integration tests for the M8 session pool — `sustained`'s pooled path.
//!
//! Parallelism is proven by **call counts**, never elapsed-time bounds: the
//! suite runs with heavy CPU contention, so wall-clock assertions are only
//! ever generous upper bounds on promptness (cancellation).

mod helpers;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::pattern::{Pattern, PatternScenario};
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::SessionFactory;
use mcp_loadtest_protocol::{Session, SessionError};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Serializes the python-spawn-heavy tests in this binary. The pooled
/// deadline is anchored *before* the spawn phase (by design — spin-up counts
/// against the configured duration), so a herd of concurrent interpreter
/// spawns from sibling tests can eat the whole duration before any worker
/// gets call time, flaking the count assertions. Taking this lock keeps the
/// spawn herds disjoint; the count-based proofs stay unchanged.
static SPAWN_HEAVY: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn spawn_heavy_lock() -> &'static tokio::sync::Mutex<()> {
    SPAWN_HEAVY.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(5),  // hang_threshold
        Duration::from_secs(10), // grace_period
    )
}

/// Hand-rolled factory wrapping `Session::spawn` for a fixture — same shape
/// `Run::execute` builds from the config, kept local so the tests stay
/// focused on the pool behavior.
fn fixture_factory(fixture: &str) -> SessionFactory {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    SessionFactory::new(move || {
        let py = py.clone();
        let mock = mock.clone();
        async move { Session::spawn(&py, [mock.as_os_str()]).await }
    })
}

/// Pre-spawn `n` fixture sessions for a count-asserting test to hand out
/// through its factory. Spawning happens *before* the scenario anchors its
/// deadline, so multi-second Python spawn latency under suite contention
/// cannot eat the call window (the pool charges in-window spawn time
/// against the duration by design — ADR 0017). nextest runs process-per-test,
/// so the in-binary `SPAWN_HEAVY` lock alone cannot keep the spawn herds
/// disjoint there.
async fn pre_spawned_sessions(fixture: &str, n: usize) -> Arc<Mutex<Vec<Session>>> {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    let mut sessions = Vec::with_capacity(n);
    for _ in 0..n {
        sessions.push(
            Session::spawn(&py, [mock.as_os_str()])
                .await
                .expect("pre-spawn failed"),
        );
    }
    Arc::new(Mutex::new(sessions))
}

/// The headline test: against `mock-slow.py` (2s per call), one sequential
/// session can complete at most 3 calls in 6s. Four pooled workers must beat
/// that ceiling — proven purely by count.
#[tokio::test]
async fn pooled_sustained_beats_sequential_call_ceiling() {
    let _spawn_heavy = spawn_heavy_lock().lock().await;
    let mock = helpers::fixture_path("mock-slow.py");
    let py = helpers::python();

    // Orchestrator-style initial session — stays idle on the pooled path.
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // Worker sessions pre-spawned so the 6s window is all call time — see
    // `pre_spawned_sessions`. The count proof is unchanged: four workers
    // still drive four distinct sessions concurrently.
    let workers = pre_spawned_sessions("mock-slow.py", 4).await;
    let factory = {
        let workers = workers.clone();
        SessionFactory::new(move || {
            let workers = workers.clone();
            async move {
                Ok(workers
                    .lock()
                    .expect("lock poisoned")
                    .pop()
                    .expect("one pre-spawned session per worker"))
            }
        })
    };

    let scenario = Sustained {
        concurrent: 4,
        duration: Duration::from_secs(6),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx().with_session_factory(factory);
    let outcome = scenario.drive(&mut session, &ctx).await;

    // Sequential ceiling in 6s at 2s/call is 3 calls; >= 4 total calls is
    // only reachable with real multi-session parallelism.
    eprintln!(
        "pooled sustained vs 3-call sequential ceiling: total_calls={} successful={}",
        outcome.total_calls, outcome.successful_calls
    );
    assert!(
        outcome.total_calls >= 4,
        "expected pooled workers to beat the 3-call sequential ceiling; got {outcome:?}"
    );
    assert_eq!(outcome.error_count, 0, "got {outcome:?}");
    assert_eq!(
        outcome.successful_calls, outcome.total_calls,
        "mock-slow always answers; got {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("pool: 4 workers (4 requested)")),
        "pool summary note must state the real worker count: {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// PatternScenario used to accept `concurrent` but always drive the borrowed
/// session sequentially. Pin it to the same pool semantics as Sustained.
#[tokio::test]
async fn pooled_pattern_scenario_honors_concurrent_workers() {
    let _spawn_heavy = spawn_heavy_lock().lock().await;
    let mock = helpers::fixture_path("mock-slow.py");
    let py = helpers::python();
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let workers = pre_spawned_sessions("mock-slow.py", 2).await;
    let factory = {
        let workers = workers.clone();
        SessionFactory::new(move || {
            let workers = workers.clone();
            async move {
                Ok(workers
                    .lock()
                    .expect("lock poisoned")
                    .pop()
                    .expect("one pre-spawned session per worker"))
            }
        })
    };
    let scenario = PatternScenario::new(
        2,
        Duration::from_secs(3),
        vec![Pattern::single_call("echo", json!({"x": 1}))],
    );
    let outcome = scenario
        .drive(&mut session, &make_ctx().with_session_factory(factory))
        .await;

    // A single mock-slow session completes at most two calls in this window.
    // Two workers each finish two, proving the pattern wrapper did not drop
    // the concurrency setting on its way into the sustained engine.
    assert!(
        outcome.total_calls >= 4,
        "pattern workers did not beat the two-call sequential ceiling: {outcome:?}"
    );
    assert_eq!(outcome.successful_calls, outcome.total_calls);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("pool: 2 workers (2 requested)")),
        "got {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Without a session factory the pooled path must not engage: the loop runs
/// sequentially on the provided session and discloses it in the notes.
#[tokio::test]
async fn no_factory_falls_back_to_sequential_with_note() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Sustained {
        concurrent: 4,
        duration: Duration::from_secs(1),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx(); // no session_factory attached
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(outcome.total_calls > 0, "got {outcome:?}");
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("sequential") && n.contains("not multiplexed")),
        "sequential disclosure note expected: {outcome:?}"
    );
    assert!(
        !outcome.notes.iter().any(|n| n.starts_with("pool:")),
        "pool must not engage without a factory: {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Cancellation fired before `drive` → the pool must not spawn anything and
/// must return promptly with zero calls.
#[tokio::test]
async fn pre_fired_cancellation_returns_fast_with_zero_calls() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Sustained {
        concurrent: 4,
        duration: Duration::from_secs(60),
        tool: "echo".to_string(),
        args: json!({}),
    };

    let ctx = make_ctx().with_session_factory(fixture_factory("mock-normal.py"));
    ctx.cancel_token.cancel();

    let started = Instant::now();
    let outcome = scenario.drive(&mut session, &ctx).await;
    let elapsed = started.elapsed();

    // Generous promptness bound (not a tight wall-clock assertion): a
    // pre-cancelled run must not sit out the 60s duration.
    assert!(
        elapsed < Duration::from_secs(10),
        "drive should return promptly when pre-cancelled; took {elapsed:?}"
    );
    assert_eq!(outcome.total_calls, 0, "got {outcome:?}");
    assert!(
        outcome.notes.iter().any(|n| n.contains("cancelled")),
        "cancellation note expected: {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Spawn failures for part of the pool: the survivors keep driving, the
/// shortfall is disclosed, and each failed spawn is counted as one error.
#[tokio::test]
async fn partial_spawn_failure_proceeds_with_survivors() {
    let _spawn_heavy = spawn_heavy_lock().lock().await;
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // Closure-controlled failure (no DNS/network): invocations 0 and 1 hand
    // out a pre-spawned session (see `pre_spawned_sessions`), invocations
    // >= 2 fail.
    let invocations = Arc::new(AtomicU32::new(0));
    let pre_spawned = pre_spawned_sessions("mock-normal.py", 2).await;
    let factory = {
        let invocations = invocations.clone();
        let pre_spawned = pre_spawned.clone();
        SessionFactory::new(move || {
            let n = invocations.fetch_add(1, Ordering::SeqCst);
            let pre_spawned = pre_spawned.clone();
            async move {
                if n >= 2 {
                    return Err(SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "injected spawn failure",
                    )));
                }
                Ok(pre_spawned
                    .lock()
                    .expect("lock poisoned")
                    .pop()
                    .expect("one pre-spawned session per surviving worker"))
            }
        })
    };

    let scenario = Sustained {
        concurrent: 4,
        // With pre-spawned survivor sessions the window is nearly all call
        // time; mock-normal answers in milliseconds, so 2s is generous.
        duration: Duration::from_secs(2),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx().with_session_factory(factory);
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        4,
        "one factory invocation per requested worker"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("pool: 2 workers (4 requested)")),
        "shortfall must be disclosed: {outcome:?}"
    );
    assert_eq!(
        outcome.error_count, 2,
        "exactly the two failed spawns (mock-normal calls never error): {outcome:?}"
    );
    assert_eq!(
        outcome.incomplete_worker_count, 2,
        "requested concurrency must remain a typed fail-closed signal: {outcome:?}"
    );
    assert!(
        outcome.total_calls > 0,
        "surviving workers must keep driving: {outcome:?}"
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
