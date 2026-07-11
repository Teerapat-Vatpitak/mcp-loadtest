//! Snapshot tests for the Markdown + JSON reporters.
//!
//! Builds two synthetic [`Report`] values — one passing, one failing — with
//! fully deterministic field values (fixed run_id, fixed `started_at` based
//! on `UNIX_EPOCH`, no `Instant`-derived fields, etc.) so the snapshots are
//! stable across runs and platforms.
//!
//! Workflow when these snapshots break:
//!   1. `cargo test -p mcp-loadtest --test reporter_snapshots` will fail and
//!      write `*.snap.new` next to the existing snapshots.
//!   2. Run `cargo insta review` (or `cargo insta accept` if the diff is
//!      intended) to bless the new output.
//!   3. Commit the updated `.snap` files.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{
    ProcessStats, Report, Reporter, ServerInfo, ThresholdKind, ThresholdViolation,
};
use mcp_loadtest_output::report::html::HtmlReporter;
use mcp_loadtest_output::report::json::JsonReporter;
use mcp_loadtest_output::report::markdown::MarkdownReporter;
use mcp_loadtest_output::report::terminal::TerminalReporter;

/// Reference run start used in every snapshot fixture.
///
/// `2026-05-10T07:30:00Z` — chosen to match the example in DESIGN.md §17.3.
const STARTED_AT_EPOCH_SECS: u64 = 1_778_398_200;

fn started_at() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(STARTED_AT_EPOCH_SECS)
}

fn server_info() -> ServerInfo {
    ServerInfo {
        command: "python".to_string(),
        args: vec!["-m".to_string(), "vibe_trading_mcp".to_string()],
        pid: Some(4242),
        protocol_version: Some("2025-03-26".to_string()),
    }
}

fn passing_metrics() -> ScenarioMetrics {
    ScenarioMetrics {
        latency: LatencyStats {
            p50: Duration::from_micros(12_300),
            p95: Duration::from_micros(45_600),
            p99: Duration::from_micros(89_000),
            p999: Duration::from_micros(120_000),
            mean: Duration::from_micros(23_400),
            min: Duration::from_micros(1_200),
            max: Duration::from_millis(150),
            count: 12_345,
        },
        throughput: ThroughputStats {
            total_requests: 12_345,
            successful_requests: 12_345,
            requests_per_sec: 205.75,
        },
        outcomes: OutcomeCounts {
            success: 12_345,
            ..Default::default()
        },
    }
}

fn failing_metrics() -> ScenarioMetrics {
    ScenarioMetrics {
        latency: LatencyStats {
            p50: Duration::from_micros(12_300),
            p95: Duration::from_micros(45_600),
            // p99 violates a 100ms threshold below.
            p99: Duration::from_micros(123_400),
            p999: Duration::from_micros(456_700),
            mean: Duration::from_micros(23_400),
            min: Duration::from_micros(1_200),
            max: Duration::from_micros(999_900),
            count: 12_345,
        },
        throughput: ThroughputStats {
            total_requests: 12_345,
            successful_requests: 12_300,
            requests_per_sec: 205.75,
        },
        outcomes: OutcomeCounts {
            success: 12_300,
            timeout: 5,
            server_error: 30,
            protocol_error: 10,
            ..Default::default()
        },
    }
}

fn process_stats() -> ProcessStats {
    ProcessStats {
        peak_rss_mb: 156.3,
        final_rss_mb: 142.1,
        avg_cpu_pct: 23.4,
        samples: vec![],
        ..Default::default()
    }
}

fn pass_report() -> Report {
    Report {
        run_id: "01HXYTESTPASS00000000000000".to_string(),
        started_at: started_at(),
        duration: Duration::from_secs(60),
        scenario_name: "sustained".to_string(),
        server_info: server_info(),
        metrics: passing_metrics(),
        process: process_stats(),
        scenario_outcome: ScenarioOutcome {
            total_calls: 12_345,
            successful_calls: 12_345,
            hang_count: 0,
            deadlock_count: 0,
            error_count: 0,
            notes: vec![],
            hung_for_ms: vec![],
        },
        trace_path: Some(PathBuf::from("./trace.jsonl")),
        threshold_violations: vec![],
        coverage: None,
    }
}

fn fail_report() -> Report {
    Report {
        run_id: "01HXYTESTFAIL00000000000000".to_string(),
        started_at: started_at(),
        duration: Duration::from_secs(60),
        scenario_name: "sustained".to_string(),
        server_info: server_info(),
        metrics: failing_metrics(),
        process: process_stats(),
        scenario_outcome: ScenarioOutcome {
            total_calls: 12_345,
            successful_calls: 12_300,
            hang_count: 0,
            deadlock_count: 0,
            error_count: 45,
            notes: vec!["one of three deadlocks occurred at 23s".to_string()],
            hung_for_ms: vec![],
        },
        trace_path: Some(PathBuf::from("./trace.jsonl")),
        threshold_violations: vec![ThresholdViolation {
            kind: ThresholdKind::P99Latency,
            expected: "<= 100ms".to_string(),
            actual: "123.4ms".to_string(),
        }],
        coverage: None,
    }
}

