//! Streamable HTTP transport.
//!
//! Per the MCP Streamable HTTP transport, the client POSTs a JSON-RPC body to
//! a single endpoint URL. The server responds with either:
//!
//! - `application/json` — one JSON-RPC response object in the body.
//! - `text/event-stream` — an SSE stream that eventually carries the matching
//!   response (and possibly server-initiated messages in between).
//!
//! Both response shapes required by the transport are supported. For the
//! 2026-07-28 stateless revision this transport also mirrors JSON-RPC fields
//! into the mandatory `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, and
//! schema-driven `Mcp-Param-*` headers (SEP-2243).

use async_trait::async_trait;
use futures_util::StreamExt;
use mcp_loadtest_auth::{AuthorizationContext, BearerChallenge, MemoryTokenStore, OAuthProvider};
use mcp_loadtest_core::config::validate_remote_endpoint;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::Value;
use std::sync::Arc;
use url::Url;

use super::guard::HostGuard;
use super::headers::RemoteHeaders;
use super::{Transport, TransportError, resolve};

mod metadata;
use metadata::ToolHeaderRegistry;

/// Header carrying the negotiated protocol revision on every request after
/// `initialize` (Streamable HTTP requirement since the 2025-06-18 spec).
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const STATELESS_PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SSE_NON_RESPONSE_EVENTS: usize = 256;
const MAX_OAUTH_REAUTH_ATTEMPTS: usize = 2;

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
    remote_headers: RemoteHeaders,
    tool_headers: ToolHeaderRegistry,
    oauth: Option<HttpOAuth>,
}

struct HttpOAuth {
    binding: tokio::sync::RwLock<OAuthBinding>,
    challenge_handler: Option<Arc<dyn OAuthChallengeHandler>>,
}

#[derive(Clone)]
struct OAuthBinding {
    provider: Arc<OAuthProvider<MemoryTokenStore>>,
    context: AuthorizationContext,
}

/// Run-scoped callback used to satisfy bounded 401/403 OAuth challenges.
#[async_trait]
pub trait OAuthChallengeHandler: Send + Sync {
    /// Obtain a new issuer/resource-bound provider after validating the
    /// challenge and applying the caller's bounded scope-step-up policy. The
    /// transport independently caps reauthorization at two attempts per MCP
    /// request even if the handler keeps returning credentials.
    async fn reauthorize(
        &self,
        challenge: BearerChallenge,
    ) -> Result<(Arc<OAuthProvider<MemoryTokenStore>>, AuthorizationContext), TransportError>;
}

