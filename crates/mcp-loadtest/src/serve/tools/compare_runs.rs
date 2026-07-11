//! `compare_runs` tool — handler + JSON schema definition + path-validation
//! helper.
//!
//! Reads two `metrics.json` files and produces the same structured diff the
//! CLI's `compare` subcommand emits. Path validation prevents arbitrary file
//! reads via path traversal (see [`validate_metrics_path`]).
//!
//! Split out of `tools.rs` in M8 to keep per-tool files under the 300-LoC
//! convention.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::analysis::regression::RegressionThresholds;

use super::{ToolError, required_str};

pub(super) fn compare_runs_def() -> Value {
    json!({
        "name": "compare_runs",
        "description":
            "Read two metrics.json files from prior runs and diff them. Returns the \
             same structured diff that `mcp-loadtest compare` produces.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "baseline": {
                    "type": "string",
                    "description": "Path to the baseline metrics.json."
                },
                "current": {
                    "type": "string",
                    "description": "Path to the current metrics.json."
                },
                "max_p99_regression_pct": {
                    "type": "number",
                    "description": "Optional. p99 latency growth (percent) that flags a regression. Default 10."
                },
                "max_error_rate_regression_pp": {
                    "type": "number",
                    "description": "Optional. Error-rate growth (percentage points) that flags a regression. Default 0.5."
                },
                "allow_deadlock_increase": {
                    "type": "boolean",
                    "description": "Optional. When true, an increase in deadlock count is not treated as a regression. Default false."
                }
            },
            "required": ["baseline", "current"]
        }
    })
}

pub(super) async fn compare_runs(args: &Value) -> Result<Value, ToolError> {
    let baseline = required_str(args, "baseline")?;
    let current = required_str(args, "current")?;

    // Reject path-traversal / arbitrary-file-read attempts before we touch the
    // filesystem. A malicious MCP client can otherwise request
    // `~/.ssh/id_rsa` (or any other unrelated file) and have us return its
    // contents indirectly via the diff error.
    let baseline_path = validate_metrics_path(&baseline)?;
    let current_path = validate_metrics_path(&current)?;

    let base_raw = tokio::fs::read_to_string(&baseline_path)
        .await
        .map_err(|e| ToolError::Io(format!("reading {baseline}: {e}")))?;
    let cur_raw = tokio::fs::read_to_string(&current_path)
        .await
        .map_err(|e| ToolError::Io(format!("reading {current}: {e}")))?;

    let base: Value = serde_json::from_str(&base_raw)
        .map_err(|e| ToolError::InvalidArgs(format!("parsing baseline json: {e}")))?;
    let cur: Value = serde_json::from_str(&cur_raw)
        .map_err(|e| ToolError::InvalidArgs(format!("parsing current json: {e}")))?;

    // Pull the few fields we diff; keep parsing local so the server stays a
    // sibling of cmd_compare rather than a hard dep on the CLI crate.
    let base_p99 = json_path_f64(&base, &["latency_ms", "p99"]);
    let cur_p99 = json_path_f64(&cur, &["latency_ms", "p99"]);
    let base_p95 = json_path_f64(&base, &["latency_ms", "p95"]);
    let cur_p95 = json_path_f64(&cur, &["latency_ms", "p95"]);
    let base_p50 = json_path_f64(&base, &["latency_ms", "p50"]);
    let cur_p50 = json_path_f64(&cur, &["latency_ms", "p50"]);
    let base_rps = json_path_f64(&base, &["throughput", "requests_per_sec"]);
    let cur_rps = json_path_f64(&cur, &["throughput", "requests_per_sec"]);
    let base_total = json_path_u64(&base, &["throughput", "total_requests"]);
    let cur_total = json_path_u64(&cur, &["throughput", "total_requests"]);
    let base_errs = json_path_u64(&base, &["errors", "total"]);
    let cur_errs = json_path_u64(&cur, &["errors", "total"]);
    let base_dl = json_path_u64(&base, &["deadlock_count"]);
    let cur_dl = json_path_u64(&cur, &["deadlock_count"]);

    let base_err_rate = error_rate_pct(base_errs, base_total);
    let cur_err_rate = error_rate_pct(cur_errs, cur_total);

    // Regression rules — same `RegressionThresholds` model the CLI's
    // `compare` subcommand uses, so the two stay in lockstep. Optional tool
    // args override the defaults (ADR 0009).
    let thr = regression_overrides(args);
    let p99_pct = pct_change(base_p99, cur_p99);
    let p99_regressed = p99_pct > thr.p99_pct;
    let err_regressed = (cur_err_rate - base_err_rate) > thr.error_rate_pp;
    let dl_regressed = thr.deadlock_zero_tolerance && cur_dl > base_dl;

    let mut regressions: Vec<&str> = Vec::new();
    if p99_regressed {
        regressions.push("latency_p99_ms");
    }
    if err_regressed {
        regressions.push("error_rate_pct");
    }
    if dl_regressed {
        regressions.push("deadlock_count");
    }
    let has_regression = !regressions.is_empty();

    Ok(json!({
        "baseline_run_id": base.get("run_id").cloned().unwrap_or(Value::Null),
        "current_run_id": cur.get("run_id").cloned().unwrap_or(Value::Null),
        "metrics": {
            "latency_p99_ms":   { "baseline": base_p99, "current": cur_p99, "change": cur_p99 - base_p99 },
            "latency_p95_ms":   { "baseline": base_p95, "current": cur_p95, "change": cur_p95 - base_p95 },
            "latency_p50_ms":   { "baseline": base_p50, "current": cur_p50, "change": cur_p50 - base_p50 },
            "requests_per_sec": { "baseline": base_rps, "current": cur_rps, "change": cur_rps - base_rps },
            "error_rate_pct":   { "baseline": base_err_rate, "current": cur_err_rate, "change": cur_err_rate - base_err_rate },
            "deadlock_count":   { "baseline": base_dl, "current": cur_dl, "change": cur_dl as i64 - base_dl as i64 },
        },
        "regressions": regressions,
        "has_regression": has_regression,
    }))
}

