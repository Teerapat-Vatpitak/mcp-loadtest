//! WebSocket transport — one JSON-RPC message per text frame.
//!
//! Architecture mirrors [`super::sse::SseTransport`]: a background reader task
//! owns the `SplitStream` half and forwards decoded payloads via mpsc; the
//! foreground owns the `SplitSink` and a [`VecDeque`] for id-mismatched
//! frames so server-initiated notifications can interleave with responses.
//!
//! Frame size cap mirrors stdio's `MAX_LINE_BYTES` (16 MB).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use mcp_loadtest_core::config::validate_remote_endpoint;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::guard::HostGuard;
use super::headers::RemoteHeaders;
use super::{Transport, TransportError, resolve};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Per-request response budget — matches stdio / SSE so wedged servers
/// surface consistently across transports.
const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_secs(60);
/// Reader → foreground channel depth. Generous to absorb bursty servers.
///
/// This is only a scheduling bound. [`MAX_BUFFERED_BYTES`] is the memory
/// safety bound shared by this channel and `WsTransport::pending`.
const READER_CHANNEL_CAP: usize = 64;
/// Per-phase WebSocket shutdown budget. Send, sink close, natural reader
/// drain, cancellation, and abort confirmation are each bounded; the 10s
/// worst case stays below the
/// engine's 15s outer lifecycle guard.
const SHUTDOWN_PHASE_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-frame OOM guard — same value/rationale as [`super::stdio`]'s
/// `MAX_LINE_BYTES`. Real MCP messages are < 1 MB.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Aggregate retained-frame budget across the reader channel and the
/// id-mismatch queue. Each reservation includes the String allocation and
/// per-frame storage, so empty/tiny-frame floods cannot bypass the bound.
const MAX_BUFFERED_BYTES: usize = 32 * 1024 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const READER_RUNNING: u8 = 0;
const READER_PEER_CLOSED: u8 = 1;
const READER_FAILED: u8 = 2;

/// Secret-safe terminal reader latch. Detailed errors still travel through
/// `incoming` to the active request; this shared state only carries a fixed
/// classification so teardown cannot miss a failure that arrived after a
/// matching response was returned.
#[derive(Debug, Default)]
struct ReaderTerminal {
    state: AtomicU8,
    local_close_sent: AtomicBool,
}

impl ReaderTerminal {
    fn mark_local_close_sent(&self) {
        self.local_close_sent.store(true, Ordering::Release);
    }

    fn record_close_handshake(&self) {
        let state = if self.local_close_sent.load(Ordering::Acquire) {
            READER_PEER_CLOSED
        } else {
            // A peer-initiated Close is terminal evidence, even when a
            // complete response happened to precede it.
            READER_FAILED
        };
        let _ =
            self.state
                .compare_exchange(READER_RUNNING, state, Ordering::AcqRel, Ordering::Acquire);
    }

    fn record_failure(&self) {
        let _ = self.state.compare_exchange(
            READER_RUNNING,
            READER_FAILED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn check_failure(&self) -> Result<(), TransportError> {
        if self.state.load(Ordering::Acquire) == READER_FAILED {
            Err(TransportError::Other(
                "ws transport: reader terminated after an inbound protocol or socket error".into(),
            ))
        } else {
            // Only a close handshake observed after our Close was sent reaches
            // READER_PEER_CLOSED. EOF, reset-without-close, unsolicited Close,
            // and every other terminal reader condition are failures.
            Ok(())
        }
    }
}

#[derive(Debug)]
struct BufferBudget {
    used: AtomicUsize,
    exhausted: AtomicBool,
    limit: usize,
}

impl BufferBudget {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            exhausted: AtomicBool::new(false),
            limit,
        })
    }

    fn try_reserve(self: &Arc<Self>, frame: String) -> Result<BufferedFrame, TransportError> {
        let Some(bytes) = frame
            .capacity()
            .checked_add(std::mem::size_of::<Result<BufferedFrame, TransportError>>())
        else {
            self.exhausted.store(true, Ordering::Release);
            return Err(self.exhaustion_error());
        };
        let reserved = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            });
        if reserved.is_err() {
            self.exhausted.store(true, Ordering::Release);
            return Err(self.exhaustion_error());
        }
        Ok(BufferedFrame {
            frame: Some(frame),
            bytes,
            budget: Arc::clone(self),
        })
    }

    fn exhaustion_error(&self) -> TransportError {
        TransportError::Other(format!(
            "ws transport: aggregate buffered-frame budget exceeded {} bytes",
            self.limit
        ))
    }

    fn check(&self) -> Result<(), TransportError> {
        if self.exhausted.load(Ordering::Acquire) {
            Err(self.exhaustion_error())
        } else {
            Ok(())
        }
    }

    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct BufferedFrame {
    frame: Option<String>,
    bytes: usize,
    budget: Arc<BufferBudget>,
}