impl HttpTransport {
    /// Probe the protected MCP endpoint for its OAuth Bearer challenge.
    ///
    /// The request uses the same DNS-vetted, address-pinned client as normal
    /// transport traffic. A successful unauthenticated response returns
    /// `None`; a 401/403 must contain one valid Bearer challenge.
    pub async fn discover_oauth_challenge(
        url: impl AsRef<str>,
        guard: &HostGuard,
        remote_headers: RemoteHeaders,
    ) -> Result<Option<BearerChallenge>, TransportError> {
        let url = validate_remote_endpoint(url.as_ref(), "http", !remote_headers.is_empty())
            .map_err(TransportError::Other)?;
        let addrs = resolve::resolve_and_check(&url, guard).await?;
        let client = resolve::pinned_client(&url, &addrs)?;
        let request = remote_headers.apply_reqwest(
            client
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json")
                .header(MCP_PROTOCOL_VERSION_HEADER, STATELESS_PROTOCOL_VERSION)
                .header("Mcp-Method", "server/discover")
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "server/discover",
                    "params": {}
                })),
        );
        let response = request.send().await.map_err(|error| {
            TransportError::Http(format!("OAuth probe: {}", error.without_url()))
        })?;
        if response.status().is_success() {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::UNAUTHORIZED
            && response.status() != reqwest::StatusCode::FORBIDDEN
        {
            return Err(TransportError::Http(format!(
                "OAuth probe status {}",
                response.status().as_u16()
            )));
        }
        let values = response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| TransportError::Other("invalid OAuth challenge header".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let refs = values.iter().map(String::as_str).collect::<Vec<_>>();
        BearerChallenge::parse(&refs)
            .map_err(|error| TransportError::Other(format!("invalid OAuth challenge: {error}")))?
            .map(Some)
            .ok_or_else(|| {
                TransportError::Other("OAuth response omitted a Bearer challenge".into())
            })
    }

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
        Self::connect_with_headers(url, guard, RemoteHeaders::default()).await
    }

    /// Connect with static outbound headers resolved from environment
    /// variables. See [`RemoteHeaders`]. These headers are attached to every
    /// POST, while protocol-owned headers remain protected from overrides.
    pub async fn connect_with_headers(
        url: impl AsRef<str>,
        guard: &HostGuard,
        remote_headers: RemoteHeaders,
    ) -> Result<Self, TransportError> {
        let url = validate_remote_endpoint(url.as_ref(), "http", !remote_headers.is_empty())
            .map_err(TransportError::Other)?;
        let addrs = resolve::resolve_and_check(&url, guard).await?;
        let client = resolve::pinned_client(&url, &addrs)?;
        Ok(Self {
            client,
            url,
            protocol_version: None,
            remote_headers,
            tool_headers: ToolHeaderRegistry::default(),
            oauth: None,
        })
    }

    /// Connect with an already configured OAuth provider and discovered
    /// authorization context. The provider is consulted before every HTTP
    /// request, including refresh-on-expiry; bearer tokens are never copied
    /// into transport configuration or debug output.
    pub async fn connect_with_oauth(
        url: impl AsRef<str>,
        guard: &HostGuard,
        remote_headers: RemoteHeaders,
        provider: Arc<OAuthProvider<MemoryTokenStore>>,
        context: AuthorizationContext,
    ) -> Result<Self, TransportError> {
        reject_static_authorization_header(&remote_headers)?;
        let mut transport = Self::connect_with_headers(url, guard, remote_headers).await?;
        transport.oauth = Some(HttpOAuth {
            binding: tokio::sync::RwLock::new(OAuthBinding { provider, context }),
            challenge_handler: None,
        });
        Ok(transport)
    }

    /// Connect with OAuth plus a bounded challenge handler for incremental
    /// consent or reauthorization after a protected request returns 401/403.
    pub async fn connect_with_oauth_handler(
        url: impl AsRef<str>,
        guard: &HostGuard,
        remote_headers: RemoteHeaders,
        provider: Arc<OAuthProvider<MemoryTokenStore>>,
        context: AuthorizationContext,
        challenge_handler: Arc<dyn OAuthChallengeHandler>,
    ) -> Result<Self, TransportError> {
        reject_static_authorization_header(&remote_headers)?;
        let mut transport = Self::connect_with_headers(url, guard, remote_headers).await?;
        transport.oauth = Some(HttpOAuth {
            binding: tokio::sync::RwLock::new(OAuthBinding { provider, context }),
            challenge_handler: Some(challenge_handler),
        });
        Ok(transport)
    }

    /// POST `body` as `application/json` and return the raw response body.
    /// Shared between `request` and `notify`.
    async fn post(
        &self,
        body: &str,
        expects_response: bool,
    ) -> Result<reqwest::Response, TransportError> {
        let mut reauthorization_attempts = 0_usize;
        loop {
            let mut req = self.remote_headers.apply_reqwest(
                self.client
                    .post(self.url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .header(ACCEPT, "application/json, text/event-stream"),
            );
            if let Some(oauth) = &self.oauth {
                let binding = oauth.binding.read().await.clone();
                if let Some(header) = binding
                    .provider
                    .authorization_header(&binding.context)
                    .await
                    .map_err(|error| {
                        TransportError::Other(format!("OAuth authorization failed: {error}"))
                    })?
                {
                    req = header.apply(req);
                }
            }
            if let Some(version) = &self.protocol_version {
                req = req.header(MCP_PROTOCOL_VERSION_HEADER, version.clone());
            }
            if expects_response && self.is_stateless() {
                let prepared = self.tool_headers.prepare(body)?;
                for (name, value) in &prepared.headers {
                    req = req.header(name, value);
                }
            }
            let resp = req
                .body(body.to_owned())
                .send()
                .await
                .map_err(|e| TransportError::Http(format!("post: {}", e.without_url())))?;
            let auth_status = resp.status() == reqwest::StatusCode::UNAUTHORIZED
                || resp.status() == reqwest::StatusCode::FORBIDDEN;
            let Some(oauth) = &self.oauth else {
                return Ok(resp);
            };
            let Some(handler) = &oauth.challenge_handler else {
                return Ok(resp);
            };
            if !auth_status {
                return Ok(resp);
            }
            if reauthorization_attempts >= MAX_OAUTH_REAUTH_ATTEMPTS {
                return Err(TransportError::Other(
                    "OAuth challenge retry limit exceeded".into(),
                ));
            }
            reauthorization_attempts += 1;
            let challenge = parse_bearer_challenge(resp.headers())?;
            let (provider, context) = handler.reauthorize(challenge).await?;
            *oauth.binding.write().await = OAuthBinding { provider, context };
        }
    }

    fn is_stateless(&self) -> bool {
        self.protocol_version.as_ref().and_then(|v| v.to_str().ok())
            == Some(STATELESS_PROTOCOL_VERSION)
    }
}

