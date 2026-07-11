//! Dependency-free JSON Schema **subset** validator for MCP tool
//! `inputSchema`s, plus the opt-in policy that decides what a mismatch
//! *means* to the load test.
//!
//! # Why a subset, and why no `jsonschema` crate
//!
//! MCP `inputSchema`s in the wild use a small, stable slice of JSON Schema:
//! `type`, `properties`, `required`, `enum`, `items`, and nesting. Pulling a
//! full validator crate would add a transitive dependency tree we can't
//! `cargo deny`-audit in every dev environment, for coverage we don't need.
//! See ADR 0010.
//!
//! # Forward-compatibility (ADR 0005)
//!
//! [`validate`] **never rejects on a keyword it does not model** — unknown
//! keywords are skipped, not failed. Strictness is therefore opt-in *policy*
//! ([`classify_schema_violation`]), not a hard protocol change: the default
//! build behaves exactly as before unless an operator turns it on.
//!
//! # Dialect
//!
//! MCP 2025-11-25 establishes JSON Schema **2020-12** as the default dialect.
//! The keywords this subset models (`type`/`properties`/`required`/`enum`/
//! `items`) are semantically identical across draft-07 and 2020-12, so the
//! validator is dialect-stable; dialect-specific keywords (`$schema`,
//! `$defs`, `prefixItems`, …) fall under the skip-unmodeled rule above.

use serde_json::Value;

/// One schema mismatch found while validating an instance against a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    /// Dotted location of the offending value, e.g. `args.timeout`.
    pub path: String,
    /// Human-readable description of what was wrong.
    pub message: String,
}

/// Where a validation happened — tool-call **arguments** (what we send) or
/// the tool-call **result** (what the server sent back). The policy can
/// treat the two sides differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationSite {
    /// Arguments we are about to send in a `tools/call`.
    ToolCallArgs,
    /// The `result` payload a `tools/call` returned.
    ToolCallResult,
}

/// What the load tester should DO about a set of violations.
///
/// This is the gate-shaping knob: [`SchemaPolicy::Fail`] makes a mismatch
/// count against the run (it maps to `CallOutcome::ProtocolError`), while
/// [`SchemaPolicy::Warn`]/[`SchemaPolicy::Ignore`] keep the run green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaPolicy {
    /// Mismatch is a protocol error — gate the run on it.
    Fail,
    /// Record/log the mismatch but do not gate.
    Warn,
    /// Ignore the mismatch entirely.
    Ignore,
}

/// Max schema nesting depth we descend. The `inputSchema` is advertised by
/// the (untrusted) server under test; a maliciously deep schema would blow
/// the stack (a Rust stack overflow is an uncatchable abort, not a panic).
/// 64 covers every real MCP `inputSchema`; deeper is treated as
/// unvalidatable (skipped, not a violation — consistent with the
/// forward-compat stance) rather than crashing the load test.
const MAX_SCHEMA_DEPTH: usize = 64;

/// Validate `instance` against the JSON Schema `schema`, returning every
/// mismatch found. Empty result = valid under the supported subset.
///
/// Supported keywords: `type` (object/array/string/number/integer/boolean/
/// null), `properties`, `required`, `enum`, `items`. Any other keyword is
/// skipped (forward-compatible — see module docs). An empty/`true`-ish
/// schema validates everything.
pub fn validate(schema: &Value, instance: &Value) -> Vec<SchemaViolation> {
    let mut out = Vec::new();
    validate_at(schema, instance, "", 0, &mut out);
    out
}

fn validate_at(
    schema: &Value,
    instance: &Value,
    path: &str,
    depth: usize,
    out: &mut Vec<SchemaViolation>,
) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(obj) = schema.as_object() else {
        // Non-object schema (e.g. `true`) — nothing to enforce.
        return;
    };

    if let Some(ty) = obj.get("type").and_then(Value::as_str)
        && !type_matches(ty, instance)
    {
        out.push(SchemaViolation {
            path: loc(path),
            message: format!("expected type `{ty}`, got `{}`", json_type(instance)),
        });
        // Type is wrong; deeper checks would just produce noise.
        return;
    }

    if let Some(enum_vals) = obj.get("enum").and_then(Value::as_array)
        && !enum_vals.iter().any(|v| v == instance)
    {
        // The server controls `enum`; cap the rendered preview so a
        // pathological multi-million-entry enum can't balloon the message.
        let preview: Vec<&Value> = enum_vals.iter().take(5).collect();
        let suffix = if enum_vals.len() > 5 {
            format!(" … ({} total)", enum_vals.len())
        } else {
            String::new()
        };
        out.push(SchemaViolation {
            path: loc(path),
            message: format!("value not in enum {preview:?}{suffix}"),
        });
    }

    if let Some(props) = obj.get("properties").and_then(Value::as_object)
        && let Some(inst_obj) = instance.as_object()
    {
        for (key, sub_schema) in props {
            if let Some(sub_inst) = inst_obj.get(key) {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                validate_at(sub_schema, sub_inst, &child, depth + 1, out);
            }
        }
    }

    if let Some(required) = obj.get("required").and_then(Value::as_array)
        && let Some(inst_obj) = instance.as_object()
    {
        for req in required.iter().filter_map(Value::as_str) {
            if !inst_obj.contains_key(req) {
                out.push(SchemaViolation {
                    path: loc(path),
                    message: format!("missing required property `{req}`"),
                });
            }
        }
    }

    if let Some(items) = obj.get("items")
        && let Some(arr) = instance.as_array()
    {
        for (i, item) in arr.iter().enumerate() {
            validate_at(items, item, &format!("{path}[{i}]"), depth + 1, out);
        }
    }
}

