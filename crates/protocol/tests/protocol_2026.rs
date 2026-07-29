//! Final-protocol MRTR and dual-era auto-negotiation tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mcp_loadtest_protocol::mcp::ProtocolVersion;
use mcp_loadtest_protocol::{Session, ToolCallRound, Transport, TransportError};
use serde_json::{Value, json};

struct ScriptedTransport {
    responses: VecDeque<String>,
    requests: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        self.requests
            .lock()
            .expect("request lock")
            .push(serde_json::from_str(body).expect("request JSON"));
        self.responses.pop_front().ok_or(TransportError::Closed)
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        self.requests
            .lock()
            .expect("request lock")
            .push(serde_json::from_str(body).expect("notification JSON"));
        Ok(())
    }

    async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
        Ok(())
    }
}

fn scripted(responses: Vec<Value>) -> (ScriptedTransport, Arc<Mutex<Vec<Value>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        ScriptedTransport {
            responses: responses
                .into_iter()
                .map(|value| serde_json::to_string(&value).expect("response JSON"))
                .collect(),
            requests: Arc::clone(&requests),
        },
        requests,
    )
}

#[tokio::test]
async fn mrtr_echoes_opaque_state_with_a_new_id() {
    let state = r#"{"opaque":"do not parse","nonce":"α"}"#;
    let (transport, requests) = scripted(vec![
        json!({"jsonrpc":"2.0","id":1,"result":{
            "resultType":"complete","ttlMs":0,"cacheScope":"private",
            "supportedVersions":["2026-07-28"],"capabilities":{}
        }}),
        json!({"jsonrpc":"2.0","id":2,"result":{
            "resultType":"input_required",
            "requestState":state,
            "inputRequests":{"confirm":{"method":"elicitation/create","params":{}}}
        }}),
        json!({"jsonrpc":"2.0","id":3,"result":{
            "resultType":"complete","content":[]
        }}),
    ]);
    let mut session = Session::from_transport_stateless(
        transport,
        Duration::from_secs(1),
        ProtocolVersion::V2026_07_28,
    )
    .await
    .expect("stateless session");

    let first = session
        .call_tool_round("needs-input", &json!({}), None, None)
        .await
        .expect("first round");
    let ToolCallRound::InputRequired(input) = first else {
        panic!("expected input-required round");
    };
    let responses = json!({"confirm":{"action":"accept","content":{"ok":true}}});
    let second = session
        .call_tool_round(
            "needs-input",
            &json!({}),
            input.request_state.as_deref(),
            Some(&responses),
        )
        .await
        .expect("second round");
    assert!(matches!(second, ToolCallRound::Complete(_)));

    let requests = requests.lock().expect("request lock");
    assert_eq!(requests[1]["id"], 2);
    assert_eq!(requests[2]["id"], 3);
    assert_eq!(requests[2]["params"]["requestState"], state);
    assert_eq!(requests[2]["params"]["inputResponses"], responses);
}

#[tokio::test]
async fn auto_falls_back_to_legacy_but_not_on_modern_errors() {
    let (transport, requests) = scripted(vec![
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"not found"}}),
        json!({"jsonrpc":"2.0","id":2,"result":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "serverInfo":{"name":"legacy","version":"1"}
        }}),
    ]);
    let session = Session::from_transport_auto(transport, Duration::from_secs(1))
        .await
        .expect("legacy fallback");
    assert_eq!(
        session.negotiated_version(),
        Some(ProtocolVersion::V2025_11_25)
    );
    let requests = requests.lock().expect("request lock");
    assert_eq!(requests[0]["method"], "server/discover");
    assert_eq!(
        requests[0]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(requests[1]["method"], "initialize");
    assert_eq!(requests[1]["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(requests[2]["method"], "notifications/initialized");
}
