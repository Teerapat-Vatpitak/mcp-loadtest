//! SSE (Server-Sent Events) transport.
//!
//! The MCP SSE handshake, summarised:
//!
//! 1. Client `GET`s the SSE URL. Server replies `200 OK` with
//!    `Content-Type: text/event-stream`.
//! 2. The first event on the stream is `event: endpoint` whose `data` is the
//!    POST URL the client should send subsequent JSON-RPC bodies to.
//! 3. Server responses + server-initiated notifications arrive as
//!    `event: message` events on the same stream.
//! 4. Client matches responses to outbound requests by JSON-RPC `id`.
//!
//! The reader half runs in a background task: it incrementally parses the SSE
//! byte stream under explicit per-event and aggregate byte budgets, then
//! forwards `message` payloads onto an mpsc channel. The foreground
//! [`Transport::request`] / [`Transport::notify`] impls do the HTTP POST and
//! (for `request`) drain the channel until a body with the matching `id`
//! arrives. Out-of-order events are buffered in a small [`VecDeque`]; for M4
//! minimal — where the [`crate::Session`] layer is single-flight — this buffer
//! should rarely hold more than one entry.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use mcp_loadtest_core::config::validate_remote_endpoint;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::guard::HostGuard;
use super::headers::RemoteHeaders;
use super::{Transport, TransportError, resolve};

mod reader;
use reader::{
    BoundedSseParser, INBOUND_BYTE_BUDGET, InboundFrame, TerminalErrorLatch, extract_id,
    spawn_reader,
};

/// Default budget for awaiting a single `message` event in response to a POST.
/// Same order of magnitude as the stdio `request` budget so tests of a wedged
/// server still terminate.
const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// Budget for the initial `endpoint` event during [`SseTransport::connect`].
const ENDPOINT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the mpsc channel between the reader task and the foreground
/// transport. Each item is one server-emitted JSON-RPC frame. 64 is generous —
/// even bursty servers shouldn't outpace the consumer by that much.
const READER_CHANNEL_CAP: usize = 64;

/// Graceful-shutdown budget for the reader task during [`Transport::shutdown`].
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the id-mismatch buffer. A misbehaving or hostile server that floods
/// notifications without ever answering the in-flight request would otherwise
/// grow `pending` without bound. 256 is well above any realistic
/// notification burst (MCP servers rarely emit > a handful per request) but
/// far below memory-exhaustion territory. On overflow we surface
/// [`TransportError::Other`] so the operator can investigate.
const MAX_PENDING_FRAMES: usize = 256;

/// SSE transport. See module docs for the handshake sketch.
pub struct SseTransport {
    /// URL we POST JSON-RPC bodies to (the server's `endpoint` event told us
    /// where).
    post_url: url::Url,
    client: reqwest::Client,
    /// Inbound `message` event payloads from the SSE reader task.
    incoming: mpsc::Receiver<Result<InboundFrame, TransportError>>,
    reader_task: Option<JoinHandle<()>>,
    terminal_error: Arc<TerminalErrorLatch>,
    cancel: CancellationToken,
    /// Holding area for responses that arrived while we weren't waiting for
    /// them — or for ones whose `id` didn't match the request we're currently
    /// blocked on. Drained on the next `request` call.
    pending: VecDeque<InboundFrame>,
    remote_headers: RemoteHeaders,
}

impl SseTransport {
    /// Open the SSE stream at `sse_url`, await the initial `endpoint` event,
    /// and spawn the background reader task.
    ///
    /// The endpoint event's `data` may be absolute or relative; relative URLs
    /// are resolved against `sse_url`.
    ///
    /// No redirects are followed — pass the canonical URL. If the SSE handshake
    /// or a subsequent POST responds with a redirect, the call surfaces as a
    /// non-2xx error rather than silently chasing the `Location` header.
    ///
    /// `guard` enforces the SSRF host-allowlist + private-IP-literal block
    /// (ADR 0012) plus the resolved-address block + pinning (ADR 0016). It is
    /// applied twice: once against `sse_url`, and again — with a fresh
    /// resolve + pin — against the server-provided `endpoint` POST URL (which
    /// is attacker-influenceable — a hostile server could point it at
    /// internal infrastructure).
    pub async fn connect(
        sse_url: impl AsRef<str>,
        guard: &HostGuard,
    ) -> Result<Self, TransportError> {
        Self::connect_with_headers(sse_url, guard, RemoteHeaders::default()).await
    }

