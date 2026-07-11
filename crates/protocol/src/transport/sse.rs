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
//! The reader half runs in a background task: it parses the SSE byte stream
//! into [`eventsource_stream::Event`]s and forwards `message` payloads onto an
//! mpsc channel. The foreground [`Transport::request`] / [`Transport::notify`]
//! impls do the HTTP POST and (for `request`) drain the channel until a body
//! with the matching `id` arrives. Out-of-order events are buffered in a
//! small [`VecDeque`]; for M4 minimal — where the [`crate::Session`] layer is
//! single-flight — this buffer should rarely hold more than one entry.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::guard::HostGuard;
use super::{Transport, TransportError, resolve};

mod reader;
use reader::{extract_id, spawn_reader};

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
    incoming: mpsc::Receiver<Result<String, TransportError>>,
    reader_task: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    /// Holding area for responses that arrived while we weren't waiting for
    /// them — or for ones whose `id` didn't match the request we're currently
    /// blocked on. Drained on the next `request` call.
    pending: VecDeque<String>,
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
        let sse_url = url::Url::parse(sse_url.as_ref())
            .map_err(|e| TransportError::Other(format!("invalid sse url: {e}")))?;
        // Resolve + vet + pin the SSE host (ADR 0012 literal layer + ADR 0016
        // resolver layer). Redirects are off inside `pinned_client` — see the
        // rationale comment there.
        let sse_addrs = resolve::resolve_and_check(&sse_url, guard).await?;
        let client = resolve::pinned_client(&sse_url, &sse_addrs)?;

        let resp = client
            .get(sse_url.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| TransportError::Http(e.to_string()))?;
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

        let mut event_stream = resp.bytes_stream().eventsource();

        // Read events until we see the `endpoint` one; ignore irrelevant
        // comments / retries.
        let endpoint_event = tokio::time::timeout(ENDPOINT_HANDSHAKE_TIMEOUT, async {
            loop {
                match event_stream.next().await {
                    Some(Ok(ev)) if ev.event == "endpoint" => return Ok(ev),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        return Err(TransportError::Other(format!("sse parse error: {e}")));
                    }
                    None => return Err(TransportError::Closed),
                }
            }
        })
        .await
        .map_err(|_| TransportError::Timeout(ENDPOINT_HANDSHAKE_TIMEOUT))??;

        let post_url = sse_url
            .join(endpoint_event.data.trim())
            .map_err(|e| TransportError::Other(format!("invalid endpoint url: {e}")))?;
        // The endpoint URL came from the server — re-run the full
        // resolve + vet + pin so a hostile server can't point our POSTs into
        // private/loopback space, a host outside the allowlist (ADR 0012), or
        // a hostname that resolves private / rebinds (ADR 0016). The POSTs go
        // through their own client pinned to the endpoint's vetted addresses;
        // the SSE GET stream above stays on the original connection.
        let post_addrs = resolve::resolve_and_check(&post_url, guard).await?;
        let client = resolve::pinned_client(&post_url, &post_addrs)?;

        let (tx, rx) = mpsc::channel::<Result<String, TransportError>>(READER_CHANNEL_CAP);
        let cancel = CancellationToken::new();
        let reader_task = spawn_reader(event_stream, tx, cancel.clone());

        Ok(Self {
            post_url,
            client,
            incoming: rx,
            reader_task: Some(reader_task),
            cancel,
            pending: VecDeque::new(),
        })
    }

    async fn post(&self, body: &str) -> Result<(), TransportError> {
        let resp = self
            .client
            .post(self.post_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| TransportError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(TransportError::Http(format!(
                "post {}: {}",
                self.post_url,
                resp.status().as_u16()
            )));
        }
        Ok(())
    }
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
            .position(|f| extract_id(f) == expected_id)
        {
            // Cheap O(n) removal — pending should be tiny.
            return Ok(self.pending.remove(idx).expect("position just returned"));
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
                    if extract_id(&frame) == expected_id {
                        return Ok(frame);
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
        self.cancel.cancel();
        if let Some(handle) = self.reader_task.take() {
            // Best-effort: don't fail shutdown if the task is slow to wind
            // down — abort and move on.
            match tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(_join_err)) => {}
                Err(_) => {
                    // We already cancelled; the task is just slow. Give up on it.
                }
            }
        }
        Ok(())
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
