//! Integration tests for the fuzzer's raw-byte payload path (T3.1).
//!
//! Exercises the raw-transport-only [`FuzzPayload`] variants against
//! `mock-normal.py` with a [`SessionFactory`] attached, so the actual send →
//! classify → respawn path runs (not the honest skip). Asserts the four
//! invariants the plan pins: the harness never hangs, `total_calls` counts
//! only sent payloads, the `Cancelled` bucket is never touched, and every raw
//! send lands in a defined `FuzzClass` (mock-normal explicitly rejects invalid
//! JSON, survives notifications, and exposes its deliberately permissive
//! handling of some structurally invalid requests).

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::fuzzer::{FuzzPayload, Fuzzer};
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::SessionFactory;
use tokio_util::sync::CancellationToken;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// A `RunContext` whose factory respawns fresh `mock-normal.py` sessions —
/// this is what lets the fuzzer recover the connection each raw send poisons.
fn ctx_with_mock_normal_factory() -> RunContext {
    let py = helpers::python();
    let mock = helpers::fixture_path("mock-normal.py");
    let factory = SessionFactory::new(move || {
        let py = py.clone();
        let mock = mock.clone();
        async move { Session::spawn(&py, [mock.as_os_str()]).await }
    });
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_millis(300), // hang_threshold
        Duration::from_millis(700), // grace_period
    )
    .with_session_factory(factory)
}

async fn shutdown_cleanly(session: Session) {
    tokio::time::timeout(SHUTDOWN_TIMEOUT, session.shutdown())
        .await
        .expect("fuzzer session shutdown timed out")
        .expect("fuzzer session shutdown failed");
}

#[tokio::test]
async fn raw_payloads_against_mock_normal_are_classified_without_cancel_or_hang() {
    let ctx = ctx_with_mock_normal_factory();

    // Initial (borrowed) session; the fuzzer poisons and respawns it as raw
    // payloads go out.
    let mut session = ctx
        .session_factory
        .as_ref()
        .expect("factory attached")
        .spawn()
        .await
        .expect("initial spawn");

    // Only raw-transport-only variants, so every iteration takes the raw path.
    let raw_variants: Vec<FuzzPayload> = FuzzPayload::all()
        .into_iter()
        .filter(|p| p.requires_raw_transport())
        .collect();
    assert!(!raw_variants.is_empty(), "expected raw-transport variants");

    let iterations = 12u32;
    let fuzzer = Fuzzer {
        iterations,
        seed: 99,
        payloads: raw_variants,
    };

    let recorder = ctx.metrics.clone();

    // Whole run must finish well inside a generous bound — each raw send
    // respawns a fresh python, so allow headroom. A hang here is exactly the
    // bug this scenario exists to catch, so we fail loudly rather than block
    // the suite.
    let outcome = tokio::time::timeout(Duration::from_secs(60), fuzzer.drive(&mut session, &ctx))
        .await
        .expect("fuzzer raw run hung");

    // total_calls counts only sent payloads. All variants are raw and a
    // factory is attached, so every iteration is a real send: total == iters.
    assert_eq!(
        outcome.total_calls, iterations as u64,
        "each raw iteration sends exactly once: {outcome:?}"
    );

    let snap = recorder.snapshot();

    // No Cancelled pollution — the raw/skip paths must never record Cancelled.
    assert_eq!(
        snap.outcomes.cancelled, 0,
        "raw path must not record Cancelled: {snap:?}"
    );

    // The recorder saw exactly the sent payloads.
    assert_eq!(
        snap.throughput.total_requests, iterations as u64,
        "recorder must see every raw send: {snap:?}"
    );

    // Every raw send is classified. A protocol-level rejection / live server
    // is a successful fuzz probe, while crash and wedge remain failures.
    let classified = outcome.successful_calls + outcome.error_count + outcome.deadlock_count as u64;
    assert_eq!(
        classified, outcome.total_calls,
        "every raw send must be classified: {outcome:?}"
    );
    let healthy = snap.outcomes.success + snap.outcomes.expected_rejection;
    assert_eq!(
        outcome.successful_calls, healthy,
        "clean survival and explicit protocol rejection are successful raw probes: {outcome:?}"
    );
    assert_eq!(
        snap.outcomes.protocol_error, 0,
        "expected fuzz handling must not be reported as a real protocol failure: {snap:?}"
    );
    assert_eq!(
        snap.throughput.successful_requests, outcome.successful_calls,
        "healthy raw-probe reactions count as successful fuzz requests: {snap:?}"
    );
    assert_eq!(
        outcome.deadlock_count, 0,
        "mock-normal explicitly rejects invalid JSON and must not wedge: {outcome:?}"
    );
    assert_eq!(
        snap.outcomes.disconnected, 0,
        "malformed JSON must not crash the normal fixture: {snap:?}"
    );
    assert!(
        snap.outcomes.expected_rejection > 0,
        "invalid JSON must produce an explicit JSON-RPC rejection: {snap:?}"
    );
    assert_eq!(
        outcome.error_count,
        snap.outcomes.disconnected + snap.outcomes.malformed + snap.outcomes.server_error,
        "raw failures must reconcile to disconnect, malformed acceptance, or unexpected server error: {outcome:?}"
    );
    assert!(
        snap.outcomes.malformed > 0,
        "the deliberately permissive fixture must expose acceptance of at least one malformed raw request: {snap:?}"
    );

    // The classified reactions split between healthy survival / explicit
    // rejection and suspicious malformed acceptance. The reference fixture
    // must neither crash nor wedge on these frames.
    let survived = snap.outcomes.success + snap.outcomes.expected_rejection;
    let accepted_badly = snap.outcomes.malformed + snap.outcomes.server_error;
    assert_eq!(
        survived + accepted_badly,
        iterations as u64,
        "every raw reaction must land in one explicit bucket: {snap:?}"
    );

    shutdown_cleanly(session).await;
}

