//! End-to-end coverage for the real `run --config` entrypoint with
//! `[validation] strict = true` (ADR 0010).
//!
//! `tests/strict_validation.rs` (lib crate) drives `scenario.drive()`
//! directly. This test exercises the *production* path that a CI user
//! actually hits: `cmd_run::run_from_config` → `Config::from_file`
//! (incl. `[validation]` TOML deserialize) → `build_scenario` →
//! `Run::execute` (the `run.rs` strict-wiring) → `emit_reports`
//! (report-on-disk) → non-zero exit on the threshold gate.

use std::fs;
use std::path::PathBuf;

use mcp_loadtest_cli::cmd_run;
use tempfile::tempdir;

fn mock_schema_fixture() -> PathBuf {
    // Fixtures live in the engine crate; this test crate is its sibling.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("engine")
        .join("tests")
        .join("fixtures")
        .join("mock-schema.py")
}

/// Build a TOML config string. `args_toml` is the inline `scenario.args`
/// table; `report_dir` is where `runs/<ulid>/` lands.
fn config_toml(args_toml: &str, report_dir: &str) -> String {
    let mock = mock_schema_fixture();
    // Forward slashes are valid in TOML double-quoted strings and accepted
    // by Windows path APIs — avoids backslash-escaping headaches.
    let mock = mock.to_string_lossy().replace('\\', "/");
    let report_dir = report_dir.replace('\\', "/");
    format!(
        r#"
[server]
command = "python"
args = ["{mock}"]
transport = "stdio"

[scenario]
type = "sustained"
tool = "echo"
duration = "400ms"
concurrent = 1
args = {args_toml}

[thresholds]
# Any error at all trips the gate — strict-mode ProtocolErrors must fail CI.
error_rate = 0.0

[validation]
strict = true

[output]
report_dir = "{report_dir}"
formats = ["json", "markdown"]
"#
    )
}

/// The single `runs/<ulid>/` dir created under `report_dir`.
fn sole_run_dir(report_dir: &std::path::Path) -> PathBuf {
    let mut entries: Vec<PathBuf> = fs::read_dir(report_dir)
        .expect("read report_dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one run dir under {report_dir:?}, got {entries:?}"
    );
    entries.pop().unwrap()
}

#[tokio::test]
async fn strict_mode_gates_the_run_and_writes_reports() {
    let tmp = tempdir().expect("tempdir");
    let report_dir = tmp.path().join("runs");
    let cfg_path = tmp.path().join("bench.toml");

    // `echo` requires a string `msg`; send an int → schema violation on
    // every call → ProtocolError → error_rate > 0.0 → gate fails.
    fs::write(
        &cfg_path,
        config_toml(r#"{ msg = 123 }"#, report_dir.to_str().unwrap()),
    )
    .expect("write config");

    let result = cmd_run::run_from_config(&cfg_path, false, false, None).await;

    // 1. The production entrypoint must surface a non-zero exit (Err).
    assert!(
        result.is_err(),
        "strict-mode schema violations must fail the run; got {result:?}"
    );

    // 2. Reports were still written before the gate tripped.
    let run_dir = sole_run_dir(&report_dir);
    let metrics_path = run_dir.join("metrics.json");
    assert!(metrics_path.is_file(), "metrics.json missing");
    assert!(run_dir.join("report.md").is_file(), "report.md missing");

    // 3. The metrics reflect strict rejection: not passed, ProtocolErrors.
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&metrics_path).expect("read metrics.json"))
            .expect("parse metrics.json");
    assert_eq!(v["passed"], false, "run should not have passed: {v}");
    assert!(
        v["errors"]["total"].as_u64().unwrap_or(0) > 0,
        "expected errors > 0: {v}"
    );
    assert!(
        v["errors"]["by_category"]["ProtocolError"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "expected ProtocolError category > 0: {v}"
    );
}

