//! Response classification, report-note rendering, and the config schema for
//! the [`super::Fuzzer`] scenario.
//!
//! Split out of `fuzzer.rs` to keep that file under the 300-line convention.

use serde_json::{Value, json};

use crate::scenario::ScenarioOutcome;
use mcp_loadtest_core::fuzz_report::{FuzzClass, FuzzReport};
use mcp_loadtest_protocol::SessionError;

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
            let class = if (-32603..=-32600).contains(&obj.code) {
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
        SessionError::IdMismatch { expected, got } => (
            FuzzClass::ParseError,
            None,
            format!("response id mismatch: expected {expected}, got {got}"),
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
        // fuzzer (unusual — the fuzzer's whole point is to send payloads
        // that violate the schema). Our own validator caught it before the
        // server saw it; surface it as a protocol issue with an explicit
        // "caught client-side" note so it's never misread as a server bug.
        SessionError::SchemaViolation { .. } => (
            FuzzClass::ProtocolError,
            None,
            format!("rejected client-side by strict validation: {err}"),
        ),
        // Strict-mode version gate (ADR 0018): produced at spawn time by the
        // run orchestrator, so fuzz iterations never hit it in practice —
        // classified as a protocol issue for exhaustiveness.
        SessionError::UnsupportedProtocolVersion { .. } => (
            FuzzClass::ProtocolError,
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
    if report.has_critical() {
        outcome
            .notes
            .push("fuzzer: CRITICAL findings — see deadlock/disconnected/parse rows".to_string());
    }
}
