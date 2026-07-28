//! Integration tests for the [`pattern`] engine.
//!
//! Covers:
//! 1. The `single_call` helper produces a one-step pattern with the right defaults.
//! 2. Weighted-random selection is approximately fair over many iterations.
//! 3. A multi-step pattern actually issues each step against a real server
//!    (mock-normal.py) and records every step in `ctx.metrics`.
//! 4. `ErrorBehavior::Abort` short-circuits a multi-step pattern at the first
//!    step that errors.
//!
//! [`pattern`]: mcp_loadtest_engine::scenario::pattern

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::RunContext;
use mcp_loadtest_engine::scenario::pattern::{self, ErrorBehavior, Pattern, PatternStep};
use mcp_loadtest_protocol::Session;
use rand::SeedableRng;
use rand::rngs::StdRng;
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

fn echo_step(payload: serde_json::Value) -> PatternStep {
    PatternStep {
        tool: "echo".to_string(),
        args: payload,
    }
}

fn three_step_pattern(on_err: ErrorBehavior) -> Pattern {
    Pattern {
        name: "trio".to_string(),
        weight: 1.0,
        think_time: Duration::ZERO,
        on_step_error: on_err,
        steps: vec![
            echo_step(json!({ "step": 1 })),
            echo_step(json!({ "step": 2 })),
            echo_step(json!({ "step": 3 })),
        ],
    }
}

#[test]
fn single_call_helper_creates_one_step_pattern() {
    let p = Pattern::single_call("echo", json!({ "x": 1 }));
    assert_eq!(p.steps.len(), 1);
    assert_eq!(p.steps[0].tool, "echo");
    assert_eq!(p.steps[0].args, json!({ "x": 1 }));
    assert!(
        (p.weight - 1.0).abs() < f64::EPSILON,
        "weight should default to 1.0, got {}",
        p.weight
    );
    assert_eq!(p.think_time, Duration::ZERO);
    assert_eq!(p.on_step_error, ErrorBehavior::Continue);
}

#[test]
fn weighted_pick_distribution() {
    // Two patterns at weights 0.7 / 0.3. Over 1000 picks the empirical split
    // should land within ±5% of those weights.
    let patterns = vec![
        Pattern {
            name: "heavy".to_string(),
            weight: 0.7,
            think_time: Duration::ZERO,
            on_step_error: ErrorBehavior::Continue,
            steps: vec![echo_step(json!({}))],
        },
        Pattern {
            name: "light".to_string(),
            weight: 0.3,
            think_time: Duration::ZERO,
            on_step_error: ErrorBehavior::Continue,
            steps: vec![echo_step(json!({}))],
        },
    ];

    // Seeded RNG so the test is deterministic.
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    let n: u32 = 1000;
    let mut counts = [0u32; 2];
    for _ in 0..n {
        let chosen = pattern::pick(&patterns, &mut rng).expect("picker returned None");
        let idx = if chosen.name == "heavy" { 0 } else { 1 };
        counts[idx] += 1;
    }

    let heavy_share = f64::from(counts[0]) / f64::from(n);
    let light_share = f64::from(counts[1]) / f64::from(n);

    let tol = 0.05; // ±5%
    assert!(
        (heavy_share - 0.7).abs() < tol,
        "heavy share {heavy_share:.3} not within ±{tol} of 0.7 (counts={counts:?})",
    );
    assert!(
        (light_share - 0.3).abs() < tol,
        "light share {light_share:.3} not within ±{tol} of 0.3 (counts={counts:?})",
    );
}

#[tokio::test]
async fn multi_step_pattern_drives_sequence() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let patterns = vec![three_step_pattern(ErrorBehavior::Continue)];
    let ctx = make_ctx();
    let mut rng = StdRng::seed_from_u64(0);

    let stats = pattern::execute(&patterns, &mut session, &ctx, &mut rng).await;

    assert_eq!(stats.steps_attempted, 3, "should attempt all 3 steps");
    assert_eq!(stats.steps_succeeded, 3, "all 3 should succeed");
    assert_eq!(stats.errors, 0);
    assert!(
        !stats.terminal_error,
        "no terminal error expected: {stats:?}"
    );

    // Metrics recorder should have seen all 3 successes.
    let snap = ctx.metrics.snapshot();
    assert_eq!(snap.outcomes.success, 3, "metrics didn't see all 3 calls");
    assert_eq!(snap.throughput.total_requests, 3);

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// Verifies `ErrorBehavior::Abort` halts the pattern at the first errored
/// step. We provoke a step-1 transport error by killing the child server's
/// process; then assert steps 2 and 3 are NEVER attempted.
#[tokio::test]
async fn error_behavior_abort_stops_at_first_error() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // Sanity: warm-up call succeeds.
    let warmup = vec![Pattern::single_call("echo", json!({ "warm": true }))];
    let ctx_warm = make_ctx();
    let mut rng = StdRng::seed_from_u64(0);
    let warm_stats = pattern::execute(&warmup, &mut session, &ctx_warm, &mut rng).await;
    assert_eq!(
        warm_stats.steps_succeeded, 1,
        "warm-up failed: {warm_stats:?}"
    );

    // Kill the child process so every subsequent call_tool yields a
    // transport error.
    if let Some(pid) = session.pid() {
        kill_pid(pid);
        // Give the OS a moment to deliver the signal.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let abort_pattern = vec![three_step_pattern(ErrorBehavior::Abort)];
    let ctx = make_ctx();
    let stats = pattern::execute(&abort_pattern, &mut session, &ctx, &mut rng).await;

    assert_eq!(
        stats.steps_attempted, 1,
        "Abort should stop after the first errored step (attempted={}, errors={})",
        stats.steps_attempted, stats.errors,
    );
    assert!(stats.errors >= 1, "expected ≥1 error: {stats:?}");
    assert_eq!(
        stats.steps_succeeded, 0,
        "no step should have succeeded: {stats:?}"
    );

    tokio::time::timeout(Duration::from_secs(2), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    use std::process::Command;
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    use std::process::Command;
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
}
