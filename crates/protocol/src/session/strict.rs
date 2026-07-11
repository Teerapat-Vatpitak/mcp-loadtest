//! Opt-in strict schema validation glue for [`Session::call_tool`] — the
//! args side (ADR 0010) and the result side (DESIGN §13.1 item 2).
//!
//! Split out of `session.rs` to keep that file within the size convention.
//! No policy lives here: every decision is delegated to
//! [`schema::classify_schema_violation`] and this module mechanically
//! applies whatever arm it returns, so the policy function stays the single
//! source of truth (notably: results are **non-gating** — `Warn`).
//!
//! [`Session::call_tool`]: super::Session::call_tool

use std::collections::HashMap;

use serde_json::Value;

use super::SessionError;
use crate::mcp::CallToolResult;
use crate::schema::{self, SchemaPolicy, SchemaViolation, ValidationSite};

/// Validate `arguments` against the tool's advertised `inputSchema` (if one
/// is registered) and apply the [`ValidationSite::ToolCallArgs`] policy.
/// A tool absent from `schemas` is never validated — a server that doesn't
/// advertise a schema is not failed on that ground (ADR 0005).
pub(super) fn check_args(
    schemas: &HashMap<String, Value>,
    tool: &str,
    arguments: &Value,
) -> Result<(), SessionError> {
    let Some(input_schema) = schemas.get(tool) else {
        return Ok(());
    };
    let violations = schema::validate(input_schema, arguments);
    apply_policy(ValidationSite::ToolCallArgs, tool, violations)
}

/// Validate a result's `structuredContent` against the tool's advertised
/// `outputSchema` (if one is registered) and apply the
/// [`ValidationSite::ToolCallResult`] policy.
///
/// Per the 2025-06-18 MCP spec, a tool that advertises `outputSchema` MUST
/// return `structuredContent` conforming to it — so a missing
/// `structuredContent` is itself a violation. The result is borrowed
/// immutably: the payload handed back to the caller is never altered.
pub(super) fn check_result(
    schemas: &HashMap<String, Value>,
    tool: &str,
    result: &CallToolResult,
) -> Result<(), SessionError> {
    let Some(output_schema) = schemas.get(tool) else {
        return Ok(());
    };
    let violations = match &result.structured_content {
        Some(content) => schema::validate(output_schema, content),
        None => vec![SchemaViolation {
            path: "<root>".to_string(),
            message: "tool advertises `outputSchema` but result carries no `structuredContent`"
                .to_string(),
        }],
    };
    apply_policy(ValidationSite::ToolCallResult, tool, violations)
}

/// Act on whatever [`schema::classify_schema_violation`] decides, arm by
/// arm. Every arm is handled for every site (even arms the current policy
/// never returns for that site) so a future policy change needs no
/// session-side edits.
fn apply_policy(
    site: ValidationSite,
    tool: &str,
    violations: Vec<SchemaViolation>,
) -> Result<(), SessionError> {
    match schema::classify_schema_violation(site, &violations) {
        SchemaPolicy::Fail => Err(SessionError::SchemaViolation {
            tool: tool.to_string(),
            summary: summarize(&violations),
        }),
        SchemaPolicy::Warn => {
            tracing::warn!(
                tool,
                site = ?site,
                violations = violations.len(),
                first = %summarize(&violations),
                "strict schema validation: payload does not match advertised schema \
                 (policy: warn, non-gating)"
            );
            Ok(())
        }
        SchemaPolicy::Ignore => Ok(()),
    }
}

/// First few violations as `path: message`, for error/log text.
fn summarize(violations: &[SchemaViolation]) -> String {
    violations
        .iter()
        .take(3)
        .map(|v| format!("{}: {}", v.path, v.message))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn output_registry() -> HashMap<String, Value> {
        HashMap::from([(
            "report".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "answer": { "type": "string" },
                    "count": { "type": "integer" }
                },
                "required": ["answer"]
            }),
        )])
    }

    fn args_registry() -> HashMap<String, Value> {
        HashMap::from([(
            "echo".to_string(),
            json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } },
                "required": ["msg"]
            }),
        )])
    }

    fn result_with(structured_content: Option<Value>) -> CallToolResult {
        CallToolResult {
            content: Vec::new(),
            is_error: false,
            structured_content,
        }
    }

    #[test]
    fn conformant_result_is_silent() {
        let result = result_with(Some(json!({ "answer": "forty-two", "count": 42 })));
        assert!(check_result(&output_registry(), "report", &result).is_ok());
    }

    #[test]
    fn violating_result_warns_but_does_not_error() {
        // Required `answer` missing AND `count` has the wrong type — yet the
        // result-side policy is Warn, so the call must NOT error.
        let result = result_with(Some(json!({ "count": "not-an-integer" })));
        assert!(check_result(&output_registry(), "report", &result).is_ok());
    }

    #[test]
    fn missing_structured_content_warns_but_does_not_error() {
        // Advertised `outputSchema` + absent `structuredContent` is a spec
        // violation, but result-side stays non-gating observability.
        let result = result_with(None);
        assert!(check_result(&output_registry(), "report", &result).is_ok());
    }

    #[test]
    fn result_payload_is_not_altered_by_validation() {
        let payload = json!({ "count": "not-an-integer" });
        let result = result_with(Some(payload.clone()));
        check_result(&output_registry(), "report", &result).expect("warn must not error");
        assert_eq!(
            result.structured_content,
            Some(payload),
            "validation must never mutate the result handed back to the caller"
        );
    }

    #[test]
    fn tool_without_registered_output_schema_is_never_result_validated() {
        let result = result_with(None);
        assert!(check_result(&output_registry(), "other-tool", &result).is_ok());
    }

    #[test]
    fn args_violation_still_fails_closed() {
        let err = check_args(&args_registry(), "echo", &json!({ "msg": 123 }))
            .expect_err("args side must keep gating");
        match err {
            SessionError::SchemaViolation { tool, summary } => {
                assert_eq!(tool, "echo");
                assert!(
                    summary.contains("msg"),
                    "summary should name the path: {summary}"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn args_tool_absent_from_registry_is_not_validated() {
        assert!(check_args(&args_registry(), "other-tool", &json!({})).is_ok());
    }
}
