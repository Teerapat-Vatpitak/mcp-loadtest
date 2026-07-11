//! End-to-end happy-path test: spawn `mock-normal.py`, do `initialize` + `tools/list`
//! + `tools/call`, then shut down cleanly.
//!
//! This is the **M1 deliverable test** — if this stays green, the protocol stack
//! and process lifecycle work end-to-end on stdio.

mod helpers;

use std::time::Duration;

use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::mcp::Content;
use serde_json::json;

#[tokio::test]
async fn handshake_list_call() {
    let mock = helpers::fixture_path("mock-normal.py");
    let py = helpers::python();

    let mut session = Session::spawn(&py, [mock.as_os_str()])
        .await
        .expect("spawn failed");

    assert!(
        !session.server_protocol_version.is_empty(),
        "server should report a protocol version during initialize",
    );

    let tools = session.list_tools().await.expect("list_tools failed");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected `echo` tool, got: {tools:?}",
    );

    let result = session
        .call_tool("echo", &json!({"msg": "hello"}))
        .await
        .expect("call_tool failed");

    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    match &result.content[0] {
        Content::Text { text } => {
            // The mock echoes args as JSON-stringified text. Just check our payload survives.
            assert!(
                text.contains("hello"),
                "echoed text missing payload: {text}"
            );
        }
        other => panic!("expected text content, got {other:?}"),
    }

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown errored");
}
