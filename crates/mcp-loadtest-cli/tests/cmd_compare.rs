//! Integration tests for the `compare` subcommand.
//!
//! Writes synthetic `metrics.json` files matching the JSON-reporter wire
//! format (DESIGN.md §17.2), invokes `cmd_compare::run`, and asserts on the
//! rendered output.

use std::fs;

use mcp_loadtest_cli::cmd_compare::{self, CompareFormat, RegressionThresholds};
use serde_json::json;
use tempfile::tempdir;

/// Construct a synthetic `metrics.json` blob with overridable fields.
fn synthetic_metrics_json(run_id: &str, p99_ms: f64, errors: u64, deadlocks: u32) -> String {
    let report = json!({
        "run_id": run_id,
        "started_at": "2026-05-11T00:00:00Z",
        "duration_secs": 60.0,
        "scenario": { "name": "sustained" },
        "server": {
            "command": "python",
            "args": ["-m", "mock"],
            "pid": 1234,
            "protocol_version": "2025-03-26"
        },
        "latency_ms": {
            "p50": 10.0,
            "p95": 50.0,
            "p99": p99_ms,
            "p999": 200.0,
            "min": 1.0,
            "max": 250.0,
            "mean": 20.0,
            "count": 1000
        },
        "throughput": {
            "total_requests": 1000,
            "successful_requests": 1000_u64.saturating_sub(errors),
            "requests_per_sec": 16.7
        },
        "errors": {
            "total": errors,
            "by_category": {
                "Hang": 0,
                "Timeout": 0,
                "ServerError": errors,
                "ProtocolError": 0,
                "Crash": 0,
                "Malformed": 0,
                "Disconnected": 0,
                "Cancelled": 0
            }
        },
        "process": {
            "peak_rss_mb": 100.0,
            "final_rss_mb": 100.0,
            "avg_cpu_pct": 5.0
        },
        "deadlock_count": deadlocks,
        "hang_count": 0,
        "threshold_violations": [],
        "passed": deadlocks == 0
    });
    serde_json::to_string_pretty(&report).expect("serialize synthetic report")
}

#[test]
fn compare_flags_p99_regression() {
    let dir = tempdir().expect("tempdir");
    let baseline_path = dir.path().join("baseline.json");
    let current_path = dir.path().join("current.json");

    // Baseline p99 = 100ms, current p99 = 250ms (150% growth, well above 10%).
    fs::write(
        &baseline_path,
        synthetic_metrics_json("01BASE", 100.0, 0, 0),
    )
    .expect("write baseline");
    fs::write(&current_path, synthetic_metrics_json("01CUR", 250.0, 0, 0)).expect("write current");

    let md = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Markdown,
        &RegressionThresholds::default(),
    )
    .expect("compare run");

    assert!(
        md.contains("REGRESSION"),
        "expected regression banner in output:\n{md}"
    );
    assert!(
        md.contains("latency_p99_ms"),
        "expected p99 metric mentioned:\n{md}"
    );
    // The baseline p99 (100.00) and current p99 (250.00) should appear.
    assert!(md.contains("100.00"), "baseline p99 should appear:\n{md}");
    assert!(md.contains("250.00"), "current p99 should appear:\n{md}");
}

#[test]
fn compare_no_regression_on_identical_reports() {
    let dir = tempdir().expect("tempdir");
    let baseline_path = dir.path().join("baseline.json");
    let current_path = dir.path().join("current.json");

    fs::write(
        &baseline_path,
        synthetic_metrics_json("01BASE", 100.0, 0, 0),
    )
    .expect("write baseline");
    fs::write(&current_path, synthetic_metrics_json("01CUR", 100.0, 0, 0)).expect("write current");

    let md = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Markdown,
        &RegressionThresholds::default(),
    )
    .expect("compare run");

    assert!(
        md.contains("no regressions"),
        "expected no-regression banner:\n{md}"
    );
}

#[test]
fn compare_flags_deadlock_uptick() {
    let dir = tempdir().expect("tempdir");
    let baseline_path = dir.path().join("baseline.json");
    let current_path = dir.path().join("current.json");

    fs::write(
        &baseline_path,
        synthetic_metrics_json("01BASE", 100.0, 0, 0),
    )
    .expect("write baseline");
    fs::write(&current_path, synthetic_metrics_json("01CUR", 100.0, 0, 1)).expect("write current");

    let md = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Markdown,
        &RegressionThresholds::default(),
    )
    .expect("compare run");
    assert!(md.contains("REGRESSION"), "expected regression on deadlock");
    assert!(md.contains("deadlock_count"));
}

#[test]
fn compare_json_output_structure() {
    let dir = tempdir().expect("tempdir");
    let baseline_path = dir.path().join("baseline.json");
    let current_path = dir.path().join("current.json");

    fs::write(
        &baseline_path,
        synthetic_metrics_json("01BASE", 100.0, 0, 0),
    )
    .expect("write baseline");
    fs::write(&current_path, synthetic_metrics_json("01CUR", 250.0, 0, 0)).expect("write current");

    let json_out = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Json,
        &RegressionThresholds::default(),
    )
    .expect("compare run");

    let v: serde_json::Value = serde_json::from_str(&json_out).expect("parse json output");
    assert_eq!(v["baseline_run_id"], "01BASE");
    assert_eq!(v["current_run_id"], "01CUR");
    assert_eq!(v["has_regression"], true);
    assert!(v["regressions"].is_array());
    let regressions = v["regressions"].as_array().expect("regressions array");
    assert!(
        !regressions.is_empty(),
        "should have at least one regression"
    );
    assert!(
        regressions.iter().any(|r| r["metric"] == "latency_p99_ms"),
        "expected latency_p99_ms in regressions: {regressions:?}"
    );
}

#[test]
fn compare_handles_missing_file() {
    let dir = tempdir().expect("tempdir");
    let baseline_path = dir.path().join("missing.json");
    let current_path = dir.path().join("also-missing.json");

    let result = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Markdown,
        &RegressionThresholds::default(),
    );
    assert!(
        result.is_err(),
        "expected error for missing files, got: {result:?}"
    );
}

#[test]
fn custom_thresholds_flip_the_regression_verdict() {
    // Identical inputs, different policy → different gate result, observed
    // through the public `run` JSON output (the CI-facing contract).
    let dir = tempdir().expect("tempdir");
    let baseline_path = dir.path().join("baseline.json");
    let current_path = dir.path().join("current.json");

    // Baseline p99 = 100ms, current p99 = 115ms → +15% growth.
    fs::write(
        &baseline_path,
        synthetic_metrics_json("01BASE", 100.0, 0, 0),
    )
    .expect("write baseline");
    fs::write(&current_path, synthetic_metrics_json("01CUR", 115.0, 0, 0)).expect("write current");

    let parse = |s: &str| -> serde_json::Value {
        serde_json::from_str(s).expect("parse compare json output")
    };

    // Default 10% policy → +15% is a regression.
    let strict = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Json,
        &RegressionThresholds::default(),
    )
    .expect("compare run (strict)");
    assert_eq!(parse(&strict)["has_regression"], true);

    // Loosen p99 budget to 20% → same +15% delta is now within budget.
    let lax = cmd_compare::run(
        &baseline_path,
        &current_path,
        CompareFormat::Json,
        &RegressionThresholds {
            p99_pct: 20.0,
            ..RegressionThresholds::default()
        },
    )
    .expect("compare run (lax)");
    assert_eq!(parse(&lax)["has_regression"], false);
}
