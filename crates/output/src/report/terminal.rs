//! Terminal reporter — ANSI-colored compact summary for live console output.
//!
//! Mirrors the structure of the Markdown reporter (`super::markdown`) and the
//! `report.md` template in DESIGN.md §17.3, but rendered as a multi-line
//! string with colors via the `console` crate.
//!
//! Color usage:
//! - Green for the PASS status badge, red for FAIL.
//! - Threshold-violated metrics rendered in red with a trailing `❌` and the
//!   expected condition shown in parentheses.
//! - Numeric callouts (request totals, RPS, percentile values) in cyan.
//! - Yellow for the error-rate row when any error landed.
//!
//! `console::style` automatically respects:
//! - `NO_COLOR` (any value disables colors)
//! - `CLICOLOR=0` (per the clicolors spec)
//! - Non-tty stdout (default — strips ANSI escapes)
//! - `CLICOLOR_FORCE` to override the auto-detect
//!
//! See `console::utils::default_colors_enabled` for the exact rules.
//!
//! # Usage
//!
//! ```ignore
//! use mcp_loadtest::report::{Report, Reporter};
//! use mcp_loadtest::report::terminal::TerminalReporter;
//!
//! # fn _example(report: &Report) -> Result<(), mcp_loadtest::report::ReportError> {
//! let summary = TerminalReporter.render(report)?;
//! println!("{summary}");
//! # Ok(())
//! # }
//! ```

use std::fmt::Write as _;
use std::time::Duration;

use console::style;

use crate::report::common::{describe_failure, fmt_count, fmt_duration, format_server_command};
use crate::report::{Report, ReportError, Reporter, ThresholdKind, ThresholdViolation};
use mcp_loadtest_core::metrics::OutcomeCounts;

/// Terminal reporter — one-shot ANSI-colored summary string.
///
/// Stateless and zero-cost; clone freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct TerminalReporter;

impl Reporter for TerminalReporter {
    fn render(&self, report: &Report) -> Result<String, ReportError> {
        // `writeln!` into `String` returns `fmt::Error` which is documented
        // as never produced by `String`'s `Write` impl; map any unexpected
        // error into `ReportError::Other` for robustness.
        render_inner(report).map_err(|e| ReportError::Other(format!("terminal render: {e}")))
    }
}

fn render_inner(report: &Report) -> Result<String, std::fmt::Error> {
    let mut out = String::with_capacity(1024);

    write_header(&mut out, report)?;
    writeln!(out)?;
    write_throughput(&mut out, report)?;
    writeln!(out)?;
    write_latency(&mut out, report)?;
    write_errors(&mut out, &report.metrics.outcomes)?;
    write_process(&mut out, report)?;
    write_trace(&mut out, report)?;

    Ok(out)
}

// ---- sections ------------------------------------------------------------

fn write_header(out: &mut String, report: &Report) -> std::fmt::Result {
    let status_line = if report.passed() {
        format!("{}", style("PASS").green().bold())
    } else {
        format!(
            "{} ({})",
            style("FAIL").red().bold(),
            describe_failure(report),
        )
    };

    let server_cmd = format_server_command(report);

    writeln!(
        out,
        "{}",
        style("mcp-loadtest run summary").bold().underlined()
    )?;
    writeln!(out, "  status:    {status_line}")?;
    writeln!(out, "  server:    {}", style(server_cmd).dim())?;
    writeln!(out, "  scenario:  {}", report.scenario_name)?;
    writeln!(out, "  duration:  {}", fmt_duration(report.duration))?;

    Ok(())
}

fn write_throughput(out: &mut String, report: &Report) -> std::fmt::Result {
    let tp = &report.metrics.throughput;
    let total = tp.total_requests;
    let ok = tp.successful_requests;
    let errors = total.saturating_sub(ok);
    let error_rate_pct = if total == 0 {
        0.0
    } else {
        (errors as f64) / (total as f64) * 100.0
    };

    let total_s = style(fmt_count(total)).cyan();
    let ok_s = style(fmt_count(ok)).cyan();
    let errors_s = if errors > 0 {
        style(fmt_count(errors)).yellow().to_string()
    } else {
        style(fmt_count(errors)).cyan().to_string()
    };
    let pct_s = if errors > 0 {
        style(format!("({error_rate_pct:.2}%)"))
            .yellow()
            .to_string()
    } else {
        style(format!("({error_rate_pct:.2}%)")).dim().to_string()
    };

    writeln!(
        out,
        "  requests:  {total_s} total · {ok_s} ok · {errors_s} errors {pct_s}",
    )?;

    let rps = format!("{:.1}", tp.requests_per_sec);
    writeln!(out, "  rps:       {}", style(rps).cyan())?;

    Ok(())
}

