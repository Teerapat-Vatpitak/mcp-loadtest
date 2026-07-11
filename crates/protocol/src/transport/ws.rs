//! WebSocket transport — one JSON-RPC message per text frame.
//!
//! Architecture mirrors [`super::sse::SseTransport`]: a background reader task
//! owns the `SplitStream` half and forwards decoded payloads via mpsc; the
//! foreground owns the `SplitSink` and a [`VecDeque`] for id-mismatched
//! frames so server-initiated notifications can interleave with responses.
//!
//! Frame size cap mirrors stdio's `MAX_LINE_BYTES` (16 MB).

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::guard::HostGuard;
use super::{Transport, TransportError, resolve};

/// Per-request response budget — matches stdio / SSE so wedged servers
/// surface consistently across transports.
const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_secs(60);
/// Reader → foreground channel depth. Generous to absorb bursty servers.
const READER_CHANNEL_CAP: usize = 64;
/// Graceful-shutdown budget for both Close-frame send and reader join.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-frame OOM guard — same value/rationale as [`super::stdio`]'s
/// `MAX_LINE_BYTES`. Real MCP messages are < 1 MB.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Cap on the id-mismatch buffer. See [`super::sse`]'s identical constant for
/// rationale. On overflow we surface [`TransportError::Other`].
const MAX_PENDING_FRAMES: usize = 256;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// WebSocket transport. See module docs.
#[derive(Debug)]
pub struct WsTransport {
    /// Outbound half of the split WS connection.
    sink: SplitSink<WsStream, Message>,
    incoming: mpsc::Receiver<Result<String, TransportError>>,
    reader_task: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    /// Frames whose `id` didn't match the in-flight request — drained on next
    /// `request` before reading from `incoming`.
    pending: VecDeque<String>,
}

/// Peel the JSON-RPC `id` out of a frame; `None` = notification or malformed.
#[derive(Deserialize)]
struct IdProbe {
    id: Option<serde_json::Value>,
}

impl WsTransport {
    /// Open a WS connection to `url`. Scheme must be `ws://` or `wss://`;
    /// other schemes return [`TransportError::Other`]. Spawns a background
    /// reader task; Ping/Pong are auto-handled by `tokio-tungstenite`.
    ///
    /// `guard` enforces the SSRF host-allowlist + private-IP-literal block
    /// (ADR 0012) and the resolved-address block (ADR 0016) against the
    /// parsed URL before the socket is dialed. The TCP socket is dialed to a
    /// *vetted* address ourselves (resolver pinning — the checked IP is the
    /// dialed IP); the WS/TLS handshake then runs with the original URL so
    /// TLS SNI and the `Host` header keep the hostname.
    pub async fn connect(url: &str, guard: &HostGuard) -> Result<Self, TransportError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| TransportError::Other(format!("invalid ws url: {e}")))?;
        match parsed.scheme() {
            "ws" | "wss" => {}
            other => {
                return Err(TransportError::Other(format!(
                    "ws transport: expected ws:// or wss:// scheme, got `{other}://`"
                )));
            }
        }
        let addrs = resolve::resolve_and_check(&parsed, guard).await?;
        let host = parsed.host_str().unwrap_or("<no host>");

        let tcp = resolve::dial_pinned(host, &addrs).await?;
        let (ws_stream, _resp) = tokio_tungstenite::client_async_tls(url, tcp)
            .await
            .map_err(|e| TransportError::Other(format!("ws connect: {e}")))?;

        let (sink, stream) = ws_stream.split();
        let (tx, rx) = mpsc::channel::<Result<String, TransportError>>(READER_CHANNEL_CAP);
        let cancel = CancellationToken::new();
        let reader_task = spawn_reader(stream, tx, cancel.clone());

        Ok(Self {
            sink,
            incoming: rx,
            reader_task: Some(reader_task),
            cancel,
            pending: VecDeque::new(),
        })
    }

    /// Send `body` as a single Text frame.
    async fn send_text(&mut self, body: &str) -> Result<(), TransportError> {
        self.sink
            .send(Message::Text(body.to_string().into()))
            .await
            .map_err(|e| TransportError::Other(format!("ws send: {e}")))
    }
}

