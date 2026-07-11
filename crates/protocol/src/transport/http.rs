//! HTTP transport (Streamable HTTP per MCP spec, simple `application/json`
//! response variant).
//!
//! Per the MCP Streamable HTTP transport, the client POSTs a JSON-RPC body to
//! a single endpoint URL. The server responds with either:
//!
//! - `application/json` — one JSON-RPC response object in the body.
//! - `text/event-stream` — an SSE stream that eventually carries the matching
//!   response (and possibly server-initiated messages in between).
//!
//! M4 minimal scope handled here: the simple JSON case. SSE-response handling
//! is deferred to M5 — if the server picks the streaming variant on us, we
//! surface a clear [`TransportError::Other`] so the caller knows to upgrade.

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use url::Url;

use super::guard::HostGuard;
use super::{Transport, TransportError, resolve};

/// Header carrying the negotiated protocol revision on every request after
/// `initialize` (Streamable HTTP requirement since the 2025-06-18 spec).
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";

/// HTTP transport — owns a reusable `reqwest::Client` and the endpoint URL.
///
/// Construct via [`HttpTransport::connect`]; pass into
/// [`crate::Session::from_transport`] to drive an MCP session over HTTP.
pub struct HttpTransport {
    client: reqwest::Client,
    url: Url,
    /// Negotiated version header value, set by [`Transport::set_protocol_version`]
    /// after the handshake. `None` before negotiation (the `initialize` POST
    /// itself carries no version header, per spec).
    protocol_version: Option<HeaderValue>,
}

impl HttpTransport {
    /// Build an [`HttpTransport`] pointing at `url`. For hostname URLs this
    /// performs one DNS resolution up front so the addresses can be vetted
    /// and pinned (ADR 0016); the TCP connection itself is established lazily
    /// on the first `request` / `notify`. URL parsing errors surface as
    /// [`TransportError::Other`].
    ///
    /// No redirects are followed — pass the canonical URL. If the target
    /// responds with a redirect, the call surfaces as a non-2xx error rather
    /// than silently chasing the `Location` header.
    ///
    /// `guard` enforces the SSRF host-allowlist + private-IP-literal block
    /// (ADR 0012) and the resolved-address block (ADR 0016) against the
    /// parsed URL before any client is built; the vetted addresses are pinned
    /// into the client so the checked IP is the dialed IP.
    pub async fn connect(url: impl AsRef<str>, guard: &HostGuard) -> Result<Self, TransportError> {
        let url = Url::parse(url.as_ref())
            .map_err(|e| TransportError::Other(format!("invalid url: {e}")))?;
        let addrs = resolve::resolve_and_check(&url, guard).await?;
        let client = resolve::pinned_client(&url, &addrs)?;
        Ok(Self {
            client,
            url,
            protocol_version: None,
        })
    }

    /// POST `body` as `application/json` and return the raw response body.
    /// Shared between `request` and `notify`.
    async fn post(&self, body: &str) -> Result<reqwest::Response, TransportError> {
        let mut req = self
            .client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            // Per MCP Streamable HTTP, advertise both shapes the server may
            // return. The simple JSON case is what we implement here; SSE is
            // M5 work.
            .header(ACCEPT, "application/json, text/event-stream");
        if let Some(version) = &self.protocol_version {
            req = req.header(MCP_PROTOCOL_VERSION_HEADER, version.clone());
        }
        let resp = req
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| TransportError::Http(format!("post: {e}")))?;
        Ok(resp)
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        let resp = self.post(body).await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(TransportError::Http(format!("status {}", status.as_u16())));
        }

        // If the server picked the streaming variant, bail out cleanly until
        // M5 lands SSE-response handling.
        if is_event_stream(&resp) {
            return Err(TransportError::Other(
                "streamable HTTP SSE response not yet supported (M5)".into(),
            ));
        }

        resp.text()
            .await
            .map_err(|e| TransportError::Http(format!("read body: {e}")))
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        let resp = self.post(body).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(TransportError::Http(format!("status {}", status.as_u16())));
        }
        // Drain any response body to free the connection back to the pool.
        // Notifications carry no JSON-RPC reply per spec, but servers may
        // still send `202 Accepted` with an empty body or a 200 with one.
        let _ = resp.bytes().await;
        Ok(())
    }

    fn pid(&self) -> Option<u32> {
        None
    }

    fn set_protocol_version(&mut self, version: &str) {
        // A permissively-accepted unknown version string may contain bytes
        // that are not valid in an HTTP header — skip the header rather than
        // poisoning every subsequent request.
        match HeaderValue::from_str(version) {
            Ok(v) => self.protocol_version = Some(v),
            Err(_) => {
                tracing::debug!(
                    version,
                    "negotiated protocol version is not a valid header value; \
                     omitting the MCP-Protocol-Version header"
                );
            }
        }
    }

    async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
        // HTTP has no session-scoped state to tear down. The connection pool
        // inside `reqwest::Client` drops with `self`.
        Ok(())
    }
}

/// Does the response Content-Type indicate `text/event-stream`?
fn is_event_stream(resp: &reqwest::Response) -> bool {
    let Some(ct) = resp.headers().get(CONTENT_TYPE) else {
        return false;
    };
    let Ok(ct_str) = ct.to_str() else {
        return false;
    };
    ct_str
        .split(';')
        .next()
        .map(|s| s.trim().eq_ignore_ascii_case("text/event-stream"))
        .unwrap_or(false)
}
