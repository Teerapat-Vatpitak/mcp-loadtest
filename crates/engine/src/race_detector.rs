//! Race / non-determinism detector.
//!
//! Fires N concurrent calls to the SAME tool with the SAME args; if responses
//! diverge, flag as potentially racy. See DESIGN.md §10.5 differentiator
//! entry.
//!
//! # M6 scope (sequential-only)
//!
//! The companion `RaceCheck` scenario must issue calls through a single
//! [`Session`][mcp_loadtest_protocol::Session] whose `call_tool` takes `&mut self` — so the N
//! calls are emitted **sequentially**, not concurrently. The detector itself
//! is concurrency-agnostic: it takes whatever response set you collect and
//! reports divergence. Once a multi-session pool ships in M7+ the same
//! detector serves the "true" race scenario without modification.
//!
//! In sequential form the detector reduces to a **non-determinism detector**:
//! "given the same input, did this tool return the same output every time?"
//! A `diverged: true` result against the same args is enough evidence that
//! the tool's response depends on hidden state — server-side time, RNG, an
//! uncoordinated cache — which is a race signal even without literal
//! concurrency.
//!
//! # Canonicalization
//!
//! Two responses are considered identical iff their canonical string forms
//! are equal. Canonicalization:
//!
//! - `Object` keys are sorted ascending before serialization, recursively.
//! - `Array` order is preserved (arrays are ordered in JSON; reordering would
//!   be a different response, not a different encoding).
//! - `Null`, `Bool`, `String` serialize as-is.
//! - `Number` serializes via `serde_json` — which preserves the textual form
//!   of integers and prints floats with `f64::to_string`-style output.
//!
//! ## Gotchas
//!
//! - **NaN / infinity** can't appear in a `serde_json::Value` (the type
//!   forbids them at construction), so no special handling is required.
//! - **Float equality** is textual — `1.0` and `1` deserialize to different
//!   `Number` variants and compare unequal. That matches user intent: an
//!   integer response vs. a float response *is* a divergence worth flagging.
//! - **Whitespace / key-order in the wire response** is normalized away by
//!   `serde_json::from_str` → `Value` → our canonicalizer, so two servers
//!   that emit `{"a":1,"b":2}` and `{"b":2, "a":1 }` are correctly grouped.

use std::collections::BTreeMap;

use serde_json::Value;

/// Outcome of running [`analyze`] over a set of responses.
#[derive(Debug, Clone, Default)]
pub struct DivergenceReport {
    /// Total responses fed in.
    pub total_responses: u64,
    /// Number of distinct canonical responses.
    pub unique_responses: u64,
    /// Per-group `(occurrences, canonical_response_json)`, sorted by
    /// occurrence count descending. Ties broken by lexicographic order of
    /// the canonical string for determinism.
    pub samples: Vec<(usize, String)>,
    /// True iff at least two distinct canonical responses were observed.
    pub diverged: bool,
}

/// Group responses by canonical form and report divergence.
///
/// An empty input yields `total_responses = 0`, `unique_responses = 0`,
/// `samples` empty, `diverged = false`.
pub fn analyze(responses: &[Value]) -> DivergenceReport {
    if responses.is_empty() {
        return DivergenceReport::default();
    }

    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for v in responses {
        let canonical = canonicalize(v);
        *groups.entry(canonical).or_insert(0) += 1;
    }

    let mut samples: Vec<(usize, String)> = groups
        .into_iter()
        .map(|(canon, count)| (count, canon))
        .collect();
    // Sort by occurrence descending; ties broken by canonical string asc.
    samples.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let total_responses = responses.len() as u64;
    let unique_responses = samples.len() as u64;
    let diverged = unique_responses > 1;

    DivergenceReport {
        total_responses,
        unique_responses,
        samples,
        diverged,
    }
}