    /// Connect with static outbound headers loaded from environment
    /// variables. Headers are sent on both the initial SSE GET and every
    /// JSON-RPC POST.
    pub async fn connect_with_headers(
        sse_url: impl AsRef<str>,
        guard: &HostGuard,
        remote_headers: RemoteHeaders,
    ) -> Result<Self, TransportError> {
        let sse_url = validate_remote_endpoint(sse_url.as_ref(), "sse", !remote_headers.is_empty())
            .map_err(TransportError::Other)?;
        // Resolve + vet + pin the SSE host (ADR 0012 literal layer + ADR 0016
        // resolver layer). Redirects are off inside `pinned_client` — see the
        // rationale comment there.
        let sse_addrs = resolve::resolve_and_check(&sse_url, guard).await?;
        let client = resolve::pinned_client(&sse_url, &sse_addrs)?;

        let resp = remote_headers
            .apply_reqwest(
                client
                    .get(sse_url.clone())
                    .header(reqwest::header::ACCEPT, "text/event-stream"),
            )
            .send()
            .await
            .map_err(|e| TransportError::Http(format!("sse handshake: {}", e.without_url())))?;
        if !resp.status().is_success() {
            return Err(TransportError::Http(format!(
                "sse handshake: {} {}",
                resp.status().as_u16(),
                resp.status().canonical_reason().unwrap_or("")
            )));
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ctype.contains("text/event-stream") {
            return Err(TransportError::Http(format!(
                "sse handshake: unexpected content-type `{ctype}`",
            )));
        }

        // Parse the wire incrementally. `eventsource-stream` accumulates
        // unbounded Strings before yielding an event, which is too late for a
        // transport-level memory limit.
        let event_stream = resp
            .bytes_stream()
            .map(|item| item.map_err(|_| "SSE stream read/parse failure"));
        let mut event_stream = BoundedSseParser::new(event_stream);

        // Read events until we see the `endpoint` one; ignore irrelevant
        // comments / retries.
        let endpoint_event = tokio::time::timeout(ENDPOINT_HANDSHAKE_TIMEOUT, async {
            loop {
                match event_stream.next_event().await {
                    Ok(Some(ev)) if ev.event == "endpoint" => return Ok(ev),
                    Ok(Some(_)) => continue,
                    Ok(None) => return Err(TransportError::Closed),
                    Err(error) => return Err(error.into_transport_error()),
                }
            }
        })
        .await
        .map_err(|_| TransportError::Timeout(ENDPOINT_HANDSHAKE_TIMEOUT))??;

        let post_url = sse_url
            .join(endpoint_event.data.trim())
            .map_err(|_| TransportError::Other("invalid SSE endpoint URL".into()))?;
        let post_url =
            validate_remote_endpoint(post_url.as_str(), "sse", !remote_headers.is_empty())
                .map_err(TransportError::Other)?;
        if !remote_headers.is_empty() && !same_origin(&sse_url, &post_url) {
            return Err(TransportError::Other(
                "sse endpoint changed origin; refusing to forward secret-backed remote headers"
                    .into(),
            ));
        }
        // The endpoint URL came from the server — re-run the full
        // resolve + vet + pin so a hostile server can't point our POSTs into
        // private/loopback space, a host outside the allowlist (ADR 0012), or
        // a hostname that resolves private / rebinds (ADR 0016). The POSTs go
        // through their own client pinned to the endpoint's vetted addresses;
        // the SSE GET stream above stays on the original connection.
        let post_addrs = resolve::resolve_and_check(&post_url, guard).await?;
        let client = resolve::pinned_client(&post_url, &post_addrs)?;

        let (tx, rx) = mpsc::channel::<Result<InboundFrame, TransportError>>(READER_CHANNEL_CAP);
        let byte_budget = Arc::new(Semaphore::new(INBOUND_BYTE_BUDGET));
        let terminal_error = Arc::new(TerminalErrorLatch::default());
        let cancel = CancellationToken::new();
        let reader_task = spawn_reader(
            event_stream,
            tx,
            byte_budget,
            terminal_error.clone(),
            cancel.clone(),
        );

        Ok(Self {
            post_url,
            client,
            incoming: rx,
            reader_task: Some(reader_task),
            terminal_error,
            cancel,
            pending: VecDeque::new(),
            remote_headers,
        })
    }

    async fn post(&self, body: &str) -> Result<(), TransportError> {
        let resp = self
            .remote_headers
            .apply_reqwest(
                self.client
                    .post(self.post_url.clone())
                    .header(reqwest::header::CONTENT_TYPE, "application/json"),
            )
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| TransportError::Http(format!("sse post: {}", e.without_url())))?;
        if !resp.status().is_success() {
            return Err(TransportError::Http(format!(
                "sse post: status {}",
                resp.status().as_u16()
            )));
        }
        Ok(())
    }