/// Peel the JSON-RPC `id` out of a frame. `None` = notification or malformed.
fn extract_id(frame: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<IdProbe>(frame)
        .ok()
        .and_then(|p| p.id)
}

/// Decode one WS message into the form the foreground transport consumes.
///
/// - `Some(Ok(s))` — forward this JSON-RPC frame to the consumer.
/// - `Some(Err(e))` — terminal error; reader task should drain & exit.
/// - `None` — skip this message (Ping/Pong/raw Frame); keep reading.
///
/// Splitting this out keeps `spawn_reader` short enough to fit the < 300-line
/// per-file convention while making the size-cap rule one place to audit.
fn decode_message(msg: Message) -> Option<Result<String, TransportError>> {
    match msg {
        Message::Text(t) => {
            if t.len() > MAX_FRAME_BYTES {
                return Some(Err(TransportError::Other(format!(
                    "ws transport: text frame {} bytes exceeds {MAX_FRAME_BYTES}; \
                     aborting to avoid OOM",
                    t.len()
                ))));
            }
            Some(Ok(t.to_string()))
        }
        Message::Binary(b) => {
            if b.len() > MAX_FRAME_BYTES {
                return Some(Err(TransportError::Other(format!(
                    "ws transport: binary frame {} bytes exceeds {MAX_FRAME_BYTES}; \
                     aborting to avoid OOM",
                    b.len()
                ))));
            }
            // Tolerate JSON-RPC carried as binary; invalid UTF-8 is a hard
            // error rather than silently lossy-decoding into garbage.
            match std::str::from_utf8(b.as_ref()) {
                Ok(s) => Some(Ok(s.to_string())),
                Err(e) => Some(Err(TransportError::Other(format!(
                    "ws transport: binary frame not valid utf-8: {e}"
                )))),
            }
        }
        Message::Close(_) => Some(Err(TransportError::Closed)),
        // Ping/Pong are handled inside tokio-tungstenite (auto-pong); raw
        // Frame is internal-only and never surfaces here in practice.
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => None,
    }
}

/// Spawn the reader task. Cancellable via `cancel`; surfaces inbound frames
/// (or terminal errors) through `tx`.
fn spawn_reader(
    mut stream: SplitStream<WsStream>,
    tx: mpsc::Sender<Result<String, TransportError>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => break,
                next = stream.next() => next,
            };
            let outcome = match next {
                Some(Ok(msg)) => decode_message(msg),
                Some(Err(e)) => Some(Err(TransportError::Other(format!("ws read: {e}")))),
                None => Some(Err(TransportError::Closed)),
            };
            match outcome {
                None => continue,
                Some(Ok(frame)) => {
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    // Every error path here is terminal (Closed, size cap,
                    // invalid utf-8, or wrapped read error). Forward and exit.
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    })
}

#[async_trait]
impl Transport for WsTransport {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        let expected_id = extract_id(body);
        self.send_text(body).await?;

        // Already-buffered out-of-order frame? Take it before reading more.
        if let Some(idx) = self
            .pending
            .iter()
            .position(|f| extract_id(f) == expected_id)
        {
            return Ok(self.pending.remove(idx).expect("position just returned"));
        }

