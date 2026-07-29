//! Safe, local-only JSON Schema 2020-12 validation for MCP tool schemas,
//! plus the opt-in policy that decides what a mismatch means to a run.
//!
//! Local `$ref` and `$dynamicRef` references are supported. Every external
//! retrieval attempt fails closed through a custom retriever; validation can
//! therefore never turn an untrusted server schema into network I/O.

use jsonschema::{Draft, Retrieve, Uri};
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

/// Validate `instance` against the JSON Schema `schema`, returning every
/// mismatch found. Empty result means valid.
///
/// The MCP default dialect is JSON Schema 2020-12. Compilation failures,
/// including any external reference, are returned as a schema violation so
/// strict mode fails closed. At most 128 instance violations are retained to
/// bound memory and diagnostics for attacker-controlled schemas.
pub fn validate(schema: &Value, instance: &Value) -> Vec<SchemaViolation> {
    let validator = match jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(LocalOnlyRetriever)
        .build(schema)
    {
        Ok(validator) => validator,
        Err(_) => {
            return vec![SchemaViolation {
                path: "<schema>".to_owned(),
                message: "schema is invalid or contains a non-local reference".to_owned(),
            }];
        }
    };
    validator
        .iter_errors(instance)
        .take(128)
        .map(|error| SchemaViolation {
            path: pointer_to_path(error.instance_path().as_str()),
            // Do not render `error`: its Display implementation may include
            // the rejected instance value, which can be sensitive.
            message: format!(
                "does not satisfy JSON Schema keyword at {}",
                error.schema_path()
            ),
        })
        .collect()
}

#[derive(Debug)]
struct LocalOnlyRetriever;

impl Retrieve for LocalOnlyRetriever {
    fn retrieve(
        &self,
        _uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "external JSON Schema references are disabled",
        )
        .into())
    }
}

fn pointer_to_path(pointer: &str) -> String {
    if pointer.is_empty() {
        return "<root>".to_owned();
    }
    let mut path = String::new();
    for raw in pointer.trim_start_matches('/').split('/') {
        let segment = raw.replace("~1", "/").replace("~0", "~");
        if segment.chars().all(|character| character.is_ascii_digit()) {
            path.push('[');
            path.push_str(&segment);
            path.push(']');
        } else {
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(&segment);
        }
    }
    path
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
        assert!(v.iter().any(|x| x.message.contains("JSON Schema keyword")));
    }

    #[test]
    fn wrong_type_is_reported_with_path() {
        let v = validate(&schema(), &json!({ "name": 7 }));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "name");
        assert!(v[0].message.contains("JSON Schema keyword"));
    }

    #[test]
    fn enum_and_array_item_types_are_checked() {
        let v = validate(
            &schema(),
            &json!({ "name": "x", "mode": "turbo", "tags": ["ok", 9] }),
        );
        assert!(v.iter().any(|x| x.path == "mode"));
        assert!(v.iter().any(|x| x.path == "tags[1]"));
    }

    #[test]
    fn full_2020_12_keywords_are_enforced() {
        let s = json!({ "type": "string", "minLength": 3 });
        assert!(!validate(&s, &json!("hi")).is_empty());
    }

    #[test]
    fn local_refs_work_and_external_refs_fail_closed() {
        let local = json!({
            "$defs": {"name": {"type": "string", "minLength": 2}},
            "$ref": "#/$defs/name"
        });
        assert!(validate(&local, &json!("ok")).is_empty());
        assert!(!validate(&local, &json!("x")).is_empty());

        let external = json!({"$ref": "https://attacker.example/schema.json"});
        let violations = validate(&external, &json!({}));
        assert_eq!(violations[0].path, "<schema>");
        assert!(violations[0].message.contains("non-local"));
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
    fn moderately_deep_schema_is_supported() {
        let mut schema = json!({ "type": "object" });
        let mut instance = json!({});
        for _ in 0..24 {
            schema = json!({ "type": "object", "properties": { "n": schema } });
            instance = json!({ "n": instance });
        }
        let v = validate(&schema, &instance);
        assert!(v.is_empty(), "valid deep schema must pass, got {v:?}");
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
