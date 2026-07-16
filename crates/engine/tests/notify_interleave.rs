//! A server may interleave JSON-RPC notifications (frames with a `method` and
//! no `id`) with responses at any time — `notifications/tools/list_changed`,
//! progress updates, etc. The single-flight `Session` reads the next line as
//! its response, so it must skip leading notification frames rather than
//! mis-read one and desync the stream. Regression coverage for the
//! interleaving bug found by dogfooding against the reference "everything"
//! server.

mod helpers;

use std::time::Duration;

use mcp_loadtest_protocol::Session;
use serde_json::json;

#[tokio::test]
async fn session_tolerates_interleaved_notifications() {
    let mock = helpers::fixture_path("mock-notify.py");
    let py = helpers::python();

    // `initialize` itself is preceded by a notification in this fixture, so a
    // successful spawn already proves the handshake skips notification frames.
    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn should succeed despite a notification before the initialize result");

    let tools = session
        .list_tools()
        .await
        .expect("list_tools should succeed");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "echo tool should be listed, got: {tools:?}"
    );

    let result = session
        .call_tool("echo", &json!({ "message": "interleaved" }))
        .await
        .expect("call_tool should return the response, not a stray notification");

    // The mock echoes the arguments back as JSON text.
    let rendered = format!("{result:?}");
    assert!(
        rendered.contains("interleaved"),
        "echoed args should round-trip through the response, got: {rendered}"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;
}