#[tokio::test]
async fn raw_wrong_jsonrpc_version_acceptance_fails_closed() {
    let ctx = ctx_with_mock_normal_factory();
    let mut session = ctx
        .session_factory
        .as_ref()
        .expect("factory attached")
        .spawn()
        .await
        .expect("initial spawn");
    let fuzzer = Fuzzer {
        iterations: 1,
        seed: 1,
        payloads: vec![FuzzPayload::WrongJsonRpcVersion],
    };

    let outcome = fuzzer.drive(&mut session, &ctx).await;
    let snap = ctx.metrics.snapshot();
    assert_eq!(outcome.total_calls, 1);
    assert_eq!(outcome.successful_calls, 0);
    assert_eq!(
        outcome.error_count, 1,
        "accepting a non-2.0 raw request must be an unexpected fuzz failure: {outcome:?}"
    );
    assert_eq!(snap.outcomes.malformed, 1);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("class=Accepted")),
        "retained diagnostics must identify permissive acceptance: {outcome:?}"
    );

    shutdown_cleanly(session).await;
}

#[tokio::test]
async fn raw_invalid_json_is_explicitly_rejected_without_crashing_fixture() {
    let ctx = ctx_with_mock_normal_factory();
    let mut session = ctx
        .session_factory
        .as_ref()
        .expect("factory attached")
        .spawn()
        .await
        .expect("initial spawn");
    let fuzzer = Fuzzer {
        iterations: 1,
        seed: 1,
        payloads: vec![FuzzPayload::InvalidJson],
    };

    let outcome = fuzzer.drive(&mut session, &ctx).await;
    let snap = ctx.metrics.snapshot();
    assert_eq!(outcome.total_calls, 1);
    assert_eq!(outcome.successful_calls, 1);
    assert_eq!(outcome.error_count, 0);
    assert_eq!(outcome.deadlock_count, 0);
    assert_eq!(snap.outcomes.expected_rejection, 1);
    assert_eq!(snap.outcomes.disconnected, 0);
    assert_eq!(snap.outcomes.deadlock, 0);

    shutdown_cleanly(session).await;
}

#[tokio::test]
async fn raw_payloads_without_factory_are_skipped_not_sent() {
    // No factory attached -> the raw path is unavailable, so raw variants must
    // be skipped (honest note, no `total_calls` bump, no recorder activity).
    let py = helpers::python();
    let mock = helpers::fixture_path("mock-normal.py");
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let ctx = RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_millis(300),
        Duration::from_millis(700),
    );
    let recorder = ctx.metrics.clone();

    let raw_variants: Vec<FuzzPayload> = FuzzPayload::all()
        .into_iter()
        .filter(|p| p.requires_raw_transport())
        .collect();
    let fuzzer = Fuzzer {
        iterations: 8,
        seed: 3,
        payloads: raw_variants,
    };

    let outcome = tokio::time::timeout(Duration::from_secs(10), fuzzer.drive(&mut session, &ctx))
        .await
        .expect("fuzzer skip run hung");

    assert_eq!(
        outcome.total_calls, 0,
        "raw variants without a factory must not send: {outcome:?}"
    );
    let snap = recorder.snapshot();
    assert_eq!(
        snap.throughput.total_requests, 0,
        "no recorder activity for skipped raw variants: {snap:?}"
    );
    assert_eq!(
        snap.outcomes.cancelled, 0,
        "skips are not Cancelled: {snap:?}"
    );
    // The run still classifies all 8 iterations as skip findings.
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("8 iterations classified")),
        "summary note must record all 8 iterations: {outcome:?}"
    );

    shutdown_cleanly(session).await;
}
