//! The JSON reporter's output must conform to the published JSON Schema at
//! `docs/schema/metrics.v1.json` (advertised in README + DESIGN §17.2/§21.5 as
//! the contract downstream LLM/CI tooling validates against).
//!
//! This is a drift guard, not a full JSON-Schema validator: it walks the
//! schema and asserts every `required` field (recursively, through nested
//! objects and array items) is present in a freshly-rendered report. If a
//! field in `report/json.rs` is renamed or dropped, a required key vanishes
//! from the output and this test fails loudly — so the committed schema can't
//! silently fall out of sync with the wire format. Dependency-free (no
//! `jsonschema` crate), public-API only.

use std::time::{Duration, SystemTime};

use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{
    ProcessStats, Report, Reporter, ServerInfo, ThresholdKind, ThresholdViolation,
};
use mcp_loadtest_output::report::json::JsonReporter;
use serde_json::Value;

/// Recursively assert `instance` satisfies the structural `required`/`type`
/// shape of `schema` (objects: every required key present + recurse into
/// present properties; arrays: recurse into each element).
fn assert_conforms(schema: &Value, instance: &Value, path: &str) {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let obj = instance
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expected an object, got {instance}"));
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required {
                    let key = key.as_str().expect("`required` entries are strings");
                    assert!(
                        obj.contains_key(key),
                        "metrics.json is missing required field `{path}/{key}` — the JSON \
                         reporter output drifted from docs/schema/metrics.v1.json"
                    );
                }
            }
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (name, subschema) in props {
                    if let Some(child) = instance.get(name) {
                        assert_conforms(subschema, child, &format!("{path}/{name}"));
                    }
                }
            }
        }
        Some("array") => {
            let arr = instance
                .as_array()
                .unwrap_or_else(|| panic!("{path}: expected an array, got {instance}"));
            if let Some(items) = schema.get("items") {
                for (i, elem) in arr.iter().enumerate() {
                    assert_conforms(items, elem, &format!("{path}[{i}]"));
                }
            }
        }
        _ => {}
    }
}

/// A report exercising every block, including a non-empty
/// `threshold_violations` so the array-item shape is checked too.
fn sample_report() -> Report {
    Report {
        run_id: "01HXSCHEMA000000000000000000".to_string(),
        started_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_secs(60),
        scenario_name: "sustained".to_string(),
        server_info: ServerInfo {
            command: "python".to_string(),
            args: vec!["-m".to_string(), "demo".to_string()],
            pid: None,
            protocol_version: Some("2025-03-26".to_string()),
        },
        metrics: ScenarioMetrics {
            latency: LatencyStats {
                p50: Duration::from_millis(10),
                p95: Duration::from_millis(20),
                p99: Duration::from_millis(42),
                p999: Duration::from_millis(80),
                mean: Duration::from_millis(15),
                min: Duration::from_millis(1),
                max: Duration::from_millis(90),
                count: 100,
            },
            throughput: ThroughputStats {
                total_requests: 100,
                successful_requests: 95,
                requests_per_sec: 33.5,
            },
            outcomes: OutcomeCounts::default(),
        },
        process: ProcessStats::default(),
        scenario_outcome: ScenarioOutcome::default(),
        trace_path: None,
        threshold_violations: vec![ThresholdViolation {
            kind: ThresholdKind::P99Latency,
            expected: "<= 100ms".to_string(),
            actual: "123.4ms".to_string(),
        }],
        coverage: None,
    }
}

#[test]
fn json_reporter_output_conforms_to_published_schema() {
    let schema_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/schema/metrics.v1.json"
    );
    let schema_raw = std::fs::read_to_string(schema_path).unwrap_or_else(|e| {
        panic!("read {schema_path}: {e} (is docs/schema/metrics.v1.json committed?)")
    });
    let schema: Value = serde_json::from_str(&schema_raw).expect("metrics.v1.json is valid JSON");

    let rendered = JsonReporter
        .render(&sample_report())
        .expect("json render must succeed");
    let output: Value =
        serde_json::from_str(&rendered).expect("JSON reporter output must be valid JSON");

    assert_conforms(&schema, &output, "");
}