#[tokio::test]
async fn strict_mode_lets_schema_compliant_runs_pass() {
    let tmp = tempdir().expect("tempdir");
    let report_dir = tmp.path().join("runs");
    let cfg_path = tmp.path().join("bench.toml");

    // Compliant args: `msg` present and a string.
    fs::write(
        &cfg_path,
        config_toml(r#"{ msg = "hello" }"#, report_dir.to_str().unwrap()),
    )
    .expect("write config");

    let result = cmd_run::run_from_config(&cfg_path, false, false, None).await;
    assert!(
        result.is_ok(),
        "strict mode must not gate schema-compliant runs; got {result:?}"
    );

    let run_dir = sole_run_dir(&report_dir);
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("metrics.json")).expect("read metrics.json"),
    )
    .expect("parse metrics.json");
    assert_eq!(v["passed"], true, "clean strict run should pass: {v}");
    assert_eq!(
        v["errors"]["total"].as_u64().unwrap_or(99),
        0,
        "no errors expected: {v}"
    );
    assert!(
        v["throughput"]["successful_requests"].as_u64().unwrap_or(0) > 0,
        "expected successful calls: {v}"
    );
}

#[tokio::test]
async fn action_output_override_wins_over_config_report_dir() {
    let tmp = tempdir().expect("tempdir");
    let config_report_dir = tmp.path().join("config-runs");
    let action_report_dir = tmp.path().join("action-runs");
    let cfg_path = tmp.path().join("bench.toml");

    fs::write(
        &cfg_path,
        config_toml(r#"{ msg = "hello" }"#, config_report_dir.to_str().unwrap()),
    )
    .expect("write config");

    let result = cmd_run::run_from_config_with_output(
        &cfg_path,
        false,
        false,
        None,
        Some(action_report_dir.clone()),
        false,
    )
    .await;
    assert!(
        result.is_ok(),
        "Action override run should pass, got {result:?}"
    );
    assert!(
        !config_report_dir.exists(),
        "config report root must not be used when the private Action override is present"
    );
    let run_dir = sole_run_dir(&action_report_dir);
    assert!(run_dir.join("metrics.json").is_file());
    assert!(run_dir.join("report.md").is_file());
}

#[tokio::test]
async fn action_mode_redacts_report_and_trace_server_identity() {
    let tmp = tempdir().expect("tempdir");
    let config_report_dir = tmp.path().join("config-runs");
    let action_report_dir = tmp.path().join("action-runs");
    let trace_path = tmp.path().join("action-trace.jsonl");
    let cfg_path = tmp.path().join("bench.toml");
    let sentinel = "ACTION_SERVER_SECRET_7F3B";

    let mut toml = config_toml(r#"{ msg = "hello" }"#, config_report_dir.to_str().unwrap());
    let mock = mock_schema_fixture().to_string_lossy().replace('\\', "/");
    let original_args = format!(r#"args = ["{mock}"]"#);
    let secret_args = format!(r#"args = ["{mock}", "{sentinel}"]"#);
    assert!(toml.contains(&original_args));
    toml = toml.replace(&original_args, &secret_args);
    fs::write(&cfg_path, toml).expect("write config");

    let result = cmd_run::run_from_config_with_output(
        &cfg_path,
        false,
        false,
        Some(trace_path.clone()),
        Some(action_report_dir.clone()),
        true,
    )
    .await;
    assert!(result.is_ok(), "redacted Action run failed: {result:?}");
    assert!(!config_report_dir.exists());

    let run_dir = sole_run_dir(&action_report_dir);
    let metrics_text = fs::read_to_string(run_dir.join("metrics.json")).expect("read metrics.json");
    let report_text = fs::read_to_string(run_dir.join("report.md")).expect("read report.md");
    let trace_text = fs::read_to_string(&trace_path).expect("read trace");
    for (name, text) in [
        ("metrics.json", &metrics_text),
        ("report.md", &report_text),
        ("trace", &trace_text),
    ] {
        assert!(
            !text.contains(sentinel) && !text.contains(&mock),
            "{name} leaked configured server identity"
        );
    }

    let metrics: serde_json::Value =
        serde_json::from_str(&metrics_text).expect("parse metrics.json");
    assert_eq!(metrics["server"]["command"], "[REDACTED]");
    assert_eq!(metrics["server"]["args"], serde_json::json!([]));
    let trace_header: serde_json::Value = serde_json::from_str(
        trace_text
            .lines()
            .next()
            .expect("trace must contain a header"),
    )
    .expect("parse trace header");
    assert_eq!(trace_header["server"], "[REDACTED]");
}
