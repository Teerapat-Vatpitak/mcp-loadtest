//! Integration tests for [`HttpTransport`] (Streamable HTTP simple variant).
//!
//! Uses `httpmock` to stand up a local stub server per-test — does not depend
//! on Agent L's `mock-http-server.py` fixture (these tests stand alone so
//! Agent J can validate independently of L's progress).

use std::time::Duration;

use httpmock::prelude::*;
use mcp_loadtest_core::config::ServerConfig;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use mcp_loadtest_protocol::transport::{HostGuard, Transport, TransportError};

/// Wrap calls in a short outer timeout so a wedged transport surfaces as a
/// test failure rather than a CI hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// `httpmock` binds its stub server to `127.0.0.1:<port>`. The SSRF guard
/// (ADR 0012) blocks loopback IP literals unless the literal is explicitly
/// listed in `allowed_hosts` — the operator escape hatch. Mirror the
/// permissive-guard construction `tests/host_guard.rs` relies on (and the
/// `ws.rs` unit tests' `allow_all`): a config whose `allowed_hosts` is exactly
/// `["127.0.0.1"]`, fed to `HostGuard::from_config`. This only threads the
/// guard so the transport compiles + connects; it does not change what any
/// test asserts about transport behavior.
fn loopback_guard() -> HostGuard {
    let mut cfg = ServerConfig::stdio("python".into(), vec![]);
    cfg.allowed_hosts = vec!["127.0.0.1".to_string()];
    HostGuard::from_config(&cfg)
}

#[tokio::test]
async fn connect_succeeds_against_running_server() {
    let server = MockServer::start_async().await;
    // No mocks needed — just verifying the URL parses and the client builds.
    let url = server.url("/mcp");

    let guard = loopback_guard();
    let transport = tokio::time::timeout(TEST_TIMEOUT, HttpTransport::connect(&url, &guard))
        .await
        .expect("connect timed out")
        .expect("connect should succeed");
    assert!(transport.pid().is_none(), "HTTP transport has no local pid");
}

#[tokio::test]
async fn connect_rejects_hostname_resolving_to_loopback() {
    // ADR 0016: `localhost` resolves to loopback via the hosts file / OS
    // stack (no external DNS); with an empty allowlist the resolver layer
    // must reject before any connect (port 1 is never dialed).
    let guard = HostGuard::from_config(&ServerConfig::stdio("python".into(), vec![]));
    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        HttpTransport::connect("http://localhost:1/", &guard),
    )
    .await
    .expect("connect should fail fast, before any network dial");
    let err = match result {
        Ok(_) => panic!("loopback-resolving hostname must be blocked"),
        Err(e) => e,
    };
    match err {
        TransportError::Other(m) => {
            assert!(m.contains("blocked host"), "stable substring missing: {m}");
            assert!(m.contains("ADR 0016"), "ADR 0016 marker missing: {m}");
        }
        other => panic!("expected TransportError::Other, got {other:?}"),
    }
}

#[tokio::test]
async fn hostname_escape_hatch_resolves_pins_and_round_trips() {
    // `allowed_hosts = ["localhost"]` is the hostname escape hatch (ADR
    // 0016): the loopback resolution is permitted because the operator
    // explicitly trusts the name, and the vetted addresses are pinned into
    // the reqwest client (`resolve_to_addrs`), which this round trip rides.
    let server = MockServer::start_async().await;

    let response_body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(200)
                .header("content-type", "application/json")
                .body(response_body);
        })
        .await;

    let mut cfg = ServerConfig::stdio("python".into(), vec![]);
    cfg.allowed_hosts = vec!["localhost".to_string()];
    let guard = HostGuard::from_config(&cfg);

    // httpmock binds 127.0.0.1; dial it by name instead.
    let url = format!("http://localhost:{}/mcp", server.port());
    let mut transport = HttpTransport::connect(&url, &guard)
        .await
        .expect("allowlisted hostname must connect via pinned addresses");

    let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let got = tokio::time::timeout(TEST_TIMEOUT, transport.request(request_body))
        .await
        .expect("request timed out")
        .expect("request failed");
    assert_eq!(got, response_body);
    mock.assert_async().await;
}

