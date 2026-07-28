//! Integration tests for [`HttpTransport`] (both Streamable HTTP response variants).
//!
//! Uses `httpmock` to stand up a local stub server per-test — does not depend
//! on Agent L's `mock-http-server.py` fixture (these tests stand alone so
//! Agent J can validate independently of L's progress).

use std::time::Duration;

use httpmock::prelude::*;
use mcp_loadtest_core::config::ServerConfig;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use mcp_loadtest_protocol::transport::{HostGuard, Transport, TransportError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// Wrap calls in a short outer timeout so a wedged transport surfaces as a
/// test failure rather than a CI hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

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

/// Serve one response using HTTP/1.1 chunked transfer coding, deliberately
/// omitting Content-Length so tests exercise the cumulative stream limit.
async fn spawn_chunked_response(
    status: &str,
    content_type: &str,
    body: Vec<u8>,
    wire_chunk_size: usize,
) -> (String, JoinHandle<()>) {
    assert_ne!(wire_chunk_size, 0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind chunked test server");
    let addr = listener.local_addr().expect("chunked test server address");
    let status = status.to_owned();
    let content_type = content_type.to_owned();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let mut request = Vec::new();
        let mut scratch = [0u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut scratch).await.expect("read request");
            assert_ne!(read, 0, "client closed before request headers");
            request.extend_from_slice(&scratch[..read]);
            if let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII request headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric content-length")
                })
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let read = socket.read(&mut scratch).await.expect("read request body");
            assert_ne!(read, 0, "client closed before request body");
            request.extend_from_slice(&scratch[..read]);
        }

        let response_headers = format!(
            "HTTP/1.1 {status}\r\n\
             Content-Type: {content_type}\r\n\
             Transfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n"
        );
        socket
            .write_all(response_headers.as_bytes())
            .await
            .expect("write response headers");
        for chunk in body.chunks(wire_chunk_size) {
            let prefix = format!("{:X}\r\n", chunk.len());
            if socket.write_all(prefix.as_bytes()).await.is_err()
                || socket.write_all(chunk).await.is_err()
                || socket.write_all(b"\r\n").await.is_err()
            {
                // Oversize readers intentionally drop the response as soon as
                // the cap is crossed, so a reset here is expected.
                return;
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
    });
    (format!("http://{addr}/mcp"), handle)
}

fn exact_limit_json_response() -> Vec<u8> {
    const PREFIX: &[u8] = br#"{"jsonrpc":"2.0","id":1,"result":{"padding":""#;
    const SUFFIX: &[u8] = br#""}}"#;
    let padding = MAX_HTTP_RESPONSE_BYTES - PREFIX.len() - SUFFIX.len();
    let mut body = Vec::with_capacity(MAX_HTTP_RESPONSE_BYTES);
    body.extend_from_slice(PREFIX);
    body.resize(body.len() + padding, b'x');
    body.extend_from_slice(SUFFIX);
    assert_eq!(body.len(), MAX_HTTP_RESPONSE_BYTES);
    body
}

fn assert_response_too_large(error: TransportError) {
    match error {
        TransportError::Http(message) => assert_eq!(
            message,
            format!("response body exceeds {MAX_HTTP_RESPONSE_BYTES}-byte limit")
        ),
        other => panic!("expected bounded HTTP body error, got {other:?}"),
    }
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
async fn request_failure_does_not_echo_endpoint_query() {
    const SECRET: &str = "query-secret-sentinel";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve port");
    let addr = listener.local_addr().expect("reserved address");
    drop(listener);

    let url = format!("http://{addr}/mcp?access_token={SECRET}&tenant=private");
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("HTTP construction is intentionally lazy");
    let err = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
    )
    .await
    .expect("request failure timed out")
    .expect_err("closed port must fail");
    let diagnostic = err.to_string();
    assert!(
        !diagnostic.contains(SECRET)
            && !diagnostic.contains("access_token")
            && !diagnostic.contains("tenant=private"),
        "HTTP error leaked endpoint query: {diagnostic}"
    );
}