    fn fail_if_reader_failed(&self) -> Result<(), TransportError> {
        match self.terminal_error.get() {
            Some(error) => Err(error.into_transport_error()),
            None => Ok(()),
        }
    }
}

fn same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

#[async_trait]
impl Transport for SseTransport {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        let expected_id = extract_id(body);

        self.post(body).await?;

        // First, see if a matching frame already sits in our buffer (it could
        // have arrived between requests, or out of order on a previous call).
        if let Some(idx) = self
            .pending
            .iter()
            .position(|f| extract_id(f.as_str()) == expected_id)
        {
            // Cheap O(n) removal — pending should be tiny.
            self.fail_if_reader_failed()?;
            return Ok(self
                .pending
                .remove(idx)
                .expect("position just returned")
                .into_string());
        }

        // Otherwise read from the channel until we see a matching frame, with
        // a wide timeout so a wedged server still surfaces eventually.
        loop {
            let recv_fut = self.incoming.recv();
            let next = tokio::time::timeout(DEFAULT_RECV_TIMEOUT, recv_fut)
                .await
                .map_err(|_| TransportError::Timeout(DEFAULT_RECV_TIMEOUT))?;
            match next {
                Some(Ok(frame)) => {
                    if extract_id(frame.as_str()) == expected_id {
                        self.fail_if_reader_failed()?;
                        return Ok(frame.into_string());
                    }
                    // Stash for a future request and keep waiting. Cap the
                    // buffer to defend against a server that floods
                    // notifications without ever answering the in-flight
                    // request — see `MAX_PENDING_FRAMES`.
                    if self.pending.len() >= MAX_PENDING_FRAMES {
                        return Err(TransportError::Other(format!(
                            "sse transport: pending id-mismatch buffer hit {MAX_PENDING_FRAMES} frames \
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
        // Notifications have no `id`, so no correlation; just POST and forget.
        self.post(body).await
    }

    fn pid(&self) -> Option<u32> {
        None
    }

    async fn shutdown(mut self: Box<Self>) -> Result<(), TransportError> {
        let mut failures = Vec::new();

        self.cancel.cancel();
        if let Some(handle) = self.reader_task.as_mut() {
            match tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, &mut *handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("reader task failed: {error}")),
                Err(_) => {
                    // Retain the JoinHandle while aborting so cancellation of
                    // this shutdown future still reaches Drop's backstop
                    // instead of detaching the task.
                    handle.abort();
                    failures.push(format!(
                        "reader task join exceeded {SHUTDOWN_JOIN_TIMEOUT:?}"
                    ));
                    match tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, &mut *handle).await {
                        Ok(Err(error)) if error.is_cancelled() => {}
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            failures.push(format!("reader task abort failed: {error}"));
                        }
                        Err(_) => failures.push(format!(
                            "reader task abort confirmation exceeded {SHUTDOWN_JOIN_TIMEOUT:?}"
                        )),
                    }
                }
            }
            self.reader_task.take();
        }
        if let Some(error) = self.terminal_error.get() {
            failures.push(format!("reader terminal failure: {error}"));
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(TransportError::Other(format!(
                "SSE shutdown incomplete: {}",
                failures.join("; ")
            )))
        }
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        // If the caller dropped us without going through `shutdown`, still
        // tear down the reader task to avoid leaking it.
        self.cancel.cancel();
        if let Some(handle) = self.reader_task.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use httpmock::Method::POST;

    use super::*;

    #[tokio::test]
    async fn shutdown_surfaces_reader_task_failure() {
        let (sender, incoming) = mpsc::channel(1);
        drop(sender);
        let reader_task = tokio::spawn(async {
            panic!("synthetic SSE reader failure");
        });
        tokio::task::yield_now().await;

        let transport = SseTransport {
            post_url: url::Url::parse("https://example.invalid/rpc").expect("valid test URL"),
            client: reqwest::Client::new(),
            incoming,
            reader_task: Some(reader_task),
            terminal_error: Arc::new(TerminalErrorLatch::default()),
            cancel: CancellationToken::new(),
            pending: VecDeque::new(),
            remote_headers: RemoteHeaders::default(),
        };

        let error = Box::new(transport)
            .shutdown()
            .await
            .expect_err("reader failure must make shutdown fail closed");
        assert!(
            error.to_string().contains("reader task failed"),
            "unexpected shutdown error: {error}"
        );
    }

    #[tokio::test]
    async fn latched_overflow_beats_earlier_matching_success() {
        const FRAME_BYTES: usize = 12 * 1024 * 1024;
        const SENTINEL: &str = "sse-latched-overflow-secret";

        fn padded_message(id: u64, marker: &str) -> Vec<u8> {
            let prefix = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"padding":""#);
            let suffix = format!("{marker}\"}}}}");
            let mut wire = Vec::with_capacity(FRAME_BYTES + 32);
            wire.extend_from_slice(b"event: message\ndata: ");
            wire.extend_from_slice(prefix.as_bytes());
            wire.extend(std::iter::repeat_n(
                b'x',
                FRAME_BYTES - prefix.len() - suffix.len(),
            ));
            wire.extend_from_slice(suffix.as_bytes());
            wire.extend_from_slice(b"\n\n");
            wire
        }

        let chunks = vec![
            Ok::<_, &'static str>(
                br#"event: message
data: {"jsonrpc":"2.0","id":42,"result":{}}

"#
                .to_vec(),
            ),
            Ok(padded_message(1, "")),
            Ok(padded_message(2, "")),
            Ok(padded_message(3, SENTINEL)),
        ];
        let parser = BoundedSseParser::new(stream::iter(chunks));
        let (sender, mut incoming) = mpsc::channel(READER_CHANNEL_CAP);
        let terminal_error = Arc::new(TerminalErrorLatch::default());
        let cancel = CancellationToken::new();
        let reader_task = spawn_reader(
            parser,
            sender,
            Arc::new(Semaphore::new(INBOUND_BYTE_BUDGET)),
            terminal_error.clone(),
            cancel.clone(),
        );

        tokio::time::timeout(Duration::from_secs(10), async {
            while terminal_error.get().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader never latched aggregate overflow");
        assert_eq!(
            terminal_error.get(),
            Some(reader::ReaderTerminalError::AggregateBudget)
        );

        let matching = incoming
            .recv()
            .await
            .expect("matching frame queued before overflow")
            .expect("first frame is successful");
        assert_eq!(extract_id(matching.as_str()), Some(serde_json::json!(42)));

        let mut pending = VecDeque::new();
        pending.push_back(matching);
        let post_server = httpmock::MockServer::start_async().await;
        let post_mock = post_server
            .mock_async(|when, then| {
                when.method(POST).path("/rpc");
                then.status(202);
            })
            .await;
        let mut transport = SseTransport {
            post_url: url::Url::parse(&post_server.url("/rpc")).expect("valid test URL"),
            client: reqwest::Client::new(),
            incoming,
            reader_task: Some(reader_task),
            terminal_error,
            cancel,
            pending,
            remote_headers: RemoteHeaders::default(),
        };
        let request_gate = transport
            .request(r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#)
            .await
            .expect_err("latched failure must beat matching pending response");
        post_mock.assert_async().await;
        assert!(request_gate.to_string().contains("aggregate budget"));
        assert!(!request_gate.to_string().contains(SENTINEL));

        let shutdown_error = Box::new(transport)
            .shutdown()
            .await
            .expect_err("unobserved queued error must make shutdown fail closed");
        let diagnostic = shutdown_error.to_string();
        assert!(diagnostic.contains("aggregate budget"), "{diagnostic}");
        assert!(!diagnostic.contains(SENTINEL), "{diagnostic}");
    }

    #[tokio::test]
    async fn matching_success_before_peer_eof_still_fails_shutdown() {
        let chunks = vec![Ok::<_, &'static str>(
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n".to_vec(),
        )];
        let parser = BoundedSseParser::new(stream::iter(chunks));
        let (sender, mut incoming) = mpsc::channel(READER_CHANNEL_CAP);
        let terminal_error = Arc::new(TerminalErrorLatch::default());
        let reader_task = spawn_reader(
            parser,
            sender,
            Arc::new(Semaphore::new(INBOUND_BYTE_BUDGET)),
            terminal_error.clone(),
            CancellationToken::new(),
        );

        let matching = incoming
            .recv()
            .await
            .expect("success frame")
            .expect("matching response");
        assert!(matches!(
            incoming.recv().await.expect("closed marker"),
            Err(TransportError::Closed)
        ));
        assert_eq!(
            terminal_error.get(),
            Some(reader::ReaderTerminalError::UnexpectedEof)
        );

        let mut pending = VecDeque::new();
        pending.push_back(matching);
        let transport = SseTransport {
            post_url: url::Url::parse("https://example.invalid/rpc").expect("valid test URL"),
            client: reqwest::Client::new(),
            incoming,
            reader_task: Some(reader_task),
            terminal_error,
            cancel: CancellationToken::new(),
            pending,
            remote_headers: RemoteHeaders::default(),
        };
        let error = Box::new(transport)
            .shutdown()
            .await
            .expect_err("matching response must not hide unexpected peer EOF");
        assert!(error.to_string().contains("closed unexpectedly"));
    }
}
