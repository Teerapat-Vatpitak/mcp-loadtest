//! Transport abstraction — stdio / HTTP / SSE plug into [`crate::Session`]
//! through the same trait.
//!
//! See DESIGN.md §4 (architecture) and §6 (CLI surface).
//!
//! **M4 ownership:** integration agent owns this module + `stdio.rs`.
//! Agent J fills in `http.rs`. Agent K fills in `sse.rs`. Locked contracts:

pub mod guard;
pub mod http;
pub(crate) mod resolve;
pub mod spawn_options;
pub mod sse;
pub mod stdio;
pub mod ws;

pub use guard::HostGuard;

use std::io;

use async_trait::async_trait;
use thiserror::Error;

/// Errors a [`Transport`] may surface.
///
/// **Locked for M4.** New variants are non-breaking only if added at the end.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// I/O error from the underlying pipe / socket.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// HTTP error (status code, reqwest failure, etc.).
    #[error("http: {0}")]
    Http(String),
    /// Server closed the connection mid-call.
    #[error("connection closed")]
    Closed,
    /// Configured deadline exceeded.
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),
    /// Other transport-specific failure.
    #[error("{0}")]
    Other(String),
}

/// One end of a JSON-RPC link to an MCP server. Stdio / HTTP / SSE all
/// implement this; [`crate::Session`] is generic over `Box<dyn Transport>`.
///
/// **Locked for M4.** Method signatures are stable across patch versions.
#[async_trait]
pub trait Transport: Send {
    /// Send a request body (single JSON-RPC object, no trailing newline) and
    /// await the matching response body. Implementations are responsible for
    /// any wire-format wrapping (newline framing for stdio, POST + body for
    /// HTTP, POST + SSE filter for SSE).
    ///
    /// The returned string is the raw JSON-RPC response object (no framing).
    async fn request(&mut self, body: &str) -> Result<String, TransportError>;

    /// Send a notification body (no `id`, no response expected).
    async fn notify(&mut self, body: &str) -> Result<(), TransportError>;

    /// Send **raw, unframed bytes** on the wire, bypassing JSON-RPC
    /// serialization entirely.
    ///
    /// This is the fuzzer's escape hatch for putting deliberately malformed
    /// data on the connection — broken framing, invalid UTF-8, truncated or
    /// oversized frames — that the typed [`Transport::request`] /
    /// [`Transport::notify`] paths cannot express. The bytes are written
    /// **verbatim**; the implementation decides only how to delimit them (the
    /// stdio impl appends a single newline so a line-framed peer sees one
    /// malformed frame).
    ///
    /// No response is read: after a raw send the wire may be desynced, so the
    /// caller must treat the connection as poisoned and reconnect before the
    /// next typed call.
    ///
    /// The default returns [`TransportError::Other`] — raw sends are only
    /// meaningful for byte-stream transports. Non-stream transports
    /// (HTTP / SSE / WebSocket) inherit this default, and callers (the fuzzer)
    /// record those iterations as skipped rather than sent.
    async fn raw_send(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let _ = bytes;
        Err(TransportError::Other(
            "raw_send unsupported on this transport".into(),
        ))
    }

    /// PID of the underlying process if applicable. Stdio knows; HTTP / SSE
    /// don't (would require a server-reported field, which MCP doesn't have).
    fn pid(&self) -> Option<u32> {
        None
    }

    /// Record the negotiated MCP protocol version once `initialize`
    /// completes. Transports that carry the version out-of-band override
    /// this — Streamable HTTP attaches the `MCP-Protocol-Version` header
    /// (required from the 2025-06-18 revision) to every subsequent request.
    /// The default is a no-op for wire formats with no such channel
    /// (stdio / SSE / WS).
    fn set_protocol_version(&mut self, _version: &str) {}

    /// Close the transport gracefully. Implementations are bounded by their
    /// own internal timeouts — the orchestrator wraps this in an outer
    /// timeout via `tokio::time::timeout` already.
    async fn shutdown(self: Box<Self>) -> Result<(), TransportError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal transport implementing only the required methods, so it
    /// inherits the default [`Transport::raw_send`] — the same position
    /// HTTP / SSE / WebSocket are in.
    struct NoRawTransport;

    #[async_trait]
    impl Transport for NoRawTransport {
        async fn request(&mut self, _body: &str) -> Result<String, TransportError> {
            Ok(String::new())
        }
        async fn notify(&mut self, _body: &str) -> Result<(), TransportError> {
            Ok(())
        }
        async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_raw_send_reports_unsupported() {
        let mut t = NoRawTransport;
        let err = t
            .raw_send(b"anything")
            .await
            .expect_err("default raw_send must error");
        match err {
            TransportError::Other(msg) => {
                assert!(msg.contains("raw_send unsupported"), "unexpected: {msg}");
            }
            other => panic!("expected TransportError::Other, got {other:?}"),
        }
    }
}
