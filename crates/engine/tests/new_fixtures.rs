//! Integration tests for four mock fixtures:
//! `mock-error.py`, `mock-malformed.py`, `mock-leak.py`, `mock-slow-init.py`.
//!
//! Each test drives the real fixture through the public API the same way the
//! existing `tests/scenarios_basic.rs` / `tests/soak.rs` suites do (spawn a
//! `Session`, build a `RunContext`, run a `Scenario`, assert on the recorded
//! metrics) — proving the fixture exercises the intended client code path:
//!
//! - `mock-error.py`     → standard JSON-RPC errors → `CallOutcome::ProtocolError`
//! - `mock-malformed.py` → `serde_json` parse error → `CallOutcome::Malformed`
//! - `mock-leak.py`      → monotonically growing RSS → `detect_leak` slope > 0
//! - `mock-slow-init.py` → 5s handshake still completes inside the 10s budget
//!
//! Timing-sensitive assertions use generous bounds; the RSS-slope test is
//! `#[ignore]`-able (see its doc comment) because process sampling can be
//! noisy on shared CI runners.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::process::ProcessSampler;
use mcp_loadtest_engine::scenario::soak::{Soak, detect_leak};
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Build a `RunContext` with a fresh `Recorder`. Mirrors the helper used by
/// `tests/scenarios_basic.rs` / `tests/soak.rs` (kept local — `tests/helpers`
/// only exposes `fixture_path` / `python`).
fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
}

