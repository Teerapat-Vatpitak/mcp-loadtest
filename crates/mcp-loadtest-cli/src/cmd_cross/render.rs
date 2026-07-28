//! Markdown rendering for the `cross` subcommand.
//!
//! Split out of `cmd_cross.rs` to keep that file under the 300-line
//! production-code convention. Pure formatting — no I/O, no async.

use std::time::Duration;

use mcp_loadtest::analysis::grading::{GradingProfile, grade};
use mcp_loadtest::report::Report;

use super::{CrossArgs, CrossScenario, ServerRow};

/// Render the cross-comparison as a Markdown report.
pub(super) fn render_markdown(rows: &[ServerRow], args: &CrossArgs) -> String {
    let scenario_label = match args.scenario {
        CrossScenario::Sustained => "sustained",
        CrossScenario::DeadlockProbe => "deadlock_probe",
    };
    let mut out = String::new();
    out.push_str("# Cross-server comparison\n\n");
    out.push_str(&format!(
        "- Scenario: `{}`\n- Tool: `{}`\n- Duration: {}s per server\n- Servers: {}\n\n",
        scenario_label,
        args.tool,
        args.duration.as_secs_f64(),
        rows.len(),
    ));

    // Servers list — separate from the metrics table so failed servers still
    // show up clearly above and below.
    out.push_str("## Servers\n\n");
    for (idx, row) in rows.iter().enumerate() {
        let label = column_label(idx);
        let status = match &row.result {
            Ok(report) if report.passed() => "ok",
            Ok(_) => "FAILED",
            Err(_) => "FAILED",
        };
        out.push_str(&format!("- **{label}**: `{}` — {status}\n", row.command));
    }
    out.push('\n');

    // Metrics table — one column per server, one row per metric.
    out.push_str("## Metrics\n\n");

    // Header row.
    out.push_str("| Metric |");
    for (idx, _) in rows.iter().enumerate() {
        out.push_str(&format!(" {} |", column_label(idx)));
    }
    out.push('\n');
    out.push_str("|---|");
    for _ in rows {
        out.push_str("---:|");
    }
    out.push('\n');

    let profile = GradingProfile::default_general();

    // Per-row formatters keep the table easy to scan; each function plucks
    // a different field out of a Report (or returns "n/a" on a failed run).
    push_metric_row(&mut out, "p50 latency", rows, |r| {
        format_duration_ms(r.metrics.latency.p50)
    });
    push_metric_row(&mut out, "p95 latency", rows, |r| {
        format_duration_ms(r.metrics.latency.p95)
    });
    push_metric_row(&mut out, "p99 latency", rows, |r| {
        format_duration_ms(r.metrics.latency.p99)
    });
    push_metric_row(&mut out, "max latency", rows, |r| {
        format_duration_ms(r.metrics.latency.max)
    });
    push_metric_row(&mut out, "RPS", rows, |r| {
        format!("{:.2}", r.metrics.throughput.requests_per_sec)
    });
    push_metric_row(&mut out, "error rate", rows, |r| {
        let total = r.metrics.throughput.total_requests;
        let success = r.metrics.throughput.successful_requests;
        if total == 0 {
            "0.00%".to_string()
        } else {
            let errors = total.saturating_sub(success);
            format!("{:.2}%", errors as f64 / total as f64 * 100.0)
        }
    });
    push_metric_row(&mut out, "deadlocks", rows, |r| {
        r.scenario_outcome.deadlock_count.to_string()
    });
    push_metric_row(&mut out, "Grade", rows, |r| {
        let g = grade(r, &profile);
        g.overall.name().to_string()
    });

    // Errors section — list any per-server failures with their full message
    // so the user can debug without spelunking through stderr.
    let failures: Vec<&ServerRow> = rows
        .iter()
        .filter(|row| match &row.result {
            Ok(report) => !report.passed(),
            Err(_) => true,
        })
        .collect();
    if !failures.is_empty() {
        out.push_str("\n## Errors\n\n");
        for row in failures {
            if let Err(e) = &row.result {
                out.push_str(&format!("- `{}`: {e:#}\n", row.command));
            } else if let Ok(report) = &row.result {
                out.push_str(&format!(
                    "- `{}`: correctness gate failed ({} deadlocks, {} divergences, \
                     {} incomplete workers, {} teardown failures, \
                     {}/{} successful calls, {} threshold violations)\n",
                    row.command,
                    report.scenario_outcome.deadlock_count,
                    report.scenario_outcome.divergence_count,
                    report.scenario_outcome.incomplete_worker_count,
                    report.scenario_outcome.teardown_failure_count,
                    report.scenario_outcome.successful_calls,
                    report.scenario_outcome.total_calls,
                    report.threshold_violations.len(),
                ));
            }
        }
    }

    out
}

/// Push one row of the metrics table. `extract` runs only on successful
/// reports; failed servers get `"n/a"`.
fn push_metric_row<F>(out: &mut String, label: &str, rows: &[ServerRow], extract: F)
where
    F: Fn(&Report) -> String,
{
    out.push_str(&format!("| {label} |"));
    for row in rows {
        let cell = match &row.result {
            Ok(report) => extract(report),
            Err(_) => "n/a".to_string(),
        };
        out.push_str(&format!(" {cell} |"));
    }
    out.push('\n');
}

/// Letter label for a column: `A`, `B`, ..., then `S1`, `S2`, ... once we
/// run out of single letters. Cross-comparing more than 26 servers in one
/// table is unlikely but the fallback keeps headers unambiguous.
pub(super) fn column_label(idx: usize) -> String {
    if idx < 26 {
        let c = (b'A' + idx as u8) as char;
        c.to_string()
    } else {
        format!("S{}", idx + 1)
    }
}

/// Format a `Duration` as millisecond-precision text. Mirrors the helper in
/// `run.rs` — small enough to inline here rather than expose pub from the lib.
fn format_duration_ms(d: Duration) -> String {
    let total_ms = d.as_secs_f64() * 1000.0;
    format!("{total_ms:.2}ms")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_label_first_letters() {
        assert_eq!(column_label(0), "A");
        assert_eq!(column_label(1), "B");
        assert_eq!(column_label(25), "Z");
        assert_eq!(column_label(26), "S27");
    }

    #[test]
    fn format_duration_ms_two_decimals() {
        assert_eq!(format_duration_ms(Duration::from_millis(1)), "1.00ms");
        assert_eq!(format_duration_ms(Duration::from_micros(1234)), "1.23ms");
    }
}