#[tokio::test]
async fn redirect_is_not_followed_and_location_is_not_echoed() {
    const SECRET: &str = "redirect-query-secret";
    let server = MockServer::start_async().await;
    let redirect = server
        .mock_async(|when, then| {
            when.method(POST).path("/start");
            then.status(307)
                .header("location", format!("/target?token={SECRET}"));
        })
        .await;
    let target = server
        .mock_async(|when, then| {
            when.method(POST).path("/target");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        })
        .await;

    let mut transport = HttpTransport::connect(server.url("/start"), &loopback_guard())
        .await
        .expect("connect");
    let err = transport
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await
        .expect_err("redirect must surface as non-2xx");
    let diagnostic = err.to_string();
    assert_eq!(redirect.hits_async().await, 1);
    assert_eq!(
        target.hits_async().await,
        0,
        "redirect must not be followed"
    );
    assert!(
        !diagnostic.contains(SECRET),
        "redirect Location leaked through error: {diagnostic}"
    );
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
async fn request_accepts_chunked_json_at_exact_body_limit() {
    let body = exact_limit_json_response();
    let (url, server) =
        spawn_chunked_response("200 OK", "application/json", body.clone(), 64 * 1024).await;
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("connect");

    let response = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
    )
    .await
    .expect("exact-limit request timed out")
    .expect("exact-limit body must be accepted");
    assert_eq!(response.as_bytes(), body);
    server.await.expect("chunked server task");
}

#[tokio::test]
async fn request_rejects_oversized_chunked_json_without_content_length() {
    let body = vec![b'x'; MAX_HTTP_RESPONSE_BYTES + 1];
    let (url, server) = spawn_chunked_response("200 OK", "application/json", body, 64 * 1024).await;
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("connect");

    let error = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
    )
    .await
    .expect("oversized request timed out")
    .expect_err("chunked body over the cap must be rejected");
    assert_response_too_large(error);
    server.await.expect("chunked server task");
}

#[tokio::test]
async fn non_2xx_jsonrpc_body_is_bounded_without_content_length() {
    let mut body = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"data":""#.to_vec();
    body.resize(MAX_HTTP_RESPONSE_BYTES + 1, b'x');
    let (url, server) = spawn_chunked_response(
        "500 Internal Server Error",
        "application/json",
        body,
        64 * 1024,
    )
    .await;
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("connect");

    let error = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
    )
    .await
    .expect("oversized error response timed out")
    .expect_err("oversized JSON-RPC error body must be rejected");
    assert_response_too_large(error);
    server.await.expect("chunked server task");
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
async fn notify_rejects_oversized_chunked_ack_without_content_length() {
    let body = vec![b'n'; MAX_HTTP_RESPONSE_BYTES + 1];
    let (url, server) =
        spawn_chunked_response("202 Accepted", "application/octet-stream", body, 64 * 1024).await;
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("connect");

    let error = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
    )
    .await
    .expect("oversized notification response timed out")
    .expect_err("oversized notification acknowledgement must be rejected");
    assert_response_too_large(error);
    server.await.expect("chunked server task");
}

#[tokio::test]
async fn sse_response_ignores_notifications_and_returns_matching_response() {
    let server = MockServer::start_async().await;

    let response_body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(format!(
                    "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\n\
                     event: message\ndata: {response_body}\n\n"
                ));
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");

    let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let response = tokio::time::timeout(TEST_TIMEOUT, transport.request(request_body))
        .await
        .expect("request timed out")
        .expect("SSE response should be supported");
    assert_eq!(response, response_body);
    mock.assert_async().await;
}