/// Vet a caller-supplied metrics file path against a small allowlist before
/// we read it. Without this, a malicious MCP client can ask `compare_runs`
/// for `/etc/passwd` or `~/.ssh/id_rsa` and observe the contents through the
/// diff (or even just through error messages that echo file content).
///
/// Accept rules:
///   1. Path must be non-empty.
///   2. Path must not contain a `..` segment, even if it would canonicalize
///      to something safe — `..` in the input is always suspicious.
///   3. After [`std::fs::canonicalize`], the path must EITHER be rooted
///      under `<cwd>/runs/` (the normal report layout), OR be absolute and
///      end with the filename `metrics.json` (so out-of-tree report dirs
///      like `/tmp/somerun/metrics.json` still work for legitimate
///      automation while `/etc/passwd` is rejected).
///
/// Canonicalization implicitly requires the file to exist; that's fine —
/// we're about to read it anyway, and rejecting non-existent paths early is
/// itself a defense against probing.
fn validate_metrics_path(p: &str) -> Result<PathBuf, ToolError> {
    if p.trim().is_empty() {
        return Err(ToolError::InvalidArgs(
            "metrics path rejected: empty".into(),
        ));
    }
    // Cheap pre-canonicalize sanity check: any `..` segment is a red flag
    // regardless of where canonicalize would land.
    let raw = std::path::Path::new(p);
    if raw
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ToolError::InvalidArgs(
            "metrics path rejected: must end in metrics.json and not contain '..'".into(),
        ));
    }

    let canonical = std::fs::canonicalize(raw).map_err(|e| {
        // Wrap canonicalize errors as InvalidArgs (not Io) so the client sees
        // a single consistent rejection reason for "bad path" regardless of
        // whether the failure is "doesn't exist", "permission denied", etc.
        // Do not echo the raw caller-supplied path back — the OS error
        // alone is enough to act on and avoids reflecting attacker input /
        // partial filesystem layout to the MCP client.
        ToolError::InvalidArgs(format!("metrics path rejected: cannot resolve path: {e}"))
    })?;

    // Allow #1: under cwd/runs/.
    let under_runs = std::env::current_dir()
        .ok()
        .and_then(|cwd| std::fs::canonicalize(cwd.join("runs")).ok())
        .map(|runs| canonical.starts_with(&runs))
        .unwrap_or(false);

    // Allow #2: absolute path ending in `metrics.json`.
    let absolute_metrics_json = canonical.is_absolute()
        && canonical
            .file_name()
            .map(|n| n == "metrics.json")
            .unwrap_or(false);

    if under_runs || absolute_metrics_json {
        Ok(canonical)
    } else {
        Err(ToolError::InvalidArgs(
            "metrics path rejected: must end in metrics.json and not contain '..'".into(),
        ))
    }
}

fn json_path_f64(v: &Value, path: &[&str]) -> f64 {
    let mut cur = v;
    for p in path {
        match cur.get(*p) {
            Some(next) => cur = next,
            None => return 0.0,
        }
    }
    cur.as_f64().unwrap_or(0.0)
}

fn json_path_u64(v: &Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for p in path {
        match cur.get(*p) {
            Some(next) => cur = next,
            None => return 0,
        }
    }
    cur.as_u64().unwrap_or(0)
}