impl BufferedFrame {
    fn text(&self) -> &str {
        self.frame.as_deref().expect("buffered frame is present")
    }

    fn into_string(mut self) -> String {
        let frame = self.frame.take().expect("buffered frame is present");
        self.budget.release(self.bytes);
        self.bytes = 0;
        frame
    }
}

impl Drop for BufferedFrame {
    fn drop(&mut self) {
        if self.frame.is_some() {
            self.budget.release(self.bytes);
        }
    }
}

/// WebSocket transport. See module docs.
#[derive(Debug)]
pub struct WsTransport {
    /// Outbound half of the split WS connection.
    sink: SplitSink<WsStream, Message>,
    incoming: mpsc::Receiver<Result<BufferedFrame, TransportError>>,
    reader_task: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    buffer_budget: Arc<BufferBudget>,
    reader_terminal: Arc<ReaderTerminal>,
    /// Frames whose `id` didn't match the in-flight request — drained on next
    /// `request` before reading from `incoming`.
    pending: VecDeque<BufferedFrame>,
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
        Self::connect_with_headers(url, guard, RemoteHeaders::default()).await
    }

    /// Connect with static outbound headers loaded from environment
    /// variables. WebSocket has one HTTP upgrade request, so the headers are
    /// applied to that handshake.
    pub async fn connect_with_headers(
        url: &str,
        guard: &HostGuard,
        remote_headers: RemoteHeaders,
    ) -> Result<Self, TransportError> {
        let parsed = validate_remote_endpoint(url, "ws", !remote_headers.is_empty())
            .map_err(TransportError::Other)?;
        let addrs = resolve::resolve_and_check(&parsed, guard).await?;
        let host = parsed.host_str().unwrap_or("<no host>");

        let tcp = resolve::dial_pinned(host, &addrs).await?;
        let mut request = parsed
            .as_str()
            .into_client_request()
            .map_err(|_| TransportError::Other("failed to build WebSocket request".into()))?;
        for (name, value) in remote_headers.iter() {
            request.headers_mut().insert(name.clone(), value.clone());
        }
        // Set tungstenite's message limit, not just our post-decode check:
        // fragmented messages are rejected while their payload is being
        // accumulated rather than after a >16 MiB Text allocation exists.
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let (ws_stream, _resp) =
            tokio_tungstenite::client_async_tls_with_config(request, tcp, Some(ws_config), None)
                .await
                .map_err(|_| TransportError::Other("WebSocket connect failed".into()))?;

        let (sink, stream) = ws_stream.split();
        let (tx, rx) = mpsc::channel::<Result<BufferedFrame, TransportError>>(READER_CHANNEL_CAP);
        let cancel = CancellationToken::new();
        let buffer_budget = BufferBudget::new(MAX_BUFFERED_BYTES);
        let reader_terminal = Arc::new(ReaderTerminal::default());
        let reader_task = spawn_reader(
            stream,
            tx,
            cancel.clone(),
            Arc::clone(&buffer_budget),
            Arc::clone(&reader_terminal),
        );

        Ok(Self {
            sink,
            incoming: rx,
            reader_task: Some(reader_task),
            cancel,
            buffer_budget,
            reader_terminal,
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
    tx: mpsc::Sender<Result<BufferedFrame, TransportError>>,
    cancel: CancellationToken,
    buffer_budget: Arc<BufferBudget>,
    reader_terminal: Arc<ReaderTerminal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => break,
                next = stream.next() => next,
            };
            let (outcome, close_handshake) = match next {
                Some(Ok(Message::Close(_))) => (Some(Err(TransportError::Closed)), true),
                Some(Ok(msg)) => (decode_message(msg), false),
                Some(Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed)) => {
                    (Some(Err(TransportError::Closed)), true)
                }
                Some(Err(e)) => (
                    Some(Err(TransportError::Other(format!("ws read: {e}")))),
                    false,
                ),
                None => (Some(Err(TransportError::Closed)), false),
            };
            match outcome {
                None => continue,
                Some(Ok(frame)) => match buffer_budget.try_reserve(frame) {
                    Ok(frame) => {
                        if tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        reader_terminal.record_failure();
                        let _ = tx.send(Err(error)).await;
                        break;
                    }
                },
                Some(Err(e)) => {
                    // Every error path here is terminal (Closed, size cap,
                    // invalid utf-8, or wrapped read error). Forward and exit.
                    if close_handshake {
                        reader_terminal.record_close_handshake();
                    } else {
                        reader_terminal.record_failure();
                    }
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
        self.buffer_budget.check()?;
        self.reader_terminal.check_failure()?;
        self.send_text(body).await?;
        self.buffer_budget.check()?;
        self.reader_terminal.check_failure()?;

        // Already-buffered out-of-order frame? Take it before reading more.
        if let Some(idx) = self
            .pending
            .iter()
            .position(|f| extract_id(f.text()) == expected_id)
        {
            self.buffer_budget.check()?;
            self.reader_terminal.check_failure()?;
            let response = self
                .pending
                .remove(idx)
                .expect("position just returned")
                .into_string();
            self.buffer_budget.check()?;
            self.reader_terminal.check_failure()?;
            return Ok(response);
        }

        loop {
            let next = tokio::time::timeout(DEFAULT_RECV_TIMEOUT, self.incoming.recv())
                .await
                .map_err(|_| TransportError::Timeout(DEFAULT_RECV_TIMEOUT))?;
            match next {
                Some(Ok(frame)) => {
                    self.buffer_budget.check()?;
                    self.reader_terminal.check_failure()?;
                    if extract_id(frame.text()) == expected_id {
                        let response = frame.into_string();
                        // Re-check after releasing the matching frame. The
                        // reader may have observed a later terminal frame
                        // while this task was correlating the response.
                        self.buffer_budget.check()?;
                        self.reader_terminal.check_failure()?;
                        return Ok(response);
                    }
                    // Stash; could be a server-initiated notification. Its
                    // reservation moves with it, so reader-channel plus
                    // pending bytes remain under one aggregate budget.
                    self.pending.push_back(frame);
                }
                Some(Err(e)) => return Err(e),
                None => return Err(TransportError::Closed),
            }
        }
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        self.buffer_budget.check()?;
        self.reader_terminal.check_failure()?;
        self.send_text(body).await?;
        self.buffer_budget.check()?;
        self.reader_terminal.check_failure()
    }

    async fn shutdown(mut self: Box<Self>) -> Result<(), TransportError> {
        let mut failures = Vec::new();

        match tokio::time::timeout(SHUTDOWN_PHASE_TIMEOUT, self.sink.send(Message::Close(None)))
            .await
        {
            Ok(Ok(())) => self.reader_terminal.mark_local_close_sent(),
            Ok(Err(error)) => failures.push(format!("close-frame send failed: {error}")),
            Err(_) => failures.push(format!(
                "close-frame send exceeded {SHUTDOWN_PHASE_TIMEOUT:?}"
            )),
        }
        match tokio::time::timeout(SHUTDOWN_PHASE_TIMEOUT, self.sink.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("sink close failed: {error}")),
            Err(_) => failures.push(format!("sink close exceeded {SHUTDOWN_PHASE_TIMEOUT:?}")),
        }

        if let Some(handle) = self.reader_task.as_mut() {
            // Let the reader consume everything ordered before the peer's
            // Close response. Cancelling first could hide a post-response
            // flood and turn teardown falsely green.
            match tokio::time::timeout(SHUTDOWN_PHASE_TIMEOUT, &mut *handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("reader task failed: {error}")),
                Err(_) => {
                    self.cancel.cancel();
                    match tokio::time::timeout(SHUTDOWN_PHASE_TIMEOUT, &mut *handle).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            failures
                                .push(format!("reader task failed after cancellation: {error}"));
                        }
                        Err(_) => {
                            // Retain ownership while aborting so cancellation
                            // of this shutdown future still reaches Drop's
                            // backstop instead of detaching the JoinHandle.
                            handle.abort();
                            failures.push(format!(
                                "reader task cancellation exceeded {SHUTDOWN_PHASE_TIMEOUT:?}"
                            ));
                            match tokio::time::timeout(SHUTDOWN_PHASE_TIMEOUT, &mut *handle).await {
                                Ok(Err(error)) if error.is_cancelled() => {}
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => {
                                    failures.push(format!("reader task abort failed: {error}"));
                                }
                                Err(_) => failures.push(format!(
                                    "reader task abort confirmation exceeded \
                                     {SHUTDOWN_PHASE_TIMEOUT:?}"
                                )),
                            }
                        }
                    }
                }
            }
            self.reader_task.take();
        }

        // A response can be correlated immediately before the reader sees a
        // later flood/error. Teardown is the final pass/fail barrier.
        if let Err(error) = self.buffer_budget.check() {
            failures.push(error.to_string());
        } else if let Err(error) = self.reader_terminal.check_failure() {
            failures.push(error.to_string());
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(TransportError::Other(format!(
                "WebSocket shutdown incomplete: {}",
                failures.join("; ")
            )))
        }
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
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::protocol::frame::Frame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

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
            // Flush tungstenite's automatically queued Close reply. Dropping
            // immediately would produce ResetWithoutClosingHandshake and is
            // intentionally treated as uncertain teardown by the client.
            let _ = sink.close().await;
        });
        (url, handle)
    }

    #[tokio::test]
    async fn roundtrip_request_and_shutdown() {
        let (url, _srv) = spawn_echo_server().await;
        let mut transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let resp = transport.request(body).await.expect("request");
        assert_eq!(resp, body);
        assert_eq!(
            transport.buffer_budget.used.load(Ordering::Acquire),
            0,
            "matching response releases its reader-channel reservation"
        );
        Box::new(transport).shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn connect_failure_does_not_echo_endpoint_query() {
        const SECRET: &str = "ws-query-secret-sentinel";
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve port");
        let addr = listener.local_addr().expect("reserved address");
        drop(listener);

        let url = format!("ws://{addr}/socket?ticket={SECRET}&tenant=private");
        let err = WsTransport::connect(&url, &allow_all())
            .await
            .expect_err("closed port must fail");
        let diagnostic = err.to_string();
        assert!(
            !diagnostic.contains(SECRET)
                && !diagnostic.contains("ticket")
                && !diagnostic.contains("tenant=private"),
            "WebSocket error leaked endpoint query: {diagnostic}"
        );
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

    /// Send one oversized text message as several individually valid frames.
    /// This specifically exercises tungstenite's accumulation-time message
    /// limit rather than the post-decode guard.
    async fn spawn_fragmented_oversized_server(
        fragment_len: usize,
        fragment_count: usize,
    ) -> (String, JoinHandle<()>) {
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
            let _ = src.next().await;
            for index in 0..fragment_count {
                let opcode = if index == 0 {
                    OpCode::Data(Data::Text)
                } else {
                    OpCode::Data(Data::Continue)
                };
                let is_final = index + 1 == fragment_count;
                let frame = Frame::message(vec![b'a'; fragment_len], opcode, is_final);
                if sink.send(Message::Frame(frame)).await.is_err() {
                    break;
                }
            }
        });
        (url, handle)
    }

    /// Flood id-mismatched responses that are each below the message cap but
    /// collectively exceed the reader+pending aggregate budget.
    async fn spawn_buffer_flood_server(
        payload_len: usize,
        response_count: usize,
    ) -> (String, JoinHandle<()>) {
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
            let _ = src.next().await;
            let padding = "a".repeat(payload_len);
            for id in 0..response_count {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": padding,
                })
                .to_string();
                if sink.send(Message::Text(response.into())).await.is_err() {
                    return;
                }
            }
            while let Some(Ok(message)) = src.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });
        (url, handle)
    }

    /// Return the requested response first, then wait for the test to release
    /// an aggregate-budget flood. This deterministically reproduces the race
    /// where request correlation finishes before the reader sees the attack.
    async fn spawn_success_then_flood_server(
        payload_len: usize,
        response_count: usize,
    ) -> (String, Arc<Notify>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let release_flood = Arc::new(Notify::new());
        let server_release = Arc::clone(&release_flood);
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sink, mut src) = ws.split();
            let _ = src.next().await;
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 777,
                "result": {"ok": true},
            })
            .to_string();
            if sink.send(Message::Text(response.into())).await.is_err() {
                return;
            }

            server_release.notified().await;
            let padding = "a".repeat(payload_len);
            for id in 0..response_count {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": padding,
                })
                .to_string();
                if sink.send(Message::Text(response.into())).await.is_err() {
                    return;
                }
            }
            while let Some(Ok(message)) = src.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });
        (url, release_flood, handle)
    }

    /// Send a valid response, then wait before initiating a proper peer Close.
    /// Because the client did not initiate shutdown, this is terminal evidence
    /// and must not be normalized into a passing teardown.
    async fn spawn_success_then_peer_close_server() -> (String, Arc<Notify>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}", listener.local_addr().expect("addr"));
        let release_close = Arc::new(Notify::new());
        let server_release = Arc::clone(&release_close);
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sink, mut src) = ws.split();
            let _ = src.next().await;
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 888,
                "result": {"ok": true},
            })
            .to_string();
            if sink.send(Message::Text(response.into())).await.is_err() {
                return;
            }
            server_release.notified().await;
            let _ = sink.send(Message::Close(None)).await;
            let _ = sink.close().await;
        });
        (url, release_close, handle)
    }

    /// Reply normally, but drop the socket when the client sends Close rather
    /// than completing the close handshake.
    async fn spawn_reset_during_shutdown_server() -> (String, JoinHandle<()>) {
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
            let _ = src.next().await;
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 889,
                "result": {"ok": true},
            })
            .to_string();
            if sink.send(Message::Text(response.into())).await.is_err() {
                return;
            }
            while let Some(Ok(message)) = src.next().await {
                if matches!(message, Message::Close(_)) {
                    // Deliberately do not flush tungstenite's queued reply.
                    return;
                }
            }
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
        // Tungstenite is configured with both max_frame_size and
        // max_message_size at 16 MiB, so this should fail before decode_message
        // ever owns the oversized payload.
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

    #[tokio::test]
    async fn oversized_fragmented_message_is_rejected_during_accumulation() {
        let fragment_len = 4 * 1024 * 1024;
        let fragment_count = 5;
        assert!(fragment_len < MAX_FRAME_BYTES);
        assert!(fragment_len * fragment_count > MAX_FRAME_BYTES);

        let (url, _srv) = spawn_fragmented_oversized_server(fragment_len, fragment_count).await;
        let transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let err = tokio::time::timeout(Duration::from_secs(5), boxed.request(body))
            .await
            .expect("fragmented oversize handling should be bounded")
            .expect_err("fragmented message above 16 MiB must be rejected");
        let TransportError::Other(message) = err else {
            panic!("expected TransportError::Other, got {err:?}");
        };
        let lower = message.to_lowercase();
        assert!(
            lower.contains("size")
                || lower.contains("too large")
                || lower.contains("too long")
                || lower.contains("space limit"),
            "expected accumulation size error, got: {message}"
        );
    }

    #[tokio::test]
    async fn aggregate_reader_and_pending_budget_fails_typed() {
        const PAYLOAD_BYTES: usize = 9 * 1024 * 1024;
        const RESPONSE_COUNT: usize = 4;
        const {
            assert!(PAYLOAD_BYTES < MAX_FRAME_BYTES);
            assert!(PAYLOAD_BYTES * RESPONSE_COUNT > MAX_BUFFERED_BYTES);
        }

        let (url, _srv) = spawn_buffer_flood_server(PAYLOAD_BYTES, RESPONSE_COUNT).await;
        let transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let mut boxed: Box<dyn Transport> = Box::new(transport);
        let body = r#"{"jsonrpc":"2.0","id":999,"method":"ping"}"#;
        let err = tokio::time::timeout(Duration::from_secs(10), boxed.request(body))
            .await
            .expect("aggregate budget exhaustion should be bounded")
            .expect_err("id-mismatch flood must exceed aggregate byte budget");
        let TransportError::Other(message) = err else {
            panic!("expected TransportError::Other, got {err:?}");
        };
        assert!(
            message.contains("aggregate buffered-frame budget exceeded")
                && message.contains(&MAX_BUFFERED_BYTES.to_string()),
            "expected stable aggregate-budget error, got: {message}"
        );
        let notify_error = boxed
            .notify(r#"{"jsonrpc":"2.0","method":"cancel"}"#)
            .await
            .expect_err("aggregate exhaustion must poison subsequent operations");
        assert!(
            matches!(
                notify_error,
                TransportError::Other(ref message)
                    if message.contains("aggregate buffered-frame budget exceeded")
            ),
            "expected the same typed aggregate-budget error, got {notify_error:?}"
        );
        let shutdown_error = boxed
            .shutdown()
            .await
            .expect_err("teardown must retain aggregate-budget failure");
        assert!(
            shutdown_error
                .to_string()
                .contains("aggregate buffered-frame budget exceeded"),
            "unexpected shutdown error: {shutdown_error}"
        );
    }

    #[tokio::test]
    async fn matched_response_then_later_flood_fails_teardown() {
        const PAYLOAD_BYTES: usize = 9 * 1024 * 1024;
        const RESPONSE_COUNT: usize = 4;
        const {
            assert!(PAYLOAD_BYTES * RESPONSE_COUNT > MAX_BUFFERED_BYTES);
        }

        let (url, release_flood, _srv) =
            spawn_success_then_flood_server(PAYLOAD_BYTES, RESPONSE_COUNT).await;
        let mut transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let body = r#"{"jsonrpc":"2.0","id":777,"method":"ping"}"#;
        let response = transport
            .request(body)
            .await
            .expect("matching response precedes controlled flood");
        assert_eq!(
            extract_id(&response),
            Some(serde_json::Value::from(777)),
            "correlated the expected response"
        );

        release_flood.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !transport.buffer_budget.exhausted.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader must process the controlled aggregate overflow");

        let shutdown_error = Box::new(transport)
            .shutdown()
            .await
            .expect_err("post-response reader failure must fail teardown");
        let message = shutdown_error.to_string();
        assert!(
            message.contains("WebSocket shutdown incomplete")
                && message.contains("aggregate buffered-frame budget exceeded")
                && message.contains(&MAX_BUFFERED_BYTES.to_string()),
            "expected secret-safe aggregate teardown failure, got: {message}"
        );
    }

    #[tokio::test]
    async fn matched_response_then_unsolicited_peer_close_fails_teardown() {
        let (url, release_close, _srv) = spawn_success_then_peer_close_server().await;
        let mut transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let response = transport
            .request(r#"{"jsonrpc":"2.0","id":888,"method":"ping"}"#)
            .await
            .expect("matching response precedes controlled peer Close");
        assert_eq!(extract_id(&response), Some(serde_json::Value::from(888)));

        release_close.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while transport.reader_terminal.state.load(Ordering::Acquire) != READER_FAILED {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader must latch the unsolicited peer Close as failure");

        let error = Box::new(transport)
            .shutdown()
            .await
            .expect_err("unsolicited peer Close must fail teardown");
        assert!(
            error
                .to_string()
                .contains("reader terminated after an inbound protocol or socket error"),
            "unexpected teardown diagnostic: {error}"
        );
    }

    #[tokio::test]
    async fn reset_without_close_handshake_fails_shutdown() {
        let (url, _srv) = spawn_reset_during_shutdown_server().await;
        let mut transport = WsTransport::connect(&url, &allow_all())
            .await
            .expect("connect");
        let response = transport
            .request(r#"{"jsonrpc":"2.0","id":889,"method":"ping"}"#)
            .await
            .expect("matching response");
        assert_eq!(extract_id(&response), Some(serde_json::Value::from(889)));

        let error = Box::new(transport)
            .shutdown()
            .await
            .expect_err("reset without a Close reply is uncertain teardown");
        assert!(
            error
                .to_string()
                .contains("reader terminated after an inbound protocol or socket error"),
            "reset-without-handshake was not retained: {error}"
        );
    }

    #[test]
    fn buffered_frame_releases_budget_on_return_and_drop() {
        let budget = BufferBudget::new(256);
        let frame = budget
            .try_reserve("1234".to_owned())
            .expect("first reservation");
        let first_charge = frame.bytes;
        assert!(first_charge > 4, "reservation includes frame storage");
        assert_eq!(budget.used.load(Ordering::Acquire), first_charge);

        let mut pending = VecDeque::new();
        pending.push_back(frame);
        assert_eq!(budget.used.load(Ordering::Acquire), first_charge);
        let returned = pending.pop_front().expect("pending frame").into_string();
        assert_eq!(returned, "1234");
        assert_eq!(budget.used.load(Ordering::Acquire), 0);

        let retained = budget
            .try_reserve("x".repeat(100))
            .expect("second reservation");
        let error = budget
            .try_reserve("y".repeat(200))
            .expect_err("aggregate overflow");
        assert!(matches!(
            error,
            TransportError::Other(ref message)
                if message.contains("aggregate buffered-frame budget exceeded 256 bytes")
        ));
        drop(retained);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert!(budget.check().is_err(), "overflow poisons the transport");
    }
}
