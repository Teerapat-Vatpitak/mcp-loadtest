//! Integration tests for `SseTransport`.
//!
//! Each test stands up a tiny in-process HTTP/SSE server on a random port via
//! `tokio::net::TcpListener` and writes the HTTP framing by hand. That keeps
//! the test surface tight — no extra HTTP-mock dep — and lets us exercise
//! exact byte sequences for endpoint / message event handshakes.

use std::sync::Arc;
use std::time::Duration;

use mcp_loadtest_core::config::ServerConfig;
use mcp_loadtest_protocol::transport::sse::SseTransport;
use mcp_loadtest_protocol::transport::{HostGuard, Transport};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};

/// The mock SSE server binds to `127.0.0.1:0`, so every `connect` here dials a
/// loopback IP literal. The SSRF guard (ADR 0012) blocks loopback literals
/// unless they are explicitly in `allowed_hosts` (the operator escape hatch).
/// Mirror the permissive-guard construction used by `tests/host_guard.rs` and
/// the `ws.rs` unit tests' `allow_all`: a config whose `allowed_hosts` is
/// exactly `["127.0.0.1"]`, fed to `HostGuard::from_config`. This only threads
/// the guard so the transport compiles + connects; it does not change what any
/// test asserts about transport behavior.
fn loopback_guard() -> HostGuard {
    let mut cfg = ServerConfig::stdio("python".into(), vec![]);
    cfg.allowed_hosts = vec!["127.0.0.1".to_string()];
    HostGuard::from_config(&cfg)
}

/// A handle to a running mock SSE server. Send raw SSE event-stream chunks
/// (already wrapped as `event: ...\n data: ...\n\n`) into `events_tx` and the
/// server pumps them to the connected client.
struct MockServer {
    base_url: String,
    /// Channel of SSE-formatted chunks to push to the *connected* GET stream.
    events_tx: mpsc::UnboundedSender<String>,
    /// Receives the body of each POST. The test asserts on these.
    posts_rx: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
    /// Returns the status code the server should reply with to the next POST.
    /// Defaults to 202.
    post_status: Arc<Mutex<u16>>,
    /// Fires once the SSE GET handler has spawned (i.e. the client is on).
    _ready_rx: oneshot::Receiver<()>,
    _shutdown_tx: oneshot::Sender<()>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
        let (posts_tx, posts_rx) = mpsc::unbounded_channel::<String>();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let post_status = Arc::new(Mutex::new(202u16));
        let post_status_handle = post_status.clone();

        // One-shot wrapping so we can move the events_rx into the GET handler.
        let events_rx = Arc::new(Mutex::new(Some(events_rx)));
        // Same for ready_tx — used exactly once on first GET.
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let Ok((sock, _)) = accept else { break };
                        let events_rx = events_rx.clone();
                        let posts_tx = posts_tx.clone();
                        let ready_tx = ready_tx.clone();
                        let post_status = post_status_handle.clone();
                        tokio::spawn(async move {
                            let _ = handle_conn(
                                sock,
                                events_rx,
                                posts_tx,
                                ready_tx,
                                post_status,
                            )
                            .await;
                        });
                    }
                }
            }
        });

        Self {
            base_url,
            events_tx,
            posts_rx: Arc::new(Mutex::new(posts_rx)),
            post_status,
            _ready_rx: ready_rx,
            _shutdown_tx: shutdown_tx,
        }
    }

    fn sse_url(&self) -> String {
        format!("{}/sse", self.base_url)
    }

    /// Push an `endpoint` event to the connected GET stream.
    fn send_endpoint(&self, post_path: &str) {
        let _ = self
            .events_tx
            .send(format!("event: endpoint\ndata: {post_path}\n\n"));
    }

    /// Push a `message` event with the given raw JSON body.
    fn send_message(&self, json_body: &str) {
        let _ = self
            .events_tx
            .send(format!("event: message\ndata: {json_body}\n\n"));
    }

    /// Block until a POST body arrives. Panics if the channel closes.
    async fn next_post(&self) -> String {
        self.posts_rx
            .lock()
            .await
            .recv()
            .await
            .expect("server posts channel closed")
    }

    /// Configure the status code the server replies with to the next POST.
    async fn set_post_status(&self, code: u16) {
        *self.post_status.lock().await = code;
    }
}