/// Build the regression policy from optional tool args, falling back to
/// [`RegressionThresholds::default`] for anything not supplied.
///
/// A threshold is only honoured when it is **finite and > 0**. A
/// non-positive value would invert the regression direction (a regression
/// would read as an improvement and vice-versa); since this is untrusted
/// MCP tool input, such values are rejected back to the default rather
/// than silently disabling the gate.
fn regression_overrides(args: &Value) -> RegressionThresholds {
    let d = RegressionThresholds::default();
    let positive_or = |key: &str, fallback: f64| {
        args.get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(fallback)
    };
    RegressionThresholds {
        p99_pct: positive_or("max_p99_regression_pct", d.p99_pct),
        error_rate_pp: positive_or("max_error_rate_regression_pp", d.error_rate_pp),
        deadlock_zero_tolerance: !args
            .get("allow_deadlock_increase")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn pct_change(base: f64, cur: f64) -> f64 {
    if base <= 0.0 {
        return 0.0;
    }
    (cur - base) / base * 100.0
}

fn error_rate_pct(errs: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    errs as f64 / total as f64 * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S-H1 regression: classic path traversal must be rejected before we
    /// touch the filesystem.
    #[test]
    fn validate_metrics_path_rejects_dotdot_traversal() {
        let err = validate_metrics_path("../../../etc/passwd").unwrap_err();
        match err {
            ToolError::InvalidArgs(m) => {
                assert!(
                    m.contains("metrics.json") || m.contains(".."),
                    "unexpected message: {m}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    /// S-H1 regression: an absolute path that exists and ends in
    /// `metrics.json` is accepted (the standard out-of-tree report case).
    #[test]
    fn validate_metrics_path_accepts_absolute_metrics_json() {
        // tempfile is in the CLI crate's dev-deps but not this one; spin up
        // a unique subdir under env::temp_dir() and clean up at end.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "mcp-loadtest-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let metrics = dir.join("metrics.json");
        std::fs::write(&metrics, b"{}").expect("write metrics.json");

        let result = validate_metrics_path(metrics.to_str().expect("utf-8 path"));

        // Always try to clean up, even on assertion failure.
        let _ = std::fs::remove_dir_all(&dir);

        let canonical = result.expect("absolute metrics.json must be accepted");
        assert!(canonical.ends_with("metrics.json"));
    }

    #[test]
    fn regression_overrides_default_when_args_absent() {
        let t = regression_overrides(&json!({}));
        assert_eq!(t, RegressionThresholds::default());
    }

    #[test]
    fn regression_overrides_parses_supplied_args() {
        let t = regression_overrides(&json!({
            "max_p99_regression_pct": 25.0,
            "max_error_rate_regression_pp": 2.0,
            "allow_deadlock_increase": true
        }));
        assert_eq!(t.p99_pct, 25.0);
        assert_eq!(t.error_rate_pp, 2.0);
        assert!(!t.deadlock_zero_tolerance);
    }

    #[test]
    fn regression_overrides_rejects_non_positive_thresholds() {
        // Negative / zero would invert the regression direction — must fall
        // back to the safe defaults rather than be honoured.
        let d = RegressionThresholds::default();
        let t = regression_overrides(&json!({
            "max_p99_regression_pct": -10.0,
            "max_error_rate_regression_pp": 0.0
        }));
        assert_eq!(t.p99_pct, d.p99_pct);
        assert_eq!(t.error_rate_pp, d.error_rate_pp);
    }

    /// Drift guard: the metrics.json field paths this tool reads must stay in
    /// lockstep with what the JSON reporter writes. `json_path_*` silently
    /// returns 0 on a missing/renamed path, which would turn a real regression
    /// into a "no change" verdict — so pin the agreement by rendering a known
    /// `Report` through the real [`JsonReporter`] and asserting every path
    /// `compare_runs` reads resolves to the expected value. A rename in
    /// `report/json.rs` breaks this test loudly instead of silently.
    #[test]
    fn json_path_fields_match_json_reporter_output() {
        use std::time::{Duration, SystemTime};

        use crate::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
        use crate::report::json::JsonReporter;
        use crate::report::{ProcessStats, Report, Reporter, ServerInfo};
        use crate::scenario::ScenarioOutcome;

        let report = Report {
            run_id: "01HXDRIFTGUARD0000000000000".to_string(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_secs(60),
            scenario_name: "sustained".to_string(),
            server_info: ServerInfo {
                command: "python".to_string(),
                args: vec![],
                pid: None,
                protocol_version: None,
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
                    successful_requests: 90,
                    requests_per_sec: 33.5,
                },
                outcomes: OutcomeCounts::default(),
            },
            process: ProcessStats::default(),
            scenario_outcome: ScenarioOutcome {
                deadlock_count: 2,
                ..Default::default()
            },
            trace_path: None,
            threshold_violations: vec![],
            coverage: None,
        };

        let rendered = JsonReporter.render(&report).expect("json render");
        let v: Value = serde_json::from_str(&rendered).expect("reporter output is valid json");

        // Exactly the paths compare_runs() reads. If any drifts, these fail.
        assert!((json_path_f64(&v, &["latency_ms", "p99"]) - 42.0).abs() < 1e-6);
        assert!((json_path_f64(&v, &["latency_ms", "p95"]) - 20.0).abs() < 1e-6);
        assert!((json_path_f64(&v, &["latency_ms", "p50"]) - 10.0).abs() < 1e-6);
        assert!((json_path_f64(&v, &["throughput", "requests_per_sec"]) - 33.5).abs() < 1e-6);
        assert_eq!(json_path_u64(&v, &["throughput", "total_requests"]), 100);
        // ErrorsView.total = total_requests - successful_requests.
        assert_eq!(json_path_u64(&v, &["errors", "total"]), 10);
        assert_eq!(json_path_u64(&v, &["deadlock_count"]), 2);
    }
}