#[tokio::test]
async fn protocol_version_header_attached_only_after_negotiation() {
    // Streamable HTTP (2025-06-18+): the `initialize` POST itself carries no
    // MCP-Protocol-Version header; every request after `set_protocol_version`
    // (called by Session once the handshake negotiates) must carry it.
    let server = MockServer::start_async().await;
    let versioned = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("MCP-Protocol-Version", "2025-11-25");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#);
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;

    // Before negotiation: no header → the version-requiring mock must not
    // match (httpmock answers unmatched requests with a non-2xx status).
    let before = tokio::time::timeout(TEST_TIMEOUT, transport.request(body))
        .await
        .expect("request timed out");
    assert!(before.is_err(), "expected non-2xx without the header");
    assert_eq!(versioned.hits_async().await, 0);

    // After negotiation: header attached → mock matches.
    transport.set_protocol_version("2025-11-25");
    let after = tokio::time::timeout(TEST_TIMEOUT, transport.request(body))
        .await
        .expect("request timed out")
        .expect("request with header failed");
    assert!(after.contains("\"ok\":true"));
    assert_eq!(versioned.hits_async().await, 1);
}

#[tokio::test]
async fn invalid_protocol_version_header_value_is_skipped() {
    // A permissively-accepted garbage version can contain bytes invalid in
    // an HTTP header; the transport must skip the header, not poison every
    // subsequent request.
    let server = MockServer::start_async().await;
    let any = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#);
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");
    transport.set_protocol_version("bad\nversion");

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let got = tokio::time::timeout(TEST_TIMEOUT, transport.request(body))
        .await
        .expect("request timed out")
        .expect("request must still succeed without the header");
    assert!(got.contains("\"ok\":true"));
    assert_eq!(any.hits_async().await, 1);
}

#[tokio::test]
async fn request_round_trips_json() {
    let server = MockServer::start_async().await;

    let response_body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;

    let mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("content-type", "application/json");
            then.status(200)
                .header("content-type", "application/json")
                .body(response_body);
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");

    let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let got = tokio::time::timeout(TEST_TIMEOUT, transport.request(request_body))
        .await
        .expect("request timed out")
        .expect("request failed");

    assert_eq!(
        got, response_body,
        "transport should return body verbatim; Session parses"
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn non_2xx_returns_http_error() {
    let server = MockServer::start_async().await;

    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(500).body("internal server error");
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");

    let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let err = tokio::time::timeout(TEST_TIMEOUT, transport.request(request_body))
        .await
        .expect("request timed out")
        .expect_err("expected http error");

    match err {
        TransportError::Http(msg) => {
            assert!(
                msg.contains("500"),
                "error should mention status 500, got: {msg}"
            );
        }
        other => panic!("expected TransportError::Http, got {other:?}"),
    }
    mock.assert_async().await;
}

#[tokio::test]
async fn notify_does_not_block_on_response() {
    let server = MockServer::start_async().await;

    // Server replies with 202 Accepted and no body — typical for an MCP
    // notification ack. notify() must still return Ok.
    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(202);
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");

    let notif_body = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    tokio::time::timeout(TEST_TIMEOUT, transport.notify(notif_body))
        .await
        .expect("notify timed out")
        .expect("notify should return Ok on empty 2xx");
    mock.assert_async().await;
}

#[tokio::test]
async fn sse_response_returns_other_until_m5() {
    let server = MockServer::start_async().await;

    // Stub a streamable-HTTP SSE response. M4 minimal scope must surface a
    // clear "not yet supported" error rather than try to parse an SSE body
    // as a single JSON object.
    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("event: message\ndata: {}\n\n");
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");

    let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let err = tokio::time::timeout(TEST_TIMEOUT, transport.request(request_body))
        .await
        .expect("request timed out")
        .expect_err("expected SSE rejection");

    match err {
        TransportError::Other(msg) => {
            assert!(
                msg.to_lowercase().contains("sse")
                    || msg.to_lowercase().contains("event-stream")
                    || msg.to_lowercase().contains("m5"),
                "error message should signal SSE-not-supported, got: {msg}"
            );
        }
        other => panic!("expected TransportError::Other, got {other:?}"),
    }
    mock.assert_async().await;
}

#[tokio::test]
async fn shutdown_is_noop_ok() {
    let server = MockServer::start_async().await;
    let guard = loopback_guard();
    let transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");

    let boxed: Box<dyn Transport> = Box::new(transport);
    tokio::time::timeout(TEST_TIMEOUT, boxed.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown should return Ok");
}
