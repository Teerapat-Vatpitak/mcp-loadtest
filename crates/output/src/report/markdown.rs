//! Markdown reporter — PR-ready output (DESIGN §17.3).
//!
//! Renders a [`Report`] into a human-friendly Markdown string suitable for
//! pasting into a PR description, dropping into a CI artifact, or rendering
//! on GitHub. Matches the template in DESIGN.md §17.3 — section ordering,
//! status emoji, and threshold-violation markers are stable.
//!
//! Use via the [`Reporter`] trait:
//!
//! ```ignore
//! use mcp_loadtest::report::{Report, Reporter};
//! use mcp_loadtest::report::markdown::MarkdownReporter;
//!
//! # fn _example(report: &Report) -> Result<(), mcp_loadtest::report::ReportError> {
//! let md = MarkdownReporter.render(report)?;
//! println!("{md}");
//! # Ok(())
//! # }
//! ```

use std::fmt::Write as _;
use std::time::Duration;

use crate::report::common::{describe_failure, fmt_count, fmt_duration, format_server_command};
use crate::report::{
    Report, ReportError, Reporter, ThresholdKind, ThresholdViolation, format_iso8601_utc,
};
use mcp_loadtest_core::metrics::OutcomeCounts;

/// Markdown reporter.
///
/// Stateless and zero-cost; clone freely. See module docs for output shape.
#[derive(Debug, Default, Clone, Copy)]
pub struct MarkdownReporter;

impl Reporter for MarkdownReporter {
    fn render(&self, report: &Report) -> Result<String, ReportError> {
        // `writeln!` into a `String` returns `fmt::Error`, which is documented
        // as never produced by the `String` `Write` impl. Map any unexpected
        // error into `ReportError::Other` for robustness.
        render_inner(report).map_err(|e| ReportError::Other(format!("markdown render: {e}")))
    }
}

fn render_inner(report: &Report) -> Result<String, std::fmt::Error> {
    let mut out = String::with_capacity(2048);

    write_header(&mut out, report)?;
    write_summary(&mut out, report)?;
    write_latency(&mut out, report)?;
    write_errors(&mut out, &report.metrics.outcomes)?;
    write_process(&mut out, report)?;
    write_threshold_violations(&mut out, &report.threshold_violations)?;
    write_trace(&mut out, report)?;

    Ok(out)
}

fn write_header(out: &mut String, report: &Report) -> std::fmt::Result {
    let status = if report.passed() {
        "✅ PASS".to_string()
    } else {
        format!("❌ FAIL ({})", describe_failure(report))
    };

    let server_cmd = format_server_command(report);
    let started = format_iso8601_utc(report.started_at);

    writeln!(out, "# Run {}\n", report.run_id)?;
    writeln!(out, "**Status:** {status}")?;
    writeln!(out, "**Server:** `{server_cmd}`")?;
    writeln!(out, "**Scenario:** {}", report.scenario_name)?;
    writeln!(out, "**Started:** {started}\n")?;

    Ok(())
}

fn write_summary(out: &mut String, report: &Report) -> std::fmt::Result {
    let tp = &report.metrics.throughput;
    let total = tp.total_requests;
    let error_rate = if total == 0 {
        0.0
    } else {
        let errors = total.saturating_sub(tp.successful_requests);
        (errors as f64) / (total as f64) * 100.0
    };

    writeln!(out, "## Summary")?;
    writeln!(out, "- Total requests: {}", fmt_count(total))?;
    writeln!(out, "- Throughput: {:.2} req/s", tp.requests_per_sec)?;
    writeln!(out, "- Error rate: {error_rate:.2}%")?;
    if report.metrics.outcomes.expected_rejection > 0 {
        writeln!(
            out,
            "- Expected rejections: {}",
            fmt_count(report.metrics.outcomes.expected_rejection)
        )?;
    }
    writeln!(
        out,
        "- Deadlocks: {}  Hangs: {}",
        report.scenario_outcome.deadlock_count, report.scenario_outcome.hang_count,
    )?;
    if report.scenario_outcome.teardown_failure_count > 0 {
        writeln!(
            out,
            "- Teardown failures: {}",
            fmt_count(report.scenario_outcome.teardown_failure_count)
        )?;
    }
    writeln!(out)?;

    Ok(())
}

