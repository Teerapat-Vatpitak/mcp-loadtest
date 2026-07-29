//! MCP-specific message types (initialize, tools/list, tools/call).
//!
//! See [the MCP spec][1] for authoritative definitions.
//!
//! [1]: https://modelcontextprotocol.io/specification/

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;

/// Wire string reported as our own `protocolVersion` by the self-hosted
/// `serve` mode. Client-side advertising goes through
/// [`ProtocolVersion::DEFAULT_ADVERTISED`] (or a config pin) instead.
pub const PROTOCOL_VERSION: &str = ProtocolVersion::DEFAULT_ADVERTISED.as_str();

pub use mcp_loadtest_core::version::ProtocolVersion;

/// Parameters sent in the `initialize` request.
#[derive(Debug, Serialize)]
pub(crate) struct InitializeParams {
    /// Protocol version this client speaks.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Client capabilities (typically empty for a load tester).
    pub capabilities: Value,
    /// Identifies the client to the server.
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// Client identity sent during initialize.
#[derive(Debug, Serialize)]
pub(crate) struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Client version.
    pub version: String,
}

/// Result returned by the server's `initialize`.
#[expect(
    dead_code,
    reason = "serde-populated wire type; only `protocol_version` is read, the rest round-trip losslessly for forward-compat and dead_code can't see serde"
)]
#[derive(Debug, Deserialize)]
pub(crate) struct InitializeResult {
    /// Spec version the server speaks.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Server capabilities.
    #[serde(default)]
    pub capabilities: Value,
    /// Optional server identity.
    #[serde(rename = "serverInfo", default)]
    pub server_info: Option<ServerInfo>,
}

/// Server identity returned in initialize.
#[expect(
    dead_code,
    reason = "serde-populated but never read (we trust the server); kept on the wire type for forward-compat / future surfacing"
)]
#[derive(Debug, Deserialize)]
pub(crate) struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
}

/// Result of `server/discover` (2026-07-28 stateless core, ADR 0019).
///
/// Wire shape reconciled against the official final specification at
/// `5f5440bb26a62e2cf3440b92da5a667efa03b267`. Its only schema delta from
/// the previously pinned snapshot affects the currently unsupported
/// `subscriptions/listen` method, so this implemented subset is unchanged.
#[expect(
    dead_code,
    reason = "serde-populated; only the version fields are read today, the rest round-trip losslessly for forward-compat and dead_code can't see serde"
)]
#[derive(Debug, Deserialize)]
pub(crate) struct DiscoverResult {
    /// Full list of revisions the server supports.
    #[serde(rename = "supportedVersions")]
    pub supported_versions: Vec<String>,
    /// Server capabilities.
    #[serde(default)]
    pub capabilities: Value,
    /// Ordinary results carry `resultType = "complete"`. Missing remains
    /// accepted because the spec requires clients to treat older results
    /// without this field as complete.
    #[serde(rename = "resultType", default)]
    pub result_type: Option<String>,
    /// Result metadata, including optional
    /// `io.modelcontextprotocol/serverInfo`.
    #[serde(rename = "_meta", default)]
    pub meta: Value,
}

/// Result of `tools/list`.
#[derive(Debug, Deserialize)]
pub(crate) struct ListToolsResult {
    /// The tools the server exposes.
    pub tools: Vec<Tool>,
}

/// A single tool exposed by an MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    /// Tool name (as called via `tools/call`).
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing valid arguments.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
    /// JSON Schema describing the tool's structured output, if advertised.
    /// Per the 2025-06-18 MCP spec a tool MAY advertise `outputSchema`;
    /// when it does, its results MUST carry conforming `structuredContent`.
    #[serde(rename = "outputSchema", default)]
    pub output_schema: Option<Value>,
}