fn type_matches(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        // JSON has no integer type; accept whole numbers.
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        // Unknown type keyword → don't enforce (forward-compatible).
        _ => true,
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn loc(path: &str) -> String {
    if path.is_empty() {
        "<root>".to_string()
    } else {
        path.to_string()
    }
}

/// Decide what a set of [`validate`] violations *means* for the run.
///
/// Kept a separate, tiny function on purpose: it encodes the gate policy
/// (a product decision), not mechanical validation, and it sits on top of
/// the deliberate forward-compat stance (ADR 0005). The policy (ADR 0010):
///
/// - **Args** ([`ValidationSite::ToolCallArgs`]) → [`SchemaPolicy::Fail`].
///   Args are what we send; strict mode is explicitly opted in and
///   [`validate`] only flags real breaks of the server's *own* advertised
///   contract (never unknown/extra fields), so a violation is an actionable
///   CI failure rather than forward-compat noise.
/// - **Result** ([`ValidationSite::ToolCallResult`]) → [`SchemaPolicy::Warn`].
///   Servers legitimately grow results over time, so the result side
///   (DESIGN §13.1) stays non-gating observability.
/// - No violations → [`SchemaPolicy::Ignore`].
pub fn classify_schema_violation(
    site: ValidationSite,
    violations: &[SchemaViolation],
) -> SchemaPolicy {
    if violations.is_empty() {
        return SchemaPolicy::Ignore;
    }
    match site {
        // Args are what *we* send. Strict mode is explicitly opted in, and
        // `validate` only ever flags genuine contract breaks against the
        // server's *own* advertised `inputSchema` (missing `required`,
        // wrong `type`, bad `enum`, bad array item). It never flags unknown
        // or extra keywords/properties — so failing here does not conflict
        // with the forward-compat stance (ADR 0005); that concern is about
        // unknown fields and result evolution, neither of which this path
        // touches. A request that violates the tool's declared input
        // contract is an actionable CI failure, not noise.
        ValidationSite::ToolCallArgs => SchemaPolicy::Fail,
        // Result-side validation (wired via `session::strict`,
        // ADR 0010 extension): servers legitimately grow result payloads
        // over time, so the result side defaults to non-gating
        // observability rather than a hard gate — a `structuredContent`
        // mismatch (or absence) warns but never fails the call.
        ValidationSite::ToolCallResult => SchemaPolicy::Warn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer" },
                "mode": { "type": "string", "enum": ["fast", "slow"] },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["name"]
        })
    }

    #[test]
    fn valid_instance_has_no_violations() {
        let v = validate(
            &schema(),
            &json!({ "name": "x", "count": 3, "mode": "fast", "tags": ["a", "b"] }),
        );
        assert!(v.is_empty(), "unexpected: {v:?}");
    }

    #[test]
    fn missing_required_is_reported() {
        let v = validate(&schema(), &json!({ "count": 1 }));
        assert!(
            v.iter()
                .any(|x| x.message.contains("required property `name`"))
        );
    }

    #[test]
    fn wrong_type_is_reported_with_path() {
        let v = validate(&schema(), &json!({ "name": 7 }));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "name");
        assert!(v[0].message.contains("expected type `string`"));
    }

    #[test]
    fn enum_and_array_item_types_are_checked() {
        let v = validate(
            &schema(),
            &json!({ "name": "x", "mode": "turbo", "tags": ["ok", 9] }),
        );
        assert!(v.iter().any(|x| x.message.contains("enum")));
        assert!(v.iter().any(|x| x.path == "tags[1]"));
    }

    #[test]
    fn unknown_keywords_are_not_rejected() {
        // `format`/`minimum` are unmodeled — must not produce violations.
        let s = json!({ "type": "string", "format": "email", "minLength": 3 });
        assert!(validate(&s, &json!("hi")).is_empty());
    }

    fn one_violation() -> Vec<SchemaViolation> {
        vec![SchemaViolation {
            path: "x".into(),
            message: "expected type `string`".into(),
        }]
    }

    #[test]
    fn policy_fails_on_arg_violations() {
        assert_eq!(
            classify_schema_violation(ValidationSite::ToolCallArgs, &one_violation()),
            SchemaPolicy::Fail
        );
    }

    #[test]
    fn policy_warns_on_result_violations() {
        assert_eq!(
            classify_schema_violation(ValidationSite::ToolCallResult, &one_violation()),
            SchemaPolicy::Warn
        );
    }

    #[test]
    fn deeply_nested_schema_is_bounded_not_a_stack_overflow() {
        // 300 levels — comfortably past MAX_SCHEMA_DEPTH (64) so the cap is
        // exercised, but shallow enough that building/dropping the nested
        // `serde_json::Value` (whose Drop is itself recursive) is safe. The
        // real attack vector (a multi-thousand-deep schema sent on the
        // wire) is rejected even earlier by serde_json's own deserialize
        // recursion limit; this guards the in-memory traversal.
        let mut schema = json!({ "type": "object" });
        let mut instance = json!({});
        for _ in 0..300 {
            schema = json!({ "type": "object", "properties": { "n": schema } });
            instance = json!({ "n": instance });
        }
        // Must return (not abort) and not spuriously flag the over-deep tail.
        let v = validate(&schema, &instance);
        assert!(v.is_empty(), "over-deep schema must be skipped, got {v:?}");
    }

    #[test]
    fn policy_ignores_when_no_violations() {
        assert_eq!(
            classify_schema_violation(ValidationSite::ToolCallArgs, &[]),
            SchemaPolicy::Ignore
        );
        assert_eq!(
            classify_schema_violation(ValidationSite::ToolCallResult, &[]),
            SchemaPolicy::Ignore
        );
    }
}