fn reject_static_authorization_header(
    remote_headers: &RemoteHeaders,
) -> Result<(), TransportError> {
    if remote_headers.iter().any(|(name, _)| name == AUTHORIZATION) {
        return Err(TransportError::Other(
            "static Authorization header cannot be combined with OAuth".into(),
        ));
    }
    Ok(())
}

fn parse_bearer_challenge(
    headers: &reqwest::header::HeaderMap,
) -> Result<BearerChallenge, TransportError> {
    let values = headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| TransportError::Other("invalid OAuth challenge header".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let refs = values.iter().map(String::as_str).collect::<Vec<_>>();
    BearerChallenge::parse(&refs)
        .map_err(|error| TransportError::Other(format!("invalid OAuth challenge: {error}")))?
        .ok_or_else(|| TransportError::Other("OAuth response omitted a Bearer challenge".into()))
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        let method = if self.is_stateless() {
            Some(self.tool_headers.prepare(body)?.method)
        } else {
            None
        };
        let resp = self.post(body, true).await?;

        let status = resp.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let response = read_response_text_limited(resp, "read error body").await?;
            // Modern MCP transport errors carry a JSON-RPC error body. Pass
            // it up so Session can inspect protocol-defined codes such as
            // UnsupportedProtocolVersion (-32022) instead of erasing them
            // behind the HTTP status.
            if is_jsonrpc_error(&response) {
                return Ok(response);
            }
            return Err(TransportError::Http(format!("status {status_code}")));
        }

        let mut response = if is_event_stream(&resp) {
            read_sse_response(resp, body).await?
        } else {
            read_response_text_limited(resp, "read body").await?
        };
        if method.as_deref() == Some("tools/list") {
            response = self.tool_headers.process_tools_list(response)?;
        }
        Ok(response)
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        let resp = self.post(body, false).await?;
        let status = resp.status();
        // Drain through the same bounded reader used for request bodies.
        // This both returns the connection to the pool and prevents a server
        // from turning notification acknowledgements into an unbounded read.
        drain_response_limited(resp, "read notification body").await?;
        if !status.is_success() {
            return Err(TransportError::Http(format!("status {}", status.as_u16())));
        }
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

async fn read_sse_response(
    response: reqwest::Response,
    request_body: &str,
) -> Result<String, TransportError> {
    let expected_id = serde_json::from_str::<Value>(request_body)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .ok_or_else(|| TransportError::Other("HTTP request is missing a JSON-RPC id".into()))?;
    reject_announced_oversize(&response)?;
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    let mut event_start = 0usize;
    let mut line_start = 0usize;
    let mut scan_offset = 0usize;
    let mut skipped = 0usize;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk
            .map_err(|e| TransportError::Http(format!("read SSE body: {}", e.without_url())))?;
        append_chunk_limited(&mut body, &chunk)?;
        if let Some(response) = scan_sse_events(
            &body,
            &mut event_start,
            &mut line_start,
            &mut scan_offset,
            &expected_id,
            &mut skipped,
            false,
        )? {
            return Ok(response);
        }
    }
    if let Some(response) = scan_sse_events(
        &body,
        &mut event_start,
        &mut line_start,
        &mut scan_offset,
        &expected_id,
        &mut skipped,
        true,
    )? {
        return Ok(response);
    }
    Err(TransportError::Closed)
}

fn reject_announced_oversize(response: &reqwest::Response) -> Result<(), TransportError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64)
    {
        return Err(response_too_large());
    }
    Ok(())
}

fn response_too_large() -> TransportError {
    TransportError::Http(format!(
        "response body exceeds {MAX_HTTP_RESPONSE_BYTES}-byte limit"
    ))
}

fn append_chunk_limited(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), TransportError> {
    if chunk.len() > MAX_HTTP_RESPONSE_BYTES.saturating_sub(body.len()) {
        return Err(response_too_large());
    }
    let new_len = body.len() + chunk.len();
    if new_len > body.capacity() {
        body.try_reserve_exact(chunk.len())
            .map_err(|_| TransportError::Http("response body allocation failed".into()))?;
    }
    body.extend_from_slice(chunk);
    Ok(())
}