#[tokio::test]
async fn sse_handles_bom_mixed_line_endings_and_one_byte_wire_chunks() {
    let body = concat!(
        "\u{feff}data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\r",
        ": keepalive\n\n",
        "data: {\"jsonrpc\":\"2.0\",\r\n",
        "data: \"id\":1,\"result\":{\"ok\":true}}\r\n\r\n"
    )
    .as_bytes()
    .to_vec();
    let (url, server) = spawn_chunked_response("200 OK", "text/event-stream", body, 1).await;
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("connect");

    let response = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
    )
    .await
    .expect("mixed-line-ending SSE response timed out")
    .expect("BOM and split CR/LF boundaries must be supported");
    let parsed: serde_json::Value =
        serde_json::from_str(&response).expect("multi-data-line response stays valid JSON");
    assert_eq!(parsed["id"], 1);
    assert_eq!(parsed["result"]["ok"], true);
    server.await.expect("chunked server task");
}

#[tokio::test]
async fn sse_rejects_oversized_chunked_event_without_content_length() {
    let mut body = b"event: message\ndata: ".to_vec();
    body.resize(MAX_HTTP_RESPONSE_BYTES + 1, b'x');
    let (url, server) =
        spawn_chunked_response("200 OK", "text/event-stream", body, 64 * 1024).await;
    let mut transport = HttpTransport::connect(&url, &loopback_guard())
        .await
        .expect("connect");

    let error = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
    )
    .await
    .expect("oversized SSE response timed out")
    .expect_err("oversized SSE response must be rejected");
    assert_response_too_large(error);
    server.await.expect("chunked server task");
}

#[tokio::test]
async fn stateless_request_includes_version_method_and_encoded_name_headers() {
    let server = MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", "=?base64?SGVsbG8sIOS4lueVjA==?=");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[]}}"#,
                );
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");
    transport.set_protocol_version("2026-07-28");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"Hello, 世界","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    transport.request(body).await.expect("request");
    mock.assert_async().await;
}

#[tokio::test]
async fn x_mcp_headers_are_cached_from_tools_list_and_mirrored() {
    let server = MockServer::start_async().await;
    let list = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("Mcp-Method", "tools/list");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","tools":[{
                        "name":"query","inputSchema":{"type":"object","properties":{
                            "region":{"type":"string","x-mcp-header":"Region"},
                            "priority":{"type":"integer","x-mcp-header":"Priority"},
                            "verbose":{"type":"boolean","x-mcp-header":"Verbose"},
                            "greeting":{"type":"string","x-mcp-header":"Greeting"}
                        }}}]}}"#,
                );
        })
        .await;
    let call = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/mcp")
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", "query")
                .header("Mcp-Param-Region", "us-west1")
                .header("Mcp-Param-Priority", "42")
                .header("Mcp-Param-Verbose", "true")
                .header("Mcp-Param-Greeting", "=?base64?SGVsbG8sIOS4lueVjA==?=");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[]}}"#,
                );
        })
        .await;

    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");
    transport.set_protocol_version("2026-07-28");
    let list_body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    transport.request(list_body).await.expect("tools/list");
    let call_body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"region":"us-west1","priority":42,"verbose":true,"greeting":"Hello, 世界"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    transport.request(call_body).await.expect("tools/call");
    list.assert_async().await;
    call.assert_async().await;
}

#[tokio::test]
async fn invalid_x_mcp_header_tool_is_excluded_without_hiding_valid_tools() {
    let server = MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/mcp");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","tools":[
                        {"name":"valid","inputSchema":{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}}},
                        {"name":"invalid","inputSchema":{"type":"object","properties":{"value":{"type":"object","x-mcp-header":"Object"}}}}
                    ]}}"#,
                );
        })
        .await;
    let guard = loopback_guard();
    let mut transport = HttpTransport::connect(server.url("/mcp"), &guard)
        .await
        .expect("connect failed");
    transport.set_protocol_version("2026-07-28");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let response = transport.request(body).await.expect("tools/list");
    assert!(response.contains("\"name\":\"valid\""), "{response}");
    assert!(!response.contains("\"name\":\"invalid\""), "{response}");
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