fn write_latency(out: &mut String, report: &Report) -> std::fmt::Result {
    let lat = &report.metrics.latency;

    writeln!(out, "## Latency")?;
    writeln!(out, "| p50 | p95 | p99 | p999 | max |")?;
    writeln!(out, "|---|---|---|---|---|")?;

    let p50 = render_latency_cell(
        ThresholdKind::P50Latency,
        lat.p50,
        &report.threshold_violations,
    );
    let p95 = render_latency_cell(
        ThresholdKind::P95Latency,
        lat.p95,
        &report.threshold_violations,
    );
    let p99 = render_latency_cell(
        ThresholdKind::P99Latency,
        lat.p99,
        &report.threshold_violations,
    );
    let p999 = render_latency_cell(
        ThresholdKind::P999Latency,
        lat.p999,
        &report.threshold_violations,
    );
    let max = fmt_duration(lat.max);

    writeln!(out, "| {p50} | {p95} | {p99} | {p999} | {max} |")?;
    writeln!(out)?;

    Ok(())
}

fn render_latency_cell(
    kind: ThresholdKind,
    value: Duration,
    violations: &[ThresholdViolation],
) -> String {
    let s = fmt_duration(value);
    if violations.iter().any(|v| v.kind == kind) {
        format!("**{s}** ❌")
    } else {
        s
    }
}

fn write_errors(out: &mut String, outcomes: &OutcomeCounts) -> std::fmt::Result {
    writeln!(out, "## Errors")?;

    // Collect non-zero error rows in DESIGN.md §17.2 order. Note: Success and
    // Hang are not errors per se; Hang is informational and Success is the
    // happy path. Per DESIGN.md template, only the error categories appear.
    let rows: Vec<(&'static str, u64)> = [
        ("Timeout", outcomes.timeout),
        ("ServerError", outcomes.server_error),
        ("ProtocolError", outcomes.protocol_error),
        ("Crash", outcomes.crash),
        ("Malformed", outcomes.malformed),
        ("Disconnected", outcomes.disconnected),
        ("Cancelled", outcomes.cancelled),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .collect();

    if rows.is_empty() {
        writeln!(out, "_No errors recorded._")?;
    } else {
        writeln!(out, "| Category | Count |")?;
        writeln!(out, "|---|---|")?;
        for (name, count) in rows {
            writeln!(out, "| {name} | {count} |")?;
        }
    }
    writeln!(out)?;

    Ok(())
}

fn write_process(out: &mut String, report: &Report) -> std::fmt::Result {
    let p = &report.process;
    writeln!(out, "## Process")?;
    writeln!(
        out,
        "Peak RSS: {:.1} MB · Final RSS: {:.1} MB · Avg CPU: {:.1}%",
        p.peak_rss_mb, p.final_rss_mb, p.avg_cpu_pct,
    )?;
    writeln!(out)?;
    Ok(())
}

fn write_threshold_violations(
    out: &mut String,
    violations: &[ThresholdViolation],
) -> std::fmt::Result {
    writeln!(out, "## Threshold violations")?;
    if violations.is_empty() {
        writeln!(out, "_None._")?;
    } else {
        for v in violations {
            writeln!(
                out,
                "- ❌ **{}**: expected {}, got {}",
                v.kind, v.expected, v.actual,
            )?;
        }
    }
    writeln!(out)?;
    Ok(())
}

fn write_trace(out: &mut String, report: &Report) -> std::fmt::Result {
    if let Some(path) = &report.trace_path {
        writeln!(out, "## Trace")?;
        let display = path.display();
        let line_count = report.metrics.throughput.total_requests;
        writeln!(out, "Full trace: `{display}` ({line_count} events)")?;
        writeln!(out)?;
    }
    Ok(())
}

// Formatting helpers (`fmt_duration`, `fmt_count`, `format_server_command`,
// `describe_failure`) live in `report::common` so the terminal reporter can
// share them. `format_iso8601_utc` lives in `mcp-loadtest-core`
// alongside the `Report` data model it formats.

// Helper-function tests live in `report::common::tests` since the helpers
// themselves moved there. Markdown-output assertions live in the
// `tests/reporter_snapshots.rs` insta suite.