async fn handle_conn(
    sock: TcpStream,
    events_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
    posts_tx: mpsc::UnboundedSender<String>,
    ready_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    post_status: Arc<Mutex<u16>>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = sock.into_split();
    let mut reader = BufReader::new(read_half);

    // Parse the request line.
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line).await?;
    if n == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let _path = parts.next().unwrap_or("/").to_string();

    // Drain headers, capture Content-Length for POSTs.
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    match method.as_str() {
        "GET" => {
            // Open the SSE response.
            let headers = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";
            write_half.write_all(headers.as_bytes()).await?;
            write_half.flush().await?;

            // Signal "client connected" exactly once.
            if let Some(tx) = ready_tx.lock().await.take() {
                let _ = tx.send(());
            }

            // Pull from the events channel and write to the wire.
            let mut rx_guard = events_rx.lock().await;
            if let Some(mut rx) = rx_guard.take() {
                drop(rx_guard);
                while let Some(chunk) = rx.recv().await {
                    if write_half.write_all(chunk.as_bytes()).await.is_err() {
                        break;
                    }
                    if write_half.flush().await.is_err() {
                        break;
                    }
                }
            }
        }
        "POST" => {
            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).await?;
            let body = String::from_utf8_lossy(&buf).to_string();
            let _ = posts_tx.send(body);

            let code = *post_status.lock().await;
            let reason = match code {
                200 => "OK",
                202 => "Accepted",
                400 => "Bad Request",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let resp = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            write_half.write_all(resp.as_bytes()).await?;
            write_half.flush().await?;
        }
        _ => {
            let resp =
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            write_half.write_all(resp.as_bytes()).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn connect_handshakes_endpoint_event() {
    let server = MockServer::start().await;
    let sse_url = server.sse_url();

    // Send the endpoint event *before* connect so the transport sees it
    // immediately. The channel is unbounded so this is fine to queue early.
    server.send_endpoint("/rpc");

    let guard = loopback_guard();
    let transport = SseTransport::connect(&sse_url, &guard)
        .await
        .expect("connect succeeds with valid endpoint event");

    // Use shutdown to release the reader cleanly.
    Box::new(transport).shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn request_correlates_by_id() {
    let server = MockServer::start().await;
    let sse_url = server.sse_url();

    server.send_endpoint("/rpc");

    let guard = loopback_guard();
    let mut transport = SseTransport::connect(&sse_url, &guard)
        .await
        .expect("connect");

    let request_body = r#"{"jsonrpc":"2.0","id":42,"method":"tools/list","params":{}}"#;

    // Drive the request + correlated response concurrently. The mock server
    // emits the response *after* the client's POST arrives, mimicking real
    // MCP servers where the response is keyed to the request.
    let request_task = tokio::spawn({
        async move {
            let body = transport.request(request_body).await.expect("request ok");
            (transport, body)
        }
    });

    // Wait for the POST then push the matching response.
    let posted = server.next_post().await;
    assert!(
        posted.contains("\"id\":42"),
        "POST body should be ours: {posted}"
    );

    server.send_message(r#"{"jsonrpc":"2.0","id":42,"result":{"tools":[]}}"#);

    let (transport, body) = tokio::time::timeout(Duration::from_secs(5), request_task)
        .await
        .expect("request_task didn't finish in time")
        .expect("request_task panicked");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("response is json");
    assert_eq!(parsed["id"], 42);
    assert_eq!(parsed["result"]["tools"], serde_json::json!([]));

    Box::new(transport).shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn non_2xx_on_post_returns_http_error() {
    let server = MockServer::start().await;
    let sse_url = server.sse_url();
    server.send_endpoint("/rpc");

    let guard = loopback_guard();
    let mut transport = SseTransport::connect(&sse_url, &guard)
        .await
        .expect("connect");
    server.set_post_status(500).await;

    let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
    let err = transport
        .request(request_body)
        .await
        .expect_err("expected HTTP 500 to surface");
    let msg = err.to_string();
    assert!(
        msg.contains("500") || msg.to_ascii_lowercase().contains("http"),
        "error should mention 500 / http; got: {msg}",
    );

    Box::new(transport).shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn shutdown_cancels_reader_task() {
    let server = MockServer::start().await;
    let sse_url = server.sse_url();
    server.send_endpoint("/rpc");

    let guard = loopback_guard();
    let transport = SseTransport::connect(&sse_url, &guard)
        .await
        .expect("connect");

    // Connect + immediate shutdown: must not hang.
    tokio::time::timeout(Duration::from_secs(5), Box::new(transport).shutdown())
        .await
        .expect("shutdown didn't return promptly")
        .expect("shutdown errored");
}

#[tokio::test]
async fn drop_without_shutdown_does_not_hang() {
    // If a caller drops the transport without going through `shutdown` (e.g.
    // panic, early return), the background reader must still wind down so the
    // test runtime can exit cleanly.
    let server = MockServer::start().await;
    let sse_url = server.sse_url();
    server.send_endpoint("/rpc");

    let guard = loopback_guard();
    tokio::time::timeout(Duration::from_secs(5), async {
        let transport = SseTransport::connect(&sse_url, &guard)
            .await
            .expect("connect");
        drop(transport);
    })
    .await
    .expect("drop-without-shutdown blocked");
}