/// Parameters for a `tools/call` request.
///
/// Borrowed in both fields so [`crate::Session::call_tool`] can wrap caller
/// arguments without copying. Scenarios that drive `tools/call` in a tight
/// loop now pay zero JSON allocation for params construction.
#[derive(Serialize)]
pub(crate) struct CallToolParams<'a> {
    /// Tool name to invoke.
    pub name: &'a str,
    /// JSON-shaped arguments.
    pub arguments: &'a Value,
    /// Opaque server state returned by an earlier input-required round.
    #[serde(rename = "requestState", skip_serializing_if = "Option::is_none")]
    pub request_state: Option<&'a str>,
    /// Client responses keyed by the server's input-request identifiers.
    #[serde(rename = "inputResponses", skip_serializing_if = "Option::is_none")]
    pub input_responses: Option<&'a Value>,
}

/// Result returned by `tools/call`.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
    /// Protocol result metadata. MCP reserves `_meta` for implementation and
    /// transport hints that clients should preserve.
    #[serde(rename = "_meta", default)]
    pub meta: Option<Value>,
    /// Tool output content (text/image/resource).
    pub content: Vec<Content>,
    /// True if the tool reported a logical error (vs. JSON-RPC error).
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    /// Structured output conforming to the tool's advertised `outputSchema`,
    /// if the server sent one (2025-06-18 MCP spec addition).
    #[serde(rename = "structuredContent", default)]
    pub structured_content: Option<Value>,
}

/// A server response requesting another round trip before a tool call can finish.
#[derive(Debug, Clone, Deserialize)]
pub struct InputRequiredResult {
    /// Protocol result metadata.
    #[serde(rename = "_meta", default)]
    pub meta: Option<Value>,
    /// Server-initiated requests keyed by server-assigned identifiers.
    #[serde(rename = "inputRequests", default)]
    pub input_requests: Option<serde_json::Map<String, Value>>,
    /// Opaque state to return unchanged on the next round.
    #[serde(rename = "requestState", default)]
    pub request_state: Option<String>,
    /// Result discriminator, required to be `input_required`.
    #[serde(rename = "resultType")]
    pub result_type: String,
}

/// One round of an MCP 2026-07-28 tool call.
#[derive(Debug, Clone)]
pub enum ToolCallRound {
    /// The tool call completed.
    Complete(CallToolResult),
    /// The server needs client input before continuing.
    InputRequired(InputRequiredResult),
}

impl<'de> Deserialize<'de> for ToolCallRound {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value.get("resultType").and_then(Value::as_str) {
            Some("input_required") => {
                let input: InputRequiredResult =
                    serde_json::from_value(value).map_err(de::Error::custom)?;
                if input.request_state.is_none() && input.input_requests.is_none() {
                    return Err(de::Error::custom(
                        "input_required result needs requestState or inputRequests",
                    ));
                }
                Ok(Self::InputRequired(input))
            }
            Some("complete") | None => serde_json::from_value(value)
                .map(Self::Complete)
                .map_err(de::Error::custom),
            Some(other) => Err(de::Error::custom(format!(
                "unsupported tools/call resultType `{other}`"
            ))),
        }
    }
}

