//! Logical MCP tool errors (`isError: true`) are failures despite their
//! successful JSON-RPC envelope.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::race_check::RaceCheck;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::{Session, SessionFactory};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn context() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
}

fn factory() -> SessionFactory {
    let python = helpers::python();
    let fixture = helpers::fixture_path("mock-tool-error.py");
    SessionFactory::new(move || {
        let python = python.clone();
        let fixture = fixture.clone();
        async move { Session::spawn(&python, [fixture.as_os_str()]).await }
    })
}

#[tokio::test]
async fn sustained_counts_logical_tool_errors_as_failures() {
    let python = helpers::python();
    let fixture = helpers::fixture_path("mock-tool-error.py");
    let mut session = Session::spawn(&python, [fixture.as_os_str()])
        .await
        .expect("spawn");
    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_millis(100),
        tool: "fail".to_owned(),
        args: json!({}),
    };
    let ctx = context();
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert!(outcome.total_calls > 0, "got {outcome:?}");
    assert_eq!(outcome.successful_calls, 0, "got {outcome:?}");
    assert_eq!(outcome.error_count, outcome.total_calls, "got {outcome:?}");
    assert_eq!(
        ctx.metrics.snapshot().outcomes.server_error,
        outcome.total_calls
    );
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn race_check_counts_logical_tool_errors_as_failures() {
    let python = helpers::python();
    let fixture = helpers::fixture_path("mock-tool-error.py");
    let mut session = Session::spawn(&python, [fixture.as_os_str()])
        .await
        .expect("spawn");
    let scenario = RaceCheck {
        concurrent: 2,
        tool: "fail".to_owned(),
        args: json!({}),
    };
    let ctx = context().with_session_factory(factory());
    let outcome = scenario.drive(&mut session, &ctx).await;

    assert_eq!(outcome.total_calls, 2, "got {outcome:?}");
    assert_eq!(outcome.successful_calls, 0, "got {outcome:?}");
    assert_eq!(outcome.error_count, 2, "got {outcome:?}");
    assert_eq!(ctx.metrics.snapshot().outcomes.server_error, 2);
    session.shutdown().await.expect("shutdown");
}
