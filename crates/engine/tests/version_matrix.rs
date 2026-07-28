//! Integration tests for the `version_matrix` scenario.
//!
//! Uses a **version-aware** `SessionFactory` whose recipe spawns
//! `mock-normal.py` with `--protocol-version` set to the advertised revision,
//! so each matrix row's server echoes exactly what that row advertised —
//! exercising the full `with_version` → advertise → negotiate loop.

mod helpers;

use std::time::{Duration, Instant};

use mcp_loadtest_core::ProtocolVersion;
use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::version_matrix::VersionMatrix;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::SessionFactory;
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_millis(200), // hang_threshold
        Duration::from_millis(500), // grace_period
    )
}

/// Version-aware factory spawning `fixture` with the advertised revision
/// echoed back by the mock's `--protocol-version` knob.
fn echoing_factory(fixture: &str) -> SessionFactory {
    let mock = helpers::fixture_path(fixture);
    let py = helpers::python();
    SessionFactory::new_versioned(move |version| {
        let advertised = version.unwrap_or(ProtocolVersion::DEFAULT_ADVERTISED);
        let mock = mock.clone();
        let py = py.clone();
        async move {
            Session::spawn_with_timeout_and_version(
                &py,
                [
                    mock.as_os_str().to_owned(),
                    "--protocol-version".into(),
                    advertised.as_str().into(),
                ],
                SpawnOptions::default(),
                Duration::from_secs(10),
                advertised,
            )
            .await
        }
    })
}

async fn spawn_normal() -> Session {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();
    Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed")
}

#[tokio::test]
async fn happy_path_drives_every_supported_revision() {
    let matrix = VersionMatrix {
        versions: Vec::new(), // all supported
        calls_per_version: 2,
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };
    let ctx = make_ctx().with_session_factory(echoing_factory("mock-normal.py"));
    let mut session = spawn_normal().await;

    let outcome = matrix.drive(&mut session, &ctx).await;

    let expected = ProtocolVersion::SUPPORTED.len() as u64 * 2;
    assert_eq!(outcome.total_calls, expected, "notes: {:?}", outcome.notes);
    assert_eq!(outcome.successful_calls, expected);
    assert_eq!(outcome.deadlock_count, 0);
    assert_eq!(outcome.error_count, 0);

    // Every revision gets its own per-tool metric channel...
    let per_tool = ctx.metrics.snapshot_per_tool();
    for v in ProtocolVersion::SUPPORTED {
        let key = VersionMatrix::metric_key(*v);
        let m = per_tool
            .get(&key)
            .unwrap_or_else(|| panic!("missing per-tool metrics for {key}"));
        assert_eq!(m.throughput.total_requests, 2, "{key}");
        // ...and a per-revision summary note.
        assert!(
            outcome.notes.iter().any(|n| n.starts_with(&key)),
            "no note for {key}: {:?}",
            outcome.notes
        );
        // The mock echoed the advertised revision, so no mismatch notes.
        assert!(
            !outcome
                .notes
                .iter()
                .any(|n| n.contains("negotiated") || n.contains("unknown version")),
            "unexpected mismatch note: {:?}",
            outcome.notes
        );
    }

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn deadlock_is_attributed_to_the_revision_that_hung() {
    // mock-broken hangs on the first tools/call regardless of revision; a
    // single-revision matrix pins the attribution machinery (deadlock count,
    // hung_for_ms, per-revision note) without waiting out several rows.
    let matrix = VersionMatrix {
        versions: vec![ProtocolVersion::V2025_03_26],
        calls_per_version: 3,
        tool: "echo".to_string(),
        args: json!({ "msg": "hi" }),
    };
    let ctx = make_ctx().with_session_factory(echoing_factory("mock-broken.py"));
    let mut session = spawn_normal().await;

    let outcome = matrix.drive(&mut session, &ctx).await;

    assert!(
        outcome.deadlock_count >= 1,
        "expected a deadlock: {outcome:?}"
    );
    assert!(!outcome.hung_for_ms.is_empty());
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("version:2025-03-26") && n.contains("deadlock")),
        "deadlock note must name the revision: {:?}",
        outcome.notes
    );
    // The row bails after the deadlock — no further calls on a wedged session.
    assert_eq!(outcome.total_calls, 1);

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}

#[tokio::test]
async fn degrades_to_noop_without_a_factory() {
    let matrix = VersionMatrix {
        versions: Vec::new(),
        calls_per_version: 2,
        tool: "echo".to_string(),
        args: json!({}),
    };
    let ctx = make_ctx(); // no factory attached
    let mut session = spawn_normal().await;

    let outcome = matrix.drive(&mut session, &ctx).await;

    assert_eq!(outcome.total_calls, 0);
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("requires a session factory")),
        "expected the degrade note: {:?}",
        outcome.notes
    );

    tokio::time::timeout(Duration::from_secs(15), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