/// A piece of content returned by a tool call.
///
/// The explicit `Text` / `Image` variants retain the original ergonomic API
/// for their minimal wire shapes. Content with optional metadata, newer MCP
/// variants, or vendor extensions is kept field-for-field as parsed JSON in
/// [`Content::Raw`]. This matters to callers such as the race detector:
/// collapsing two distinct forward-compatible responses into one catch-all
/// value would produce a false "no divergence" result.
#[derive(Debug, Clone)]
pub enum Content {
    /// Plain text content.
    Text {
        /// The text payload.
        text: String,
    },
    /// Image content as base64.
    Image {
        /// Base64-encoded image bytes.
        data: String,
        /// MIME type (e.g. `image/png`).
        mime_type: String,
    },
    /// A valid content object whose complete wire shape must be preserved.
    ///
    /// This includes audio, resource links, embedded resources, unknown future
    /// variants, and otherwise-known text/image blocks carrying annotations,
    /// `_meta`, or extension fields.
    Raw(Value),
    /// Legacy programmatic catch-all retained for source compatibility.
    ///
    /// Wire deserialization uses [`Content::Raw`] instead so information is
    /// never discarded.
    Other,
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| de::Error::custom("MCP content block must be an object"))?;
        let content_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("MCP content block requires string field `type`"))?;

        match content_type {
            "text" => {
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        de::Error::custom("MCP text content requires string field `text`")
                    })?
                    .to_owned();
                if object.len() == 2 {
                    Ok(Self::Text { text })
                } else {
                    Ok(Self::Raw(value))
                }
            }
            "image" => {
                let data = object
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        de::Error::custom("MCP image content requires string field `data`")
                    })?
                    .to_owned();
                let mime_type = object
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        de::Error::custom("MCP image content requires string field `mimeType`")
                    })?
                    .to_owned();
                if object.len() == 3 {
                    Ok(Self::Image { data, mime_type })
                } else {
                    Ok(Self::Raw(value))
                }
            }
            _ => Ok(Self::Raw(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ProtocolVersion's own unit tests (round-trip, stateless-set membership,
    // unknown-string parsing) live in `mcp_loadtest_core::version` now that
    // the type moved there. This test stays here because it also exercises
    // `PROTOCOL_VERSION`, a wire constant local to this module.
    #[test]
    fn default_advertised_is_2025_11_25() {
        // Deliberately advanced in T1.2 (spec gap audit cleared it). Changing
        // this again is a user-visible wire change: CHANGELOG + audit first.
        assert_eq!(ProtocolVersion::DEFAULT_ADVERTISED.as_str(), "2025-11-25");
        assert_eq!(PROTOCOL_VERSION, "2025-11-25");
    }

    #[test]
    fn parse_call_tool_result_text() {
        let raw = r#"{"content":[{"type":"text","text":"hello"}]}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.content.len(), 1);
        assert!(!result.is_error);
        match &result.content[0] {
            Content::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_tool_result_unknown_content_kind() {
        // Forward-compat: unknown content types retain their complete JSON
        // shape instead of collapsing into a unit catch-all.
        let raw = r#"{"content":[{"type":"resource","uri":"file:///x"}]}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        match &result.content[0] {
            Content::Raw(value) => {
                assert_eq!(value["type"], "resource");
                assert_eq!(value["uri"], "file:///x");
            }
            other => panic!("expected raw content, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_tool_result_preserves_metadata_on_known_content() {
        let raw = r#"{"content":[{"type":"text","text":"hello",
            "annotations":{"priority":0.9},"_meta":{"cacheKey":"a"}}]}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        match &result.content[0] {
            Content::Raw(value) => {
                assert_eq!(value["text"], "hello");
                assert_eq!(value["annotations"]["priority"], 0.9);
                assert_eq!(value["_meta"]["cacheKey"], "a");
            }
            other => panic!("expected metadata-bearing content to stay raw, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_tool_result_preserves_result_meta() {
        let raw = r#"{"_meta":{"requestId":"abc","cache":{"hit":true}},
            "content":[{"type":"text","text":"hello"}]}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        let meta = result.meta.expect("top-level _meta should be preserved");
        assert_eq!(meta["requestId"], "abc");
        assert_eq!(meta["cache"]["hit"], true);
    }

    #[test]
    fn list_tools_result_parses() {
        let raw = r#"{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}"#;
        let result: ListToolsResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "echo");
        // No `outputSchema` advertised → None (additive, forward-compatible).
        assert!(result.tools[0].output_schema.is_none());
    }

    #[test]
    fn tool_output_schema_parses_when_advertised() {
        let raw = r#"{"tools":[{"name":"report","inputSchema":{"type":"object"},
            "outputSchema":{"type":"object","required":["answer"]}}]}"#;
        let result: ListToolsResult = serde_json::from_str(raw).unwrap();
        let schema = result.tools[0]
            .output_schema
            .as_ref()
            .expect("outputSchema");
        assert_eq!(schema["required"][0], "answer");
    }

    #[test]
    fn call_tool_result_structured_content_parses() {
        let raw = r#"{"content":[{"type":"text","text":"{}"}],
            "structuredContent":{"answer":"forty-two"}}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        let sc = result.structured_content.expect("structuredContent");
        assert_eq!(sc["answer"], "forty-two");
    }

    #[test]
    fn call_tool_result_without_structured_content_defaults_to_none() {
        let raw = r#"{"content":[{"type":"text","text":"hello"}]}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        assert!(result.structured_content.is_none());
    }
}
