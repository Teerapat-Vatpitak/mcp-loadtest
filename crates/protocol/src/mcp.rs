//! MCP-specific message types (initialize, tools/list, tools/call).
//!
//! See [the MCP spec][1] for authoritative definitions.
//!
//! [1]: https://modelcontextprotocol.io/specification/

use serde::{Deserialize, Serialize};
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
/// Parsed tolerantly against the **release candidate** shape: every field is
/// optional/defaulted so a server reporting more or fewer fields still
/// parses. Field names re-verified against the final spec on 2026-07-29.
#[expect(
    dead_code,
    reason = "serde-populated; only the version fields are read today, the rest round-trip losslessly for forward-compat and dead_code can't see serde"
)]
#[derive(Debug, Deserialize)]
pub(crate) struct DiscoverResult {
    /// Preferred revision the server speaks, if reported.
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: Option<String>,
    /// Full list of revisions the server supports, if reported.
    #[serde(rename = "protocolVersions", default)]
    pub protocol_versions: Vec<String>,
    /// Server capabilities.
    #[serde(default)]
    pub capabilities: Value,
    /// Optional server identity.
    #[serde(rename = "serverInfo", default)]
    pub server_info: Option<ServerInfo>,
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
}

/// Result returned by `tools/call`.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
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

/// A piece of content returned by a tool call.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Content {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        /// The text payload.
        text: String,
    },
    /// Image content as base64.
    #[serde(rename = "image")]
    Image {
        /// Base64-encoded image bytes.
        data: String,
        /// MIME type (e.g. `image/png`).
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Other content types we don't model yet — preserved as raw JSON for forward-compat.
    #[serde(other)]
    Other,
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
        // Forward-compat: unknown content types should round-trip into `Other`.
        let raw = r#"{"content":[{"type":"resource","uri":"file:///x"}]}"#;
        let result: CallToolResult = serde_json::from_str(raw).unwrap();
        assert!(matches!(result.content[0], Content::Other));
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
