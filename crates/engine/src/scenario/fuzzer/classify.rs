//! Response classification, report-note rendering, and the config schema for
//! the [`super::Fuzzer`] scenario.
//!
//! Split out of `fuzzer.rs` to keep that file under the 300-line convention.

use serde_json::{Value, json};

use crate::scenario::ScenarioOutcome;
use mcp_loadtest_core::fuzz_report::{FuzzClass, FuzzReport, is_expected_rejection_code};
use mcp_loadtest_protocol::SessionError;
use mcp_loadtest_protocol::mcp::CallToolResult;

/// JSON schema for the fuzzer's TOML config block, surfaced by `example-config`.
pub(super) fn config_schema() -> Value {
    json!({
        "type": "object",
        "title": "Fuzzer",
        "description": "Sends malformed payloads and classifies the responses.",
        "properties": {
            "iterations": {
                "type": "integer",
                "minimum": 1,
                "default": 50,
                "description": "Number of fuzz iterations."
            },
            "seed": {
                "type": "integer",
                "default": 0xC0FFEE,
                "description": "RNG seed for reproducible payload selection."
            },
            "payloads": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional explicit list of payload labels to draw from. Empty = all."
            }
        },
        "required": ["iterations"]
    })
}

/// Map a [`SessionError`] to a [`FuzzClass`] + optional JSON-RPC code +
/// short diagnostic note.
pub(super) fn classify_err(err: &SessionError) -> (FuzzClass, Option<i64>, String) {
    match err {
        SessionError::Server(obj) => {
            let class = if is_expected_rejection_code(obj.code) {
                FuzzClass::ProtocolError
            } else {
                FuzzClass::ServerError
            };
            (
                class,
                Some(obj.code),
                format!("server error {}: {}", obj.code, obj.message),
            )
        }
        SessionError::Json(e) => (
            FuzzClass::ParseError,
            None,
            format!("client-side json parse failed: {e}"),
        ),
        SessionError::ResponseShape(e) => (
            FuzzClass::ParseError,
            None,
            format!("MCP response shape error: {e}"),
        ),
        SessionError::IdMismatch { expected, got } => (
            FuzzClass::ParseError,
            None,
            format!("response id mismatch: expected {expected}, got {got}"),
        ),
        SessionError::InvalidResponseId { expected, got } => (
            FuzzClass::ParseError,
            None,
            format!("response id mismatch: expected {expected}, got {got}"),
        ),
        SessionError::MismatchedSuccessResponse { expected, got, .. } => (
            FuzzClass::ParseError,
            None,
            format!("success response id mismatch: expected {expected}, got {got}"),
        ),
        SessionError::MismatchedErrorResponse {
            expected,
            got,
            error,
        } => (
            FuzzClass::ParseError,
            Some(error.code),
            format!(
                "error response id mismatch: expected {expected}, got {got}; code {}",
                error.code
            ),
        ),
        SessionError::InvalidJsonRpcVersion { got } => (
            FuzzClass::ParseError,
            None,
            format!("invalid JSON-RPC response version `{got}`"),
        ),
        SessionError::Transport(_) | SessionError::Io(_) => (
            FuzzClass::Disconnected,
            None,
            format!("transport / io error: {err}"),
        ),
        SessionError::StartupTimeout(_) => (
            FuzzClass::Disconnected,
            None,
            format!("startup timeout: {err}"),
        ),
        // Only reachable if strict validation is enabled alongside the
        // fuzzer. Our own validator caught it before the server saw it, so it
        // is not the expected server-side protocol rejection.
        SessionError::SchemaViolation { .. } => (
            FuzzClass::Other,
            None,
            format!("rejected client-side by strict validation: {err}"),
        ),
        // Strict-mode version gate (ADR 0018): produced at spawn time by the
        // run orchestrator, so fuzz iterations never hit it in practice —
        // classified as a protocol issue for exhaustiveness.
        SessionError::UnsupportedProtocolVersion { .. } => (
            FuzzClass::Other,
            None,
            format!("unsupported protocol version negotiated: {err}"),
        ),
        // `SessionError` is `#[non_exhaustive]` (mcp-loadtest-protocol): a
        // cross-crate wildcard is mandatory. Known variants are classified
        // above; anything new is surfaced as a generic server error.
        _ => (
            FuzzClass::ServerError,
            None,
            format!("unclassified session error: {err}"),
        ),
    }
}

/// Whether a successful JSON-RPC `tools/call` envelope explicitly rejected
/// the deliberately malformed fuzz input.
///
/// MCP reports tool-level failures inside a successful JSON-RPC result using
/// `isError: true`; treating that as server acceptance would turn a compliant
/// rejection into a false failure.
pub(super) fn is_expected_tool_rejection(result: &CallToolResult) -> bool {
    result.is_error
}

/// Append a few rendered lines of the aggregated [`FuzzReport`] into the
/// outcome's `notes` for callers that don't run a custom renderer.
pub(super) fn push_report_notes(outcome: &mut ScenarioOutcome, report: &FuzzReport) {
    outcome
        .notes
        .push(format!("fuzzer: {} iterations classified", report.total));
    for (class, count) in &report.by_class {
        outcome.notes.push(format!("  {:?}: {}", class, count));
    }
    if !report.by_code.is_empty() {
        outcome.notes.push("  jsonrpc codes:".to_string());
        for (code, count) in &report.by_code {
            outcome.notes.push(format!("    {code}: {count}"));
        }
    }
    // Retain a bounded sample of non-healthy findings in the ordinary
    // ScenarioOutcome so CI logs explain *why* a fuzz run failed without
    // requiring a separate custom renderer.
    for finding in report
        .findings
        .iter()
        .filter(|finding| finding.class != FuzzClass::ProtocolError)
        .take(8)
    {
        outcome.notes.push(format!(
            "  finding payload={} class={:?} code={} note={}",
            finding.payload,
            finding.class,
            finding
                .code
                .map_or_else(|| "none".to_owned(), |code| code.to_string()),
            finding.note
        ));
    }
    if report.has_critical() {
        outcome
            .notes
            .push("fuzzer: CRITICAL findings — see deadlock/disconnected/parse rows".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_loadtest_protocol::jsonrpc::ErrorObject;
    use mcp_loadtest_protocol::mcp::Content;

    fn server_error(code: i64) -> SessionError {
        SessionError::Server(ErrorObject {
            code,
            message: format!("code {code}"),
            data: None,
        })
    }

    fn tool_result(is_error: bool) -> CallToolResult {
        CallToolResult {
            content: vec![Content::Text {
                text: "result".to_owned(),
            }],
            is_error,
            structured_content: None,
            meta: None,
        }
    }

    #[test]
    fn expected_jsonrpc_rejection_boundaries_are_exact() {
        for code in [-32700, -32600, -32601, -32602] {
            let (class, actual, _) = classify_err(&server_error(code));
            assert_eq!(class, FuzzClass::ProtocolError, "code {code}");
            assert_eq!(actual, Some(code));
        }

        for code in [-32603, -32599, -32000] {
            let (class, actual, _) = classify_err(&server_error(code));
            assert_eq!(class, FuzzClass::ServerError, "code {code}");
            assert_eq!(actual, Some(code));
        }
    }

    #[test]
    fn tools_call_is_error_is_an_explicit_expected_rejection() {
        assert!(is_expected_tool_rejection(&tool_result(true)));
        assert!(!is_expected_tool_rejection(&tool_result(false)));
    }
}