        loop {
            let next = tokio::time::timeout(DEFAULT_RECV_TIMEOUT, self.incoming.recv())
                .await
                .map_err(|_| TransportError::Timeout(DEFAULT_RECV_TIMEOUT))?;
            match next {
                Some(Ok(frame)) => {
                    if extract_id(&frame) == expected_id {
                        return Ok(frame);
                    }
                    // Stash; could be a server-initiated notification. Cap
                    // the buffer to defend against a flooding server — see
                    // `MAX_PENDING_FRAMES`.
                    if self.pending.len() >= MAX_PENDING_FRAMES {
                        return Err(TransportError::Other(format!(
                            "ws transport: pending id-mismatch buffer hit {MAX_PENDING_FRAMES} frames \
                             without matching response — server is misbehaving (flooding notifications?)"
                        )));
                    }
                    self.pending.push_back(frame);
                }
                Some(Err(e)) => return Err(e),
                None => return Err(TransportError::Closed),
            }
        }
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        self.send_text(body).await
    }

    async fn shutdown(mut self: Box<Self>) -> Result<(), TransportError> {
        // Best-effort Close frame; ignore send errors (peer may already be
        // gone), then cancel the reader and wait briefly for it to drain.
        let _ =
            tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, self.sink.send(Message::Close(None))).await;
        let _ = tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, self.sink.close()).await;
        self.cancel.cancel();
        if let Some(handle) = self.reader_task.take() {
            // Best-effort drain; we already cancelled, so a slow reader just
            // gets aborted via the timeout arm rather than blocking shutdown.
            let _ = tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, handle).await;
        }
        Ok(())
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        // Tear down the reader task if the caller skipped `shutdown`.
        self.cancel.cancel();
        if let Some(handle) = self.reader_task.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_loadtest_core::config::ServerConfig;
    use tokio::net::TcpListener;

    /// A permissive guard (empty allowlist => any public/loopback literal
    /// passes? no — loopback literals still need an escape-hatch entry, so
    /// these tests that dial `ws://127.0.0.1:<port>` list `127.0.0.1`).
    fn allow_all() -> HostGuard {
        let mut cfg = ServerConfig::stdio("x".into(), vec![]);
        cfg.allowed_hosts = vec!["127.0.0.1".to_string()];
        HostGuard::from_config(&cfg)
    }

    /// Spin up a one-shot echo server. Returns the URL the client should dial.
    async fn spawn_echo_server() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sink, mut src) = ws.split();
            while let Some(Ok(msg)) = src.next().await {
                match msg {
                    Message::Text(t) => {
                        if sink.send(Message::Text(t)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn roundtrip_request_and_shutdown() {
        let (url, _srv) = spawn_echo_server().await;
        let transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let resp = boxed.request(body).await.expect("request");
        assert_eq!(resp, body);
        boxed.shutdown().await.expect("shutdown");
    }

    /// Build a guard whose `allowed_hosts` is exactly `hosts`.
    fn guard_with(hosts: &[&str]) -> HostGuard {
        let mut cfg = ServerConfig::stdio("x".into(), vec![]);
        cfg.allowed_hosts = hosts.iter().map(|s| s.to_string()).collect();
        HostGuard::from_config(&cfg)
    }

    #[tokio::test]
    async fn hostname_url_resolves_pins_and_round_trips() {
        // `localhost` resolves via the hosts file / OS stack — no external
        // DNS. It must be allowlisted (escape hatch) since it resolves to
        // loopback (ADR 0016). The echo server listens on 127.0.0.1 only, so
        // a resolver that returns `::1` first also exercises the pinned-dial
        // fallback across vetted addresses.
        let (url, _srv) = spawn_echo_server().await;
        let host_url = url.replace("127.0.0.1", "localhost");
        let transport = WsTransport::connect(&host_url, &guard_with(&["localhost"]))
            .await
            .expect("hostname connect via resolver pinning");
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let resp = boxed.request(body).await.expect("request");
        assert_eq!(resp, body);
        boxed.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn hostname_resolving_private_is_blocked_before_dial() {
        // Empty allowlist: `localhost` resolves to loopback, so the ADR 0016
        // resolver layer must reject before any socket is dialed (port 1 is
        // never touched).
        let err = WsTransport::connect("ws://localhost:1/", &guard_with(&[]))
            .await
            .expect_err("loopback-resolving hostname must be blocked");
        let TransportError::Other(m) = err else {
            panic!("expected TransportError::Other, got {err:?}");
        };
        assert!(m.contains("blocked host"), "stable substring missing: {m}");
        assert!(m.contains("ADR 0016"), "ADR 0016 marker missing: {m}");
    }

    #[tokio::test]
    async fn rejects_non_ws_scheme() {
        // Scheme is checked before the SSRF guard, so a non-ws URL surfaces
        // the scheme error regardless of the guard.
        let err = WsTransport::connect("http://example.invalid/", &allow_all())
            .await
            .expect_err("should reject http scheme");
        assert!(matches!(err, TransportError::Other(ref m) if m.contains("scheme")));
    }

    /// Accept one connection, read one message, send a Close frame without
    /// any response. Lets us drive the "server vanishes mid-call" path.
    async fn spawn_close_after_one_read_server() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sink, mut src) = ws.split();
            // Read one frame then close without responding.
            let _ = src.next().await;
            let _ = sink.send(Message::Close(None)).await;
            let _ = sink.close().await;
        });
        (url, handle)
    }

    /// Accept the connection, then hang — never read, never write. Used to
    /// verify cancellation unblocks an in-flight request.
    async fn spawn_hanging_server() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(_ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            // Hold the connection open without doing anything. Sleep long
            // enough to outlast any reasonable test deadline.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        (url, handle)
    }

    /// Accept one connection and reply to the first frame with a Text frame
    /// of `payload_len` bytes. Used for the oversized-frame test.
    async fn spawn_oversized_response_server(payload_len: usize) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sink, mut src) = ws.split();
            // Wait for the client's request, then ship the huge reply.
            let _ = src.next().await;
            let huge = "a".repeat(payload_len);
            let _ = sink.send(Message::Text(huge.into())).await;
            // Keep the socket open briefly so the client can surface the
            // oversize error before we tear the connection down.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        (url, handle)
    }

    #[tokio::test]
    async fn server_closes_mid_call_surfaces_transport_closed() {
        let (url, _srv) = spawn_close_after_one_read_server().await;
        let transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let result = tokio::time::timeout(Duration::from_secs(2), boxed.request(body))
            .await
            .expect("request did not complete before timeout");
        let err = result.expect_err("request should fail when server closes mid-call");
        // Either `Closed` (clean Close frame surfaced by decode_message) or
        // an `Other` wrapping a read error from the underlying socket — both
        // signal the same "peer went away" condition.
        assert!(
            matches!(err, TransportError::Closed)
                || matches!(err, TransportError::Other(ref m) if m.contains("read") || m.contains("close")),
            "expected Closed or read/close Other, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cancel_during_request_unblocks() {
        let (url, _srv) = spawn_hanging_server().await;
        let transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        // Stash the CancellationToken so we can trip it from outside the
        // request future — equivalent to what `Drop` does, but without
        // having to drop the transport while a borrowed future is alive.
        let cancel = transport.cancel.clone();
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });

        let result = tokio::time::timeout(Duration::from_secs(2), boxed.request(body))
            .await
            .expect("cancel should unblock the request before the 2s deadline");
        let _ = canceller.await;

        // Cancelling the reader closes the mpsc sender, so the request loop
        // observes `None` and surfaces `Closed`. Anything other than a hang
        // satisfies the test, but assert on the error variant for clarity.
        let err = result.expect_err("cancelled request should not return Ok");
        assert!(
            matches!(err, TransportError::Closed) || matches!(err, TransportError::Other(_)),
            "expected Closed or Other after cancel, got {err:?}"
        );
    }

    #[tokio::test]
    async fn oversized_text_frame_returns_other_error() {
        let payload_len = MAX_FRAME_BYTES + 1024;
        let (url, _srv) = spawn_oversized_response_server(payload_len).await;
        let transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let result = tokio::time::timeout(Duration::from_secs(3), boxed.request(body))
            .await
            .expect("oversize handling should resolve before timeout");
        let err = result.expect_err("oversized frame should fail the request");
        // Two valid outcomes, both proving the size cap is enforced:
        //   1. `decode_message` surfaces our own "exceeds" message after the
        //      frame is delivered to us intact.
        //   2. tokio-tungstenite's own max_message_size (~64 MiB default) lets
        //      a 17 MiB frame through, but its max_frame_size (16 MiB) may
        //      reject during reassembly, surfacing as `ws read: ...` with
        //      tungstenite's own size-related wording. Accept either.
        let TransportError::Other(msg) = err else {
            panic!("expected TransportError::Other, got {err:?}");
        };
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("exceeds")
                || lower.contains("size")
                || lower.contains("too large")
                || lower.contains("too long"),
            "expected size-related error, got: {msg}"
        );
    }
}
