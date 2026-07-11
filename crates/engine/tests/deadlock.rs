//! Integration tests for the `deadlock_probe` scenario (M2, Agent B + integration).
//!
//! Drives the real `DeadlockProbe` scenario end-to-end against:
//! - `mock-normal.py`  → expect 0 deadlocks, all SUCCESS
//! - `mock-broken.py`  → expect ≥1 deadlock (the canonical Vibe-Trading bug pattern)

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::deadlock_probe::DeadlockProbe;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
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

#[tokio::test]
async fn mock_normal_no_deadlock() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let probe = DeadlockProbe {
        concurrent: 5,
        hang_threshold: Duration::from_secs(2),
        grace_period: Duration::from_secs(5),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };

    let ctx = make_ctx();
    let outcome = probe.drive(&mut session, &ctx).await;

    assert_eq!(
        outcome.total_calls, 5,
        "all 5 iterations should run: {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, 5,
        "every call against mock-normal should succeed: {outcome:?}"
    );
    assert_eq!(
        outcome.deadlock_count, 0,
        "no deadlocks expected: {outcome:?}"
    );
    assert_eq!(outcome.hang_count, 0, "no slow calls expected: {outcome:?}");
    assert_eq!(outcome.error_count, 0, "no errors expected: {outcome:?}");

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// The killer test: replicate the Vibe-Trading PR #85 bug class.
///
/// `mock-broken.py` hangs forever on the first `tools/call`; with a 200ms hang
/// threshold and 500ms grace period, `DeadlockProbe` must classify it as a
/// deadlock and bail out of the iteration loop.
#[tokio::test]
async fn mock_broken_detects_deadlock() {
    let mock = helpers::fixture_path("mock-broken.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let probe = DeadlockProbe {
        concurrent: 5,
        hang_threshold: Duration::from_millis(200),
        grace_period: Duration::from_millis(500),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };

    let ctx = make_ctx();
    let outcome = probe.drive(&mut session, &ctx).await;

    assert!(
        outcome.deadlock_count >= 1,
        "expected at least one deadlock against mock-broken: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("deadlock detected")),
        "outcome should annotate the deadlock for the report: {outcome:?}"
    );
    // The structured field the `serve` deadlock_probe tool reads (it must not
    // re-parse the note string). One entry per deadlock, each at least the
    // hang_threshold the watchdog waited before giving up.
    assert_eq!(
        outcome.hung_for_ms.len(),
        outcome.deadlock_count as usize,
        "hung_for_ms must carry one duration per deadlock: {outcome:?}"
    );
    assert!(
        outcome.hung_for_ms.iter().all(|&ms| ms >= 200),
        "each hung_for duration should be >= the 200ms hang_threshold: {outcome:?}"
    );

    // After a deadlock the session is wedged; shutdown is best-effort.
    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;
}
