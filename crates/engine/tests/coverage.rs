//! Coverage tracking integration tests (M7 Agent V).
//!
//! Confirms:
//! - Running a `Sustained` scenario against `mock-normal.py` records the
//!   `echo` tool call into the per-tool counters, and the synthesized
//!   `CoverageReport` reports 100% coverage.
//! - Synthesizing a `CoverageReport` directly with a partial `exercised` map
//!   correctly identifies the un-exercised tools.

mod helpers;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use mcp_loadtest_core::coverage::CoverageReport;
use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::sustained::Sustained;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
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

#[tokio::test]
async fn coverage_against_mock_normal_records_echo_call() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    // Pull the registered list before driving traffic — same shape Run::execute uses.
    let registered: Vec<String> = session
        .list_tools()
        .await
        .expect("list_tools failed")
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        registered.contains(&"echo".to_string()),
        "mock-normal must advertise echo; got {registered:?}"
    );

    let scenario = Sustained {
        concurrent: 1,
        duration: Duration::from_millis(500),
        tool: "echo".to_string(),
        args: json!({ "x": 1 }),
    };

    let ctx = make_ctx();
    let outcome = scenario.drive(&mut session, &ctx).await;
    assert!(
        outcome.total_calls > 0,
        "expected at least one echo call; got {outcome:?}"
    );

    // Snapshot per-tool counters and build the coverage report — mirrors
    // what Run::execute does in production.
    let per_tool = ctx.metrics.snapshot_per_tool();
    let echo_calls = per_tool
        .get("echo")
        .map(|m| m.throughput.total_requests)
        .unwrap_or(0);
    assert!(
        echo_calls > 0,
        "echo should be exercised; per_tool={per_tool:?}"
    );

    let exercised: BTreeMap<String, u64> = per_tool
        .into_iter()
        .map(|(k, v)| (k, v.throughput.total_requests))
        .collect();
    let coverage = CoverageReport::build(registered, exercised);

    assert!(coverage.exercised.contains_key("echo"));
    assert!(coverage.exercised["echo"] > 0);
    assert_eq!(
        coverage.coverage_pct(),
        100.0,
        "all registered tools exercised; got {coverage:?}"
    );
    assert!(
        coverage.unexercised.is_empty(),
        "no tools should be unexercised; got {:?}",
        coverage.unexercised
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[test]
fn unexercised_tools_listed() {
    let mut exercised = BTreeMap::new();
    exercised.insert("a".to_string(), 1);
    let coverage = CoverageReport::build(
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        exercised,
    );
    assert_eq!(coverage.unexercised, vec!["b".to_string(), "c".to_string()]);
}