async fn read_response_bytes_limited(
    response: reqwest::Response,
    context: &'static str,
) -> Result<Vec<u8>, TransportError> {
    reject_announced_oversize(&response)?;
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk =
            chunk.map_err(|e| TransportError::Http(format!("{context}: {}", e.without_url())))?;
        append_chunk_limited(&mut body, &chunk)?;
    }
    Ok(body)
}

async fn read_response_text_limited(
    response: reqwest::Response,
    context: &'static str,
) -> Result<String, TransportError> {
    let body = read_response_bytes_limited(response, context).await?;
    String::from_utf8(body)
        .map_err(|_| TransportError::Http(format!("{context}: response body is not UTF-8")))
}

async fn drain_response_limited(
    response: reqwest::Response,
    context: &'static str,
) -> Result<(), TransportError> {
    reject_announced_oversize(&response)?;
    let mut chunks = response.bytes_stream();
    let mut received = 0usize;
    while let Some(chunk) = chunks.next().await {
        let chunk =
            chunk.map_err(|e| TransportError::Http(format!("{context}: {}", e.without_url())))?;
        if chunk.len() > MAX_HTTP_RESPONSE_BYTES.saturating_sub(received) {
            return Err(response_too_large());
        }
        received += chunk.len();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_sse_events(
    body: &[u8],
    event_start: &mut usize,
    line_start: &mut usize,
    scan_offset: &mut usize,
    expected_id: &Value,
    skipped: &mut usize,
    eof: bool,
) -> Result<Option<String>, TransportError> {
    while *scan_offset < body.len() {
        let delimiter = match body[*scan_offset] {
            b'\n' => Some((*scan_offset, *scan_offset + 1)),
            b'\r' if *scan_offset + 1 < body.len() => {
                let next = if body[*scan_offset + 1] == b'\n' {
                    *scan_offset + 2
                } else {
                    *scan_offset + 1
                };
                Some((*scan_offset, next))
            }
            b'\r' if eof => Some((*scan_offset, *scan_offset + 1)),
            b'\r' => None,
            _ => {
                *scan_offset += 1;
                continue;
            }
        };
        let Some((line_end, next_line)) = delimiter else {
            break;
        };
        *scan_offset = next_line;
        if line_end == *line_start {
            if let Some(response) =
                parse_sse_event(&body[*event_start..*line_start], expected_id, skipped)?
            {
                return Ok(Some(response));
            }
            *event_start = next_line;
        }
        *line_start = next_line;
    }

    if eof && *event_start < body.len() {
        return parse_sse_event(&body[*event_start..], expected_id, skipped);
    }
    Ok(None)
}

fn parse_sse_event(
    event: &[u8],
    expected_id: &Value,
    skipped: &mut usize,
) -> Result<Option<String>, TransportError> {
    let event = std::str::from_utf8(event)
        .map_err(|_| TransportError::Other("streamable HTTP SSE read/parse failure".into()))?;
    let event = event.strip_prefix('\u{feff}').unwrap_or(event);
    let mut data = String::with_capacity(event.len());
    let mut has_data = false;
    let mut cursor = 0usize;
    while cursor < event.len() {
        let bytes = event.as_bytes();
        let mut line_end = cursor;
        while line_end < bytes.len() && !matches!(bytes[line_end], b'\r' | b'\n') {
            line_end += 1;
        }
        let line = &event[cursor..line_end];
        cursor = if line_end >= bytes.len() {
            line_end
        } else if bytes[line_end] == b'\r'
            && line_end + 1 < bytes.len()
            && bytes[line_end + 1] == b'\n'
        {
            line_end + 2
        } else {
            line_end + 1
        };

        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field != "data" {
            continue;
        }
        has_data = true;
        data.push_str(value.strip_prefix(' ').unwrap_or(value));
        data.push('\n');
    }
    if !has_data {
        return Ok(None);
    }
    data.pop();
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return Ok(None);
    };
    if value.get("id") == Some(expected_id)
        && (value.get("result").is_some() || value.get("error").is_some())
    {
        return Ok(Some(data));
    }
    *skipped += 1;
    if *skipped > MAX_SSE_NON_RESPONSE_EVENTS {
        return Err(TransportError::Other(format!(
            "streamable HTTP SSE emitted more than {MAX_SSE_NON_RESPONSE_EVENTS} non-response events"
        )));
    }
    Ok(None)
}

fn is_jsonrpc_error(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .is_some_and(|v| v.get("error").is_some() && v.get("id").is_some())
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
