//! [`Session`] — a single MCP session over any [`Transport`].
//!
//! Owns the JSON-RPC `id` counter, performs the `initialize` handshake, and
//! exposes `list_tools` / `call_tool` for higher layers (scenarios, CLI
//! subcommands). The wire-format details (stdio framing, HTTP POST, SSE
//! correlation) live behind [`Transport`].
//!
//! M1 scope: synchronous request/response only. Server-initiated
//! notifications and concurrent in-flight requests are deferred to M5+.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

use crate::jsonrpc::ErrorObject;
use crate::mcp::ProtocolVersion;
use crate::transport::{Transport, TransportError};

mod connection;
mod lifecycle;
mod request;
mod strict;
mod version;

/// Default time budget for the `initialize` round-trip during construct.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors returned by [`Session`] operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SessionError {
    /// I/O error from the underlying transport (pipe closed, write failed, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization or response-envelope parsing failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A valid JSON-RPC success envelope carried a result that did not match
    /// the shape required by the requested MCP method.
    ///
    /// Keeping this distinct from [`SessionError::Json`] lets diagnostics
    /// distinguish malformed wire JSON from a stale/desynchronized response
    /// that is valid JSON but belongs to another method.
    #[error("response shape: {0}")]
    ResponseShape(serde_json::Error),
    /// The server returned a structured JSON-RPC error.
    #[error(transparent)]
    Server(#[from] ErrorObject),
    /// Underlying transport (stdio / HTTP / SSE) failed.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    /// The server's response id didn't match the outgoing request's id.
    #[error("id mismatch: sent {expected}, got {got}")]
    IdMismatch {
        /// Id we sent.
        expected: u64,
        /// Id we received.
        got: u64,
    },
    /// The server returned a valid JSON-RPC id that cannot match this
    /// session's numeric outgoing request id (for example string or null).
    #[error("id mismatch: sent {expected}, got {got}")]
    InvalidResponseId {
        /// Numeric id sent by this session.
        expected: u64,
        /// Non-numeric or out-of-range id returned by the server.
        got: Value,
    },
    /// A success response belonged to a different request id.
    ///
    /// The opaque result is retained so the raw protocol fuzzer can
    /// distinguish “server accepted the malformed request” from a rejection;
    /// it is intentionally omitted from the Display string.
    #[error("id mismatch: sent {expected}, got {got} (success response)")]
    MismatchedSuccessResponse {
        /// Numeric id sent by this session.
        expected: u64,
        /// Id returned by the server.
        got: Value,
        /// Opaque success result returned for the other request.
        result: Value,
    },
    /// A structured JSON-RPC error response belonged to a different request
    /// id. Retaining the error code lets the raw fuzzer tell an expected
    /// malformed-input rejection from an internal server error.
    #[error("id mismatch: sent {expected}, got {got} ({error})")]
    MismatchedErrorResponse {
        /// Numeric id sent by this session.
        expected: u64,
        /// Id returned by the server.
        got: Value,
        /// Structured error returned for the other request.
        error: ErrorObject,
    },
    /// The response envelope was valid JSON but did not declare JSON-RPC 2.0.
    #[error("invalid JSON-RPC response version `{got}` (expected `2.0`)")]
    InvalidJsonRpcVersion {
        /// Version literal returned by the server.
        got: String,
    },
    /// `initialize` did not complete within the configured budget.
    #[error("server did not respond to initialize within {0:?}")]
    StartupTimeout(Duration),
    /// Strict validation rejected a `tools/call` payload: it didn't match
    /// the tool's advertised schema and the policy classified it as
    /// `SchemaPolicy::Fail`. Only produced when `[validation] strict =
    /// true`, and — under the current policy — only for the *args* side
    /// (result-side mismatches warn without gating; see
    /// `schema::classify_schema_violation`). Maps to
    /// `CallOutcome::ProtocolError`.
    #[error("schema violation for tool `{tool}`: {summary}")]
    SchemaViolation {
        /// Tool whose payload failed validation.
        tool: String,
        /// Human-readable summary of the violations (first few).
        summary: String,
    },
    /// The server answered `initialize` with a protocol revision outside the
    /// supported set (ADR 0018) **and** the run is under
    /// `[validation] strict = true`. The permissive default only warns —
    /// this error is produced by the run orchestrator, not by `Session`
    /// construction itself.
    #[error(
        "server negotiated unsupported protocol version `{got}` (client advertised `{advertised}`)"
    )]
    UnsupportedProtocolVersion {
        /// Version string the server answered with.
        got: String,
        /// Version the client advertised.
        advertised: String,
    },
}

/// A single MCP session over an opaque [`Transport`].
pub struct Session {
    transport: Box<dyn Transport>,
    next_id: u64,
    /// Reported by the server during `initialize` — kept for diagnostics.
    pub server_protocol_version: String,
    /// Revision advertised in `initialize` (config-pinned or the default).
    advertised_version: ProtocolVersion,
    /// Typed form of `server_protocol_version` when it parses to a supported
    /// revision; `None` means the server answered with an unknown version
    /// (warned at handshake; gates under strict — ADR 0018).
    negotiated_version: Option<ProtocolVersion>,
    /// `Some` switches this session to the stateless 2026-07-28 mode
    /// (ADR 0019): no handshake happened, and every outgoing request is
    /// wrapped with the `_meta` block these constants feed. `None` (the
    /// default) is the handshake mode, byte-for-byte its pre-existing self.
    stateless: Option<connection::StatelessMeta>,
    /// Strict args-validation registry: tool name → advertised
    /// `inputSchema`. `None` (the default) means strict validation is off
    /// and `call_tool` is byte-for-byte its pre-existing self — a single
    /// `Option` check on the hot path, no allocation, ADR 0006 preserved.
    /// `Some` is populated once at run start from the `tools/list` the run
    /// already fetches (see [`Session::set_strict_tool_schemas`]).
    tool_schemas: Option<HashMap<String, Value>>,
    /// Strict result-side registry: tool name → advertised `outputSchema`
    /// (DESIGN §13.1). Same `Option` hot-path discipline as
    /// `tool_schemas`; mismatches are non-gating under the current policy.
    tool_output_schemas: Option<HashMap<String, Value>>,
}