fn write_latency(out: &mut String, report: &Report) -> std::fmt::Result {
    let lat = &report.metrics.latency;
    let v = &report.threshold_violations;

    writeln!(out, "  latency:")?;
    writeln!(
        out,
        "    p50:     {}",
        render_latency_value(ThresholdKind::P50Latency, lat.p50, v)
    )?;
    writeln!(
        out,
        "    p95:     {}",
        render_latency_value(ThresholdKind::P95Latency, lat.p95, v)
    )?;
    writeln!(
        out,
        "    p99:     {}",
        render_latency_value(ThresholdKind::P99Latency, lat.p99, v)
    )?;
    writeln!(out, "    max:     {}", style(fmt_duration(lat.max)).cyan())?;
    writeln!(out)?;

    Ok(())
}

/// Format a single latency cell. If the metric appears in `violations`,
/// render it red with `❌ (expected …)` so the user can see why it failed
/// without scrolling. Otherwise plain cyan.
fn render_latency_value(
    kind: ThresholdKind,
    value: Duration,
    violations: &[ThresholdViolation],
) -> String {
    let s = fmt_duration(value);
    if let Some(v) = violations.iter().find(|v| v.kind == kind) {
        format!(
            "{} {} ({} {})",
            style(&s).red().bold(),
            style("❌").red(),
            style("expected").dim(),
            style(&v.expected).dim(),
        )
    } else {
        style(s).cyan().to_string()
    }
}

