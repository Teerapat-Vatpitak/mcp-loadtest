//! JSON-RPC 2.0 message types.
//!
//! Per spec ([JSON-RPC 2.0]) and MCP transport convention, messages are
//! line-delimited (one JSON object per line, no embedded newlines).
//!
//! [JSON-RPC 2.0]: https://www.jsonrpc.org/specification

use serde::{Deserialize, Deserializer, Serialize, de};
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
#[derive(Debug)]
pub(crate) struct ResponseEnvelope {
    /// JSON-RPC version, validated as exactly `"2.0"` by the session.
    pub jsonrpc: String,
    /// Response id. JSON-RPC permits string, number, or null; the session
    /// validates it against its numeric outgoing id after parsing so a
    /// well-formed but mismatched/null id is not misreported as malformed
    /// JSON.
    pub id: Value,
    /// Either a successful result or a structured error.
    pub payload: ResponsePayload,
}

impl<'de> Deserialize<'de> for ResponseEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawEnvelope {
            jsonrpc: String,
            id: Value,
            #[serde(flatten)]
            members: serde_json::Map<String, Value>,
        }

        let mut raw = RawEnvelope::deserialize(deserializer)?;
        let result = raw.members.remove("result");
        let error = raw.members.remove("error");
        let payload = match (result, error) {
            (Some(result), None) => ResponsePayload::Ok { result },
            (None, Some(error)) => ResponsePayload::Err {
                error: serde_json::from_value(error).map_err(de::Error::custom)?,
            },
            (Some(_), Some(_)) => {
                return Err(de::Error::custom(
                    "JSON-RPC response cannot contain both `result` and `error`",
                ));
            }
            (None, None) => {
                return Err(de::Error::custom(
                    "JSON-RPC response requires exactly one of `result` or `error`",
                ));
            }
        };
        Ok(Self {
            jsonrpc: raw.jsonrpc,
            id: raw.id,
            payload,
        })
    }
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
        assert_eq!(env.id, serde_json::json!(1));
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
        assert_eq!(env.id, serde_json::json!(2));
        match env.payload {
            ResponsePayload::Err { error } => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "method not found");
            }
            ResponsePayload::Ok { .. } => panic!("expected Err"),
        }
    }

    #[test]
    fn response_requires_exactly_one_result_or_error() {
        for raw in [
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603,"message":"bad"}}"#,
        ] {
            let error = serde_json::from_str::<ResponseEnvelope>(raw)
                .expect_err("ambiguous response envelope must be rejected");
            assert!(
                error.to_string().contains("result") && error.to_string().contains("error"),
                "unexpected diagnostic: {error}"
            );
        }
    }

    #[test]
    fn null_is_a_present_success_result() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let env: ResponseEnvelope = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            env.payload,
            ResponsePayload::Ok {
                result: Value::Null
            }
        ));
    }
}
