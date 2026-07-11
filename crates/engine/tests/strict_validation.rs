//! End-to-end coverage for opt-in strict args validation (ADR 0010) and its
//! result-side extension (DESIGN §13.1 item 2).
//!
//! Args side: drives the real `Sustained` scenario against `mock-schema.py`
//! (whose `echo` tool advertises `{required: ["msg"], msg: string}`) with
//! strict validation enabled exactly the way `Run::execute` enables it, and
//! asserts the production policy: bad args are rejected client-side as
//! `ProtocolError`; compliant args pass untouched.
//!
//! Result side: drives `mock-output-schema.py` (whose `report` tool
//! advertises an `outputSchema`; `--mode ok|bad|missing` shapes its
//! `structuredContent`) and asserts the documented NON-GATING policy:
//! violating or absent `structuredContent` warns but every call still
//! succeeds, and the result payload reaches the caller unaltered.

mod helpers;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn make_ctx() -> (RunContext, Recorder) {
    let metrics = Recorder::new();
    let ctx = RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        metrics.clone(),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    (ctx, metrics)
}

/// Mirror `Run::execute`: pull `tools/list`, hand the name→inputSchema map to
/// the session so subsequent `call_tool`s validate args.
async fn enable_strict(session: &mut Session) {
    let tools = session.list_tools().await.expect("tools/list failed");
    let schemas: HashMap<String, serde_json::Value> = tools
        .iter()
        .map(|t| (t.name.clone(), t.input_schema.clone()))
        .collect();
    session.set_strict_tool_schemas(schemas);
}

/// Mirror `Run::execute` with the result-side extension: register both
/// the args registry and the name→outputSchema map (tools that advertise
/// one) from the same single `tools/list`.
async fn enable_strict_with_output(session: &mut Session) {
    let tools = session.list_tools().await.expect("tools/list failed");
    session.set_strict_tool_schemas(
        tools
            .iter()
            .map(|t| (t.name.clone(), t.input_schema.clone()))
            .collect(),
    );
    session.set_strict_tool_output_schemas(
        tools
            .iter()
            .filter_map(|t| t.output_schema.clone().map(|s| (t.name.clone(), s)))
            .collect(),
    );
}

/// Spawn `mock-output-schema.py` in the given response mode with full strict
/// validation (args + output schemas) enabled.
async fn spawn_output_schema_mock(mode: &str) -> Session {
    let mock = helpers::fixture_path("mock-output-schema.py");
    let py = helpers::python();

    let mut session = Session::spawn(
        &py,
        [mock.as_os_str(), OsStr::new("--mode"), OsStr::new(mode)],
    )
    .await
    .expect("spawn failed");
    enable_strict_with_output(&mut session).await;
    session
}

/// Drive `Sustained` on the `report` tool and assert the run is NOT gated:
/// every call succeeds and nothing is classified as `ProtocolError`.
async fn assert_result_side_non_gating(session: &mut Session, mode: &str) {
    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_millis(500),
        tool: "report".to_string(),
        args: json!({}),
    };

    let (ctx, metrics) = make_ctx();
    let outcome = scenario.drive(session, &ctx).await;

    assert!(
        outcome.total_calls > 0,
        "mode {mode}: expected calls; got {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, outcome.total_calls,
        "mode {mode}: result-side validation must be non-gating — every call \
         must still succeed; got {outcome:?}"
    );
    let snap = metrics.snapshot();
    assert_eq!(
        snap.outcomes.protocol_error, 0,
        "mode {mode}: result-side mismatches must not be ProtocolError; got {:?}",
        snap.outcomes
    );
}

#[tokio::test]
async fn strict_rejects_args_violating_advertised_schema() {
    let mock = helpers::fixture_path("mock-schema.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");
    enable_strict(&mut session).await;

    // `echo` requires a string `msg`; send an int instead → schema violation.
    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_millis(500),
        tool: "echo".to_string(),
        args: json!({ "msg": 123 }),
    };

    let (ctx, metrics) = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(
        outcome.total_calls > 0,
        "expected calls to be attempted; got {outcome:?}"
    );
    assert_eq!(
        outcome.successful_calls, 0,
        "no call should succeed — every one violates the schema; got {outcome:?}"
    );

    let snap = metrics.snapshot();
    assert_eq!(
        snap.outcomes.protocol_error, outcome.total_calls,
        "every rejected call must be a ProtocolError; got {:?}",
        snap.outcomes
    );
    assert_eq!(
        snap.outcomes.success, 0,
        "no success expected; got {:?}",
        snap.outcomes
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn strict_allows_schema_compliant_args() {
    let mock = helpers::fixture_path("mock-schema.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");
    enable_strict(&mut session).await;

    // Compliant args: `msg` present and a string.
    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_millis(500),
        tool: "echo".to_string(),
        args: json!({ "msg": "hello" }),
    };

    let (ctx, metrics) = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(outcome.total_calls > 0, "expected calls; got {outcome:?}");
    assert_eq!(
        outcome.successful_calls, outcome.total_calls,
        "strict mode must not gate schema-compliant calls; got {outcome:?}"
    );
    assert_eq!(
        metrics.snapshot().outcomes.protocol_error,
        0,
        "compliant args must not produce ProtocolError"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn strict_result_conformant_passes_silently() {
    let mut session = spawn_output_schema_mock("ok").await;

    assert_result_side_non_gating(&mut session, "ok").await;

    // The conformant payload reaches the caller intact.
    let result = session
        .call_tool("report", &json!({}))
        .await
        .expect("conformant result must succeed");
    assert_eq!(
        result.structured_content,
        Some(json!({ "answer": "forty-two", "count": 42 })),
        "structuredContent must round-trip unaltered"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn strict_result_violation_warns_but_does_not_gate() {
    let mut session = spawn_output_schema_mock("bad").await;

    // `bad` returns structuredContent missing required `answer` with a
    // wrong-typed `count` — the documented policy is Warn, never gate.
    assert_result_side_non_gating(&mut session, "bad").await;

    // The violating payload still reaches the caller unaltered: validation
    // is observability, not a filter.
    let result = session
        .call_tool("report", &json!({}))
        .await
        .expect("violating result must still succeed (warn is non-gating)");
    assert_eq!(
        result.structured_content,
        Some(json!({ "count": "not-an-integer" })),
        "violating structuredContent must be passed through unaltered"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn strict_result_missing_structured_content_warns_but_does_not_gate() {
    let mut session = spawn_output_schema_mock("missing").await;

    // Advertised outputSchema + absent structuredContent is a spec
    // violation, but the result side must stay non-gating observability.
    assert_result_side_non_gating(&mut session, "missing").await;

    let result = session
        .call_tool("report", &json!({}))
        .await
        .expect("missing structuredContent must still succeed (warn is non-gating)");
    assert!(
        result.structured_content.is_none(),
        "no structuredContent was sent, so none must be synthesized"
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