/// Recursive normalization → canonical JSON string.
///
/// Sorts object keys; preserves array order. The output is a valid JSON
/// document so two callers that round-trip identical inputs always produce
/// byte-identical canonical forms.
fn canonicalize(value: &Value) -> String {
    let mut buf = String::new();
    write_canonical(value, &mut buf);
    buf
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            // Reuse serde_json's escaping by serializing the bare string.
            // Fallback to a manual escape if for any reason serialization
            // fails (which it shouldn't for a String).
            match serde_json::to_string(s) {
                Ok(escaped) => out.push_str(&escaped),
                Err(_) => {
                    out.push('"');
                    out.push_str(s);
                    out.push('"');
                }
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // BTreeMap iterates keys in sorted order — exactly what we want.
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Key is always a string in JSON.
                match serde_json::to_string(k) {
                    Ok(escaped) => out.push_str(&escaped),
                    Err(_) => {
                        out.push('"');
                        out.push_str(k);
                        out.push('"');
                    }
                }
                out.push(':');
                write_canonical(v, out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn analyze_empty_input() {
        let report = analyze(&[]);
        assert_eq!(report.total_responses, 0);
        assert_eq!(report.unique_responses, 0);
        assert!(report.samples.is_empty());
        assert!(!report.diverged);
    }

    #[test]
    fn analyze_detects_divergence() {
        let responses = vec![json!({"answer": 42}), json!({"answer": 43})];
        let report = analyze(&responses);
        assert_eq!(report.total_responses, 2);
        assert_eq!(report.unique_responses, 2);
        assert!(report.diverged);
    }

    #[test]
    fn analyze_groups_identical() {
        let responses = vec![
            json!({"x": 1}),
            json!({"x": 1}),
            json!({"x": 1}),
            json!({"x": 1}),
            json!({"x": 1}),
        ];
        let report = analyze(&responses);
        assert_eq!(report.total_responses, 5);
        assert_eq!(report.unique_responses, 1);
        assert!(!report.diverged);
        assert_eq!(report.samples.len(), 1);
        assert_eq!(report.samples[0].0, 5);
    }

    #[test]
    fn analyze_canonicalizes_key_order() {
        // Two JSON objects that differ only in key insertion order must
        // canonicalize to the same string and group together.
        let a: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let report = analyze(&[a, b]);
        assert_eq!(report.total_responses, 2);
        assert_eq!(
            report.unique_responses, 1,
            "key-order should not produce divergence; samples={:?}",
            report.samples
        );
        assert!(!report.diverged);
    }

    #[test]
    fn analyze_canonicalizes_nested_keys() {
        // Nested objects' keys also sort.
        let a: Value = serde_json::from_str(r#"{"outer":{"a":1,"b":2},"z":3}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"z":3,"outer":{"b":2,"a":1}}"#).unwrap();
        let report = analyze(&[a, b]);
        assert_eq!(report.unique_responses, 1);
    }

    #[test]
    fn analyze_preserves_array_order() {
        // Arrays are ordered; reordering them is a different response.
        let a = json!([1, 2, 3]);
        let b = json!([3, 2, 1]);
        let report = analyze(&[a, b]);
        assert_eq!(report.unique_responses, 2);
        assert!(report.diverged);
    }

    #[test]
    fn analyze_samples_sorted_by_count_desc() {
        // 3× "a", 2× "b", 1× "c" — samples should land in that order.
        let responses = vec![
            json!({"k": "a"}),
            json!({"k": "a"}),
            json!({"k": "a"}),
            json!({"k": "b"}),
            json!({"k": "b"}),
            json!({"k": "c"}),
        ];
        let report = analyze(&responses);
        assert_eq!(report.total_responses, 6);
        assert_eq!(report.unique_responses, 3);
        assert!(report.diverged);
        assert_eq!(report.samples[0].0, 3);
        assert!(report.samples[0].1.contains("\"a\""));
        assert_eq!(report.samples[1].0, 2);
        assert_eq!(report.samples[2].0, 1);
    }

    #[test]
    fn analyze_handles_primitives_and_nulls() {
        let responses = vec![
            json!(null),
            json!(null),
            json!(true),
            json!(1),
            json!("hello"),
            json!("hello"),
        ];
        let report = analyze(&responses);
        assert_eq!(report.total_responses, 6);
        // unique groups: null, true, 1, "hello"
        assert_eq!(report.unique_responses, 4);
        assert!(report.diverged);
    }

    #[test]
    fn integer_and_float_are_distinct_responses() {
        // 1 vs 1.0 — different JSON Number variants; flagging as divergence
        // matches user intent (it really is a different wire-shape).
        let a: Value = serde_json::from_str("1").unwrap();
        let b: Value = serde_json::from_str("1.0").unwrap();
        let report = analyze(&[a, b]);
        assert_eq!(report.unique_responses, 2);
    }
}