#[test]
fn markdown_pass_snapshot() {
    let r = pass_report();
    let md = MarkdownReporter.render(&r).expect("render markdown pass");
    insta::assert_snapshot!("markdown_pass", md);
}

#[test]
fn markdown_fail_snapshot() {
    let r = fail_report();
    let md = MarkdownReporter.render(&r).expect("render markdown fail");
    insta::assert_snapshot!("markdown_fail", md);
}

#[test]
fn json_pass_snapshot() {
    let r = pass_report();
    let j = JsonReporter.render(&r).expect("render json pass");
    insta::assert_snapshot!("json_pass", j);
}

#[test]
fn json_fail_snapshot() {
    let r = fail_report();
    let j = JsonReporter.render(&r).expect("render json fail");
    insta::assert_snapshot!("json_fail", j);
}

// ---- HTML + terminal snapshots (T3.2) ------------------------------------
//
// An earlier iteration asserted substring landmarks here because the two reporters seemed
// too structurally variable to snapshot. In practice the shared fixture is
// fully deterministic (fixed run ids, epoch-based started_at, no
// Instant-derived fields) and the terminal reporter's colors can be forced
// off, so real snapshots are stable — the landmark tests they supersede
// were retired (plan T3.2).

/// Empty-metrics fixture for "extreme value" coverage: zero calls, zero
/// throughput, zero process samples. Catches divide-by-zero and "skeleton
/// must still render" regressions.
fn empty_report() -> Report {
    Report {
        run_id: "01HXYTESTEMPTY0000000000000".to_string(),
        started_at: started_at(),
        duration: Duration::from_secs(0),
        scenario_name: "empty".to_string(),
        server_info: server_info(),
        metrics: ScenarioMetrics::default(),
        process: ProcessStats::default(),
        scenario_outcome: ScenarioOutcome::default(),
        trace_path: None,
        threshold_violations: vec![],
        coverage: None,
    }
}

#[test]
fn html_pass_snapshot() {
    let html = HtmlReporter
        .render(&pass_report())
        .expect("render html pass");
    insta::assert_snapshot!("html_pass", html);
}

#[test]
fn html_fail_snapshot() {
    let html = HtmlReporter
        .render(&fail_report())
        .expect("render html fail");
    insta::assert_snapshot!("html_fail", html);
}

#[test]
fn html_empty_snapshot() {
    // Extreme-value coverage: zero calls / zero throughput must still render
    // a full document (divide-by-zero guard) with the chart section omitted.
    let html = HtmlReporter
        .render(&empty_report())
        .expect("render html empty");
    assert!(
        !html.contains("Latency distribution"),
        "chart should be omitted for empty metrics"
    );
    insta::assert_snapshot!("html_empty", html);
}

#[test]
fn terminal_pass_snapshot() {
    // Force colors off: the NO_COLOR rendering is the stable contract; the
    // ANSI variant is environment-sensitive by design.
    console::set_colors_enabled(false);
    let s = TerminalReporter
        .render(&pass_report())
        .expect("render terminal pass");
    assert!(!s.contains('\x1b'), "unexpected ANSI escape: {s:?}");
    insta::assert_snapshot!("terminal_pass", s);
}

#[test]
fn terminal_fail_snapshot() {
    console::set_colors_enabled(false);
    let s = TerminalReporter
        .render(&fail_report())
        .expect("render terminal fail");
    insta::assert_snapshot!("terminal_fail", s);
}

#[test]
fn terminal_empty_snapshot() {
    console::set_colors_enabled(false);
    let s = TerminalReporter
        .render(&empty_report())
        .expect("render terminal empty");
    insta::assert_snapshot!("terminal_empty", s);
}

#[test]
fn json_handles_empty_metrics_and_round_trips() {
    let r = empty_report();
    let j = JsonReporter.render(&r).expect("render json empty");
    let v: serde_json::Value =
        serde_json::from_str(&j).expect("json output must be valid serde_json::Value");
    // throughput.total_requests must round-trip as 0 without divide-by-zero.
    // (The JSON schema is flat — see report::json::ReportView — so throughput
    // sits at the top level, not nested under a `result` wrapper.)
    assert_eq!(
        v["throughput"]["total_requests"], 0,
        "expected total_requests=0, got: {j}"
    );
    // run_id must be present and non-empty.
    let run_id = v["run_id"].as_str().expect("run_id should be a string");
    assert!(!run_id.is_empty(), "run_id should be non-empty: {j}");
}