fn write_errors(out: &mut String, outcomes: &OutcomeCounts) -> std::fmt::Result {
    // Match the markdown reporter's category ordering for consistency.
    let rows: Vec<(&'static str, u64)> = [
        ("server_error", outcomes.server_error),
        ("protocol_error", outcomes.protocol_error),
        ("timeout", outcomes.timeout),
        ("crash", outcomes.crash),
        ("malformed", outcomes.malformed),
        ("disconnected", outcomes.disconnected),
        ("cancelled", outcomes.cancelled),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .collect();

    if rows.is_empty() {
        return Ok(());
    }

    write!(out, "  errors:    ")?;
    let mut first = true;
    for (name, count) in rows {
        if !first {
            write!(out, " ")?;
        }
        first = false;
        write!(out, "{}={}", style(name).yellow(), style(count).cyan(),)?;
    }
    writeln!(out)?;
    writeln!(out)?;

    Ok(())
}

fn write_process(out: &mut String, report: &Report) -> std::fmt::Result {
    let p = &report.process;
    // Skip the section if no sampling happened — keeps short reports tidy.
    if p.samples.is_empty() && p.peak_rss_mb == 0.0 && p.final_rss_mb == 0.0 {
        return Ok(());
    }

    let peak = format!("{:.1} MB", p.peak_rss_mb);
    let final_ = format!("{:.1} MB", p.final_rss_mb);
    let cpu = format!("{:.1}%", p.avg_cpu_pct);

    writeln!(
        out,
        "  process:   peak={} · final={} · cpu={}",
        style(peak).cyan(),
        style(final_).cyan(),
        style(cpu).cyan(),
    )?;
    writeln!(out)?;

    Ok(())
}

fn write_trace(out: &mut String, report: &Report) -> std::fmt::Result {
    if let Some(path) = &report.trace_path {
        writeln!(out, "  trace:     {}", style(path.display()).dim())?;
    }
    Ok(())
}

// Formatting helpers (`fmt_duration`, `fmt_count`, `format_server_command`,
// `describe_failure`) live in `report::common`.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use console::set_colors_enabled;

    use crate::report::{
        ProcessStats, Report, Reporter, ServerInfo, ThresholdKind, ThresholdViolation,
    };
    use mcp_loadtest_core::metrics::{
        LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats,
    };
    use mcp_loadtest_core::outcome::ScenarioOutcome;

    use super::*;

    fn sample_report() -> Report {
        Report {
            run_id: "01HXYTEST0000000000000000".to_string(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(60_200),
            scenario_name: "sustained".to_string(),
            server_info: ServerInfo {
                command: "python".to_string(),
                args: vec!["-m".to_string(), "my_mcp".to_string()],
                pid: Some(1234),
                protocol_version: None,
            },
            metrics: ScenarioMetrics {
                latency: LatencyStats {
                    p50: Duration::from_micros(12_300),
                    p95: Duration::from_micros(45_600),
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
                    server_error: 30,
                    protocol_error: 10,
                    timeout: 5,
                    ..Default::default()
                },
            },
            process: ProcessStats {
                peak_rss_mb: 156.3,
                final_rss_mb: 142.1,
                avg_cpu_pct: 23.4,
                samples: vec![],
                ..Default::default()
            },
            scenario_outcome: ScenarioOutcome::default(),
            trace_path: Some(PathBuf::from("./trace.jsonl")),
            threshold_violations: vec![ThresholdViolation {
                kind: ThresholdKind::P99Latency,
                expected: "<= 100ms".to_string(),
                actual: "123.4ms".to_string(),
            }],
            coverage: None,
        }
    }

    // `fmt_duration` / `fmt_count` are tested in `report::common::tests`.

    #[test]
    fn renders_pass_when_no_violations() {
        // Force colors off so the test is portable across platforms / CI tty
        // configs. We're asserting on plain text, not on ANSI sequences.
        set_colors_enabled(false);

        let mut report = sample_report();
        report.threshold_violations.clear();
        let s = TerminalReporter.render(&report).unwrap();

        assert!(s.contains("PASS"), "expected PASS badge, got: {s}");
        assert!(s.contains("python -m my_mcp"));
        assert!(s.contains("sustained"));
        assert!(s.contains("12,345 total"));
        assert!(s.contains("12,300 ok"));
        assert!(s.contains("rps:"));
    }

    #[test]
    fn renders_fail_with_violation_marker() {
        set_colors_enabled(false);

        let s = TerminalReporter.render(&sample_report()).unwrap();

        assert!(s.contains("FAIL"), "expected FAIL badge, got: {s}");
        // Threshold violation shows the metric value followed by the failure
        // marker and the expected condition.
        assert!(s.contains("123.4ms"));
        assert!(s.contains("❌"));
        assert!(s.contains("<= 100ms"));
    }

    #[test]
    fn no_color_environment_strips_ansi() {
        // Belt-and-braces: even with set_colors_enabled(false), output must
        // not contain ANSI CSI escape (`\x1b[`) sequences.
        set_colors_enabled(false);
        let s = TerminalReporter.render(&sample_report()).unwrap();
        assert!(
            !s.contains('\x1b'),
            "rendered output contained ANSI escape with colors disabled: {s:?}"
        );
    }

    #[test]
    fn errors_section_only_appears_when_errors_present() {
        set_colors_enabled(false);

        let mut report = sample_report();
        report.metrics.outcomes = OutcomeCounts::default();
        let s = TerminalReporter.render(&report).unwrap();
        assert!(
            !s.contains("errors:    "),
            "errors line should be absent: {s}"
        );

        // With errors back in, the line should be present.
        let s2 = TerminalReporter.render(&sample_report()).unwrap();
        assert!(s2.contains("errors:    "));
        assert!(s2.contains("server_error=30"));
        assert!(s2.contains("protocol_error=10"));
        assert!(s2.contains("timeout=5"));
    }

    #[test]
    fn process_section_omitted_when_no_samples_and_zero_metrics() {
        set_colors_enabled(false);
        let mut report = sample_report();
        report.process = ProcessStats::default();
        let s = TerminalReporter.render(&report).unwrap();
        assert!(!s.contains("process:"));
    }

    #[test]
    fn process_section_present_when_metrics_set() {
        set_colors_enabled(false);
        let s = TerminalReporter.render(&sample_report()).unwrap();
        assert!(s.contains("process:"));
        assert!(s.contains("156.3 MB"));
        assert!(s.contains("142.1 MB"));
        assert!(s.contains("23.4%"));
    }

    #[test]
    fn trace_line_present_when_trace_path_set() {
        set_colors_enabled(false);
        let s = TerminalReporter.render(&sample_report()).unwrap();
        assert!(s.contains("trace:"));
        assert!(s.contains("trace.jsonl"));
    }
}
