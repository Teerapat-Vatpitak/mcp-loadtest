//! JSON-RPC 2.0 message types.
//!
//! Per spec ([JSON-RPC 2.0]) and MCP transport convention, messages are
//! line-delimited (one JSON object per line, no embedded newlines).
//!
//! [JSON-RPC 2.0]: https://www.jsonrpc.org/specification

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// JSON-RPC version literal — always `"2.0"`.
pub const JSONRPC_VERSION: &str = "2.0";

/// Outgoing request to the server. The `id` correlates with [`ResponseEnvelope::id`].
///
/// Borrowed in both `method` and `params` so the hot path (every `tools/call`)
/// can serialize without first materializing an intermediate `Value` tree or
/// cloning the method name. `P: ?Sized + Serialize` so callers can pass
/// `&str`, `&serde_json::Value`, or any custom `Serialize` impl directly.
#[derive(Serialize)]
pub(crate) struct OutgoingRequest<'a, P: ?Sized + Serialize> {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Request id; the server echoes this in its response.
    pub id: u64,
    /// JSON-RPC method name (e.g. `"tools/call"`).
    pub method: &'a str,
    /// Method-specific parameters.
    pub params: &'a P,
}

/// Outgoing notification — like a request but without an `id`, no response expected.
///
/// Same zero-copy treatment as [`OutgoingRequest`].
#[derive(Serialize)]
pub(crate) struct OutgoingNotification<'a, P: ?Sized + Serialize> {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// JSON-RPC method name.
    pub method: &'a str,
    /// Method-specific parameters.
    pub params: &'a P,
}

/// A response from the server, carrying either a result or an error.
#[expect(
    dead_code,
    reason = "serde populates `jsonrpc` via Deserialize but we never read it (we trust the server); dead_code can't see the runtime use"
)]
#[derive(Debug, Deserialize)]
pub(crate) struct ResponseEnvelope {
    /// JSON-RPC version (always `"2.0"` from compliant servers).
    pub jsonrpc: String,
    /// Response id, matching the corresponding request's id.
    pub id: u64,
    /// Either a successful result or a structured error.
    #[serde(flatten)]
    pub payload: ResponsePayload,
}

/// The success-or-error half of a JSON-RPC response.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ResponsePayload {
    /// Successful result; opaque JSON value to be deserialized by the caller.
    Ok {
        /// Successful result payload.
        result: Value,
    },
    /// Server returned a structured error.
    Err {
        /// The JSON-RPC error object.
        error: ErrorObject,
    },
}

/// JSON-RPC structured error.
///
/// Standard codes per spec:
/// - `-32700` Parse error
/// - `-32600` Invalid request
/// - `-32601` Method not found
/// - `-32602` Invalid params
/// - `-32603` Internal error
/// - `-32000..=-32099` Server-defined errors
#[derive(Debug, Clone, Deserialize, Serialize, Error)]
#[error("JSON-RPC error {code}: {message}")]
pub struct ErrorObject {
    /// Numeric error code.
    pub code: i64,
    /// Short human-readable description.
    pub message: String,
    /// Optional structured payload with additional info.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_ok() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"foo":"bar"}}"#;
        let env: ResponseEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.id, 1);
        match env.payload {
            ResponsePayload::Ok { result } => {
                assert_eq!(result["foo"], "bar");
            }
            ResponsePayload::Err { .. } => panic!("expected Ok"),
        }
    }

    #[test]
    fn parse_response_err() {
        let raw =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}"#;
        let env: ResponseEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.id, 2);
        match env.payload {
            ResponsePayload::Err { error } => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "method not found");
            }
            ResponsePayload::Ok { .. } => panic!("expected Err"),
        }
    }
}