/// `mock-error.py` returns a JSON-RPC error on every `tools/call`, cycling
/// `-32601 / -32602 / -32603`. Standard JSON-RPC failures in a normal
/// workload map to
/// [`mcp_loadtest_core::metrics::CallOutcome::ProtocolError`] so a permissive
/// error-rate threshold cannot turn a protocol mismatch into PASS. A short
/// sustained run must therefore record zero successes and a non-zero
/// `protocol_error` count.
#[tokio::test]
async fn mock_error_classifies_as_protocol_error() {
    let mock = helpers::fixture_path("mock-error.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_secs(2),
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };

    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(
        outcome.total_calls > 0,
        "expected >0 calls in 2s; got {outcome:?}"
    );
    assert!(
        outcome.error_count > 0,
        "every call against mock-error should error; got {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, 0,
        "mock-error never succeeds; got {outcome:?}"
    );

    let snap = ctx.metrics.snapshot();
    assert!(
        snap.outcomes.protocol_error > 0,
        "standard JSON-RPC errors must classify as ProtocolError; outcomes={:?}",
        snap.outcomes
    );
    // Sanity: the cycled codes are standard protocol failures, not
    // implementation-defined server errors or malformed transport frames.
    assert_eq!(
        snap.outcomes.server_error, 0,
        "standard JSON-RPC errors must not be counted as ServerError; outcomes={:?}",
        snap.outcomes
    );
    assert_eq!(
        snap.outcomes.malformed, 0,
        "structured protocol errors must not be counted as Malformed; outcomes={:?}",
        snap.outcomes
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// `mock-malformed.py` emits a truncated-but-newline-terminated line on every
/// 10th `tools/call` (the other 9 are normal). The Rust stdio transport reads
/// a full line then `serde_json`-parses it; a complete-but-invalid line yields
/// a parse error which `classify_error` maps to
/// [`mcp_loadtest_core::metrics::CallOutcome::Malformed`]. With ≥15 calls at least one
/// malformed response is guaranteed, and the run must neither panic nor hang.
#[tokio::test]
async fn mock_malformed_classifies_as_malformed() {
    let mock = helpers::fixture_path("mock-malformed.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // 2s of sustained echo against this fixture drives well over 15 calls on
    // any machine (mock-normal-class latency is sub-ms), so the 10th/20th/…
    // malformed responses are hit deterministically. A generous outer timeout
    // guards against the "unterminated line → hang" regression: if the broken
    // line ever lost its trailing newline this would time out instead of
    // recording Malformed.
    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_secs(2),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx();
    let outcome = tokio::time::timeout(Duration::from_secs(30), scenario.drive(&mut session, &ctx))
        .await
        .expect("drive hung — broken line likely not newline-terminated (Timeout, not Malformed)");

    assert!(
        outcome.total_calls >= 15,
        "expected ≥15 calls in 2s so a 10th-call malformed lands; got {outcome:?}"
    );

    let snap = ctx.metrics.snapshot();
    assert!(
        snap.outcomes.malformed >= 1,
        "≥1 malformed response expected (every 10th call); outcomes={:?}",
        snap.outcomes
    );
    // The 9/10 good responses must still be counted as successes — the
    // fixture is not wholesale broken.
    assert!(
        snap.outcomes.success > 0,
        "the 9/10 valid responses must succeed; outcomes={:?}",
        snap.outcomes
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// `mock-leak.py` appends 10 KB to a module-global list on every `tools/call`
/// and never frees it, so the server's RSS climbs monotonically under load.
/// We sample the child process via [`ProcessSampler`] while a short soak runs,
/// then assert the linear-regression slope from [`detect_leak`] is strictly
/// positive (tolerant: we check *direction*, not magnitude).
///
/// `#[ignore]` rationale: process RSS sampling is inherently noisy on shared
/// CI runners (allocator arena reuse, GC-less but page-cache effects, sampler
/// scheduling jitter) and the soak window here is deliberately short to keep
/// the suite fast. The leak detector itself has exhaustive synthetic-shape
/// unit coverage in `scenario/soak/leak_detect.rs` + `tests/soak.rs`; this
/// test is the end-to-end smoke and is gated to avoid flaking the default
/// `cargo test` gate. Run explicitly with `--ignored` to exercise it.
//
// reason: end-to-end RSS-slope timing is environment-sensitive; unit-level
// detect_leak coverage already pins the algorithm. Documented in the report.
#[tokio::test]
#[ignore = "RSS-slope timing is environment-sensitive on shared CI; run with --ignored. detect_leak has full unit coverage."]
async fn mock_leak_rss_slope_positive() {
    let mock = helpers::fixture_path("mock-leak.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    let pid = session.pid().expect("stdio child must expose a pid");

    let cancel = CancellationToken::new();
    // Sample fast so a short soak still yields several points to regress.
    let sampler = ProcessSampler::spawn(pid, Duration::from_millis(250), cancel.clone());

    let scenario = Soak {
        concurrent: 1,
        duration: Duration::from_secs(6),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
        sample_interval: Duration::from_millis(500),
        latency_drift_ms_per_sec: 5.0,
    };

    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;
    assert!(
        outcome.total_calls > 0,
        "soak should drive calls against mock-leak; got {outcome:?}"
    );

    let stats = sampler.finish().await;
    assert!(
        stats.samples.len() >= 2,
        "need ≥2 RSS samples to fit a slope; got {} samples",
        stats.samples.len()
    );

    let series: Vec<(f64, f64)> = stats
        .samples
        .iter()
        .map(|s| (s.at_secs, s.rss_mb))
        .collect();
    let slope = detect_leak(&series).expect("≥2 distinct-t samples should fit a regression");

    // Tolerant: a real leak makes RSS strictly increase, so the slope must be
    // positive. We do NOT assert a magnitude — allocator behaviour and sample
    // cadence make the exact MB/sec environment-dependent. Peak > final-ish
    // monotonic growth is the signal.
    assert!(
        slope > 0.0,
        "leaking server RSS should regress to a positive slope; got {slope} MB/s, series={series:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

/// `mock-slow-init.py` sleeps 5s before answering `initialize`, then behaves
/// normally. 5s is under the session's 10s default startup budget, so the
/// handshake must still succeed — this test pins that contract.
///
/// **Note — cold_start is a real scenario** (`scenario/cold_start.rs`
/// respawns a fresh session per iteration via `RunContext::session_factory`;
/// `cold_start_measures_handshake_per_fresh_session` in
/// `tests/scenarios_basic.rs` covers it). This test still deliberately does
/// **not** drive the fixture through `cold_start` — a 5s handshake × N
/// iterations would be too slow for the suite. It only pins
/// `mock-slow-init.py`'s own contract (slow handshake within budget +
/// subsequent normal operation) so cold_start keeps a slow-handshake fixture
/// ready and a regression guard if the delay/budget relationship changes.
#[tokio::test]
async fn mock_slow_init_pinned_contract() {
    let mock = helpers::fixture_path("mock-slow-init.py");
    let py = helpers::python();

    // Wrap spawn in a > 5s (but < ∞) timeout: the 5s initialize sleep must
    // complete and the handshake succeed. A 15s ceiling proves it isn't
    // hanging while still tolerating slow CI + Python interpreter startup.
    let started = Instant::now();
    let mut session = tokio::time::timeout(
        Duration::from_secs(15),
        Session::spawn(&py, [mock.as_os_str()]),
    )
    .await
    .expect("slow-init spawn exceeded 15s — handshake hung")
    .expect("slow-init session should initialize within the 10s startup budget");
    let init_elapsed = started.elapsed();

    assert!(
        init_elapsed >= Duration::from_secs(4),
        "initialize should have taken ~5s (fixture sleeps 5s); took {init_elapsed:?}"
    );

    // After the slow handshake the server is normal: a tool call succeeds
    // promptly. This guards the "everything else responds immediately" half
    // of the fixture contract.
    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_millis(500),
        tool: "echo".to_string(),
        args: json!({ "ok": true }),
    };
    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;
    assert!(
        outcome.successful_calls > 0,
        "post-init tool calls must succeed (fixture is normal after handshake); got {outcome:?}"
    );
    assert_eq!(
        outcome.error_count, 0,
        "mock-slow-init only delays initialize; tool calls must not error; got {outcome:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
