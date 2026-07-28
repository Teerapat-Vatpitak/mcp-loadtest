//! Shared formatting helpers for the markdown / terminal / json reporters.
//!
//! Pulled out post-M4 (`/simplify` pass) when the M2-M3 sprints landed five
//! near-identical helpers in `markdown.rs` and `terminal.rs` — `fmt_duration`,
//! `fmt_count`, `format_server_command`, `describe_failure`. `format_iso8601_utc`
//! lives in `mcp-loadtest-core` alongside the `Report` data model it
//! formats; import it via `crate::report::format_iso8601_utc`.

use std::time::Duration;

use crate::report::Report;

/// Format a `Duration` at human-friendly resolution.
///
/// - ≥ 1 s → `"1.23s"` (2 dp)
/// - ≥ 1 ms → `"12.3ms"` (1 dp)
/// - ≥ 1 µs → `"450µs"` (0 dp)
/// - else → `"123ns"`
///
/// Zero rounds to `"0µs"` to match the histogram's lowest bucket semantics.
pub(crate) fn fmt_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0µs".to_string();
    }

    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        return format!("{secs:.2}s");
    }

    let millis = secs * 1_000.0;
    if millis >= 1.0 {
        return format!("{millis:.1}ms");
    }

    let micros = secs * 1_000_000.0;
    if micros >= 1.0 {
        return format!("{micros:.0}µs");
    }

    format!("{nanos}ns")
}

/// Format a request count with thousands separators (e.g., `12,345`).
pub(crate) fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        let from_right = bytes.len() - i;
        if i > 0 && from_right.is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Render `command` + `args` as a single shell-ish string for display.
pub(crate) fn format_server_command(report: &Report) -> String {
    let mut s = report.server_info.command.clone();
    for arg in &report.server_info.args {
        s.push(' ');
        s.push_str(arg);
    }
    s
}

/// Build the parenthesised reason that follows the `FAIL` badge. Mentions
/// every signal that contributed (deadlocks first since they're the loudest).
///
/// Used by both markdown and terminal reporters so the wording stays
/// consistent.
pub(crate) fn describe_failure(report: &Report) -> String {
    let dc = report.scenario_outcome.deadlock_count;
    let divergences = report.scenario_outcome.divergence_count;
    let tv = report.threshold_violations.len();
    let mut parts: Vec<String> = Vec::new();
    if dc > 0 {
        parts.push(format!("{dc} deadlock{}", if dc == 1 { "" } else { "s" }));
    }
    if divergences > 0 {
        parts.push(format!(
            "{divergences} response divergence{}",
            if divergences == 1 { "" } else { "s" }
        ));
    }
    if report.scenario_name == "race_check" && report.scenario_outcome.error_count > 0 {
        parts.push("incomplete race-check cohort".to_owned());
    }
    if report.scenario_name == "deadlock_probe" && report.scenario_outcome.error_count > 0 {
        let errors = report.scenario_outcome.error_count;
        parts.push(format!(
            "{errors} probe error{}",
            if errors == 1 { "" } else { "s" }
        ));
    }
    if report.scenario_name == "fuzzer" && report.scenario_outcome.error_count > 0 {
        let errors = report.scenario_outcome.error_count;
        parts.push(format!(
            "{errors} unexpected fuzzer error{}",
            if errors == 1 { "" } else { "s" }
        ));
    }
    let incomplete_workers = report.scenario_outcome.incomplete_worker_count;
    if incomplete_workers > 0 && report.scenario_name != "race_check" {
        parts.push(format!(
            "{incomplete_workers} incomplete pooled worker{}",
            if incomplete_workers == 1 { "" } else { "s" }
        ));
    }
    let teardown_failures = report.scenario_outcome.teardown_failure_count;
    if teardown_failures > 0 {
        parts.push(format!(
            "{teardown_failures} teardown failure{}",
            if teardown_failures == 1 { "" } else { "s" }
        ));
    }
    if report.scenario_outcome.total_calls == 0 {
        parts.push("no calls attempted".to_owned());
    } else if report.scenario_outcome.successful_calls == 0 && dc == 0 {
        parts.push("no successful calls".to_owned());
    }
    let recorded = &report.metrics.outcomes;
    if dc == 0 && recorded.deadlock > 0 {
        parts.push(format!(
            "{} recorded deadlock{}",
            recorded.deadlock,
            if recorded.deadlock == 1 { "" } else { "s" }
        ));
    }
    let protocol_count = recorded.protocol_error + recorded.malformed;
    if protocol_count > 0 {
        parts.push(format!(
            "{protocol_count} protocol/malformed outcome{}",
            if protocol_count == 1 { "" } else { "s" }
        ));
    }
    let terminal_count =
        recorded.timeout + recorded.crash + recorded.disconnected + recorded.cancelled;
    if terminal_count > 0 {
        parts.push(format!(
            "{terminal_count} terminal session outcome{}",
            if terminal_count == 1 { "" } else { "s" }
        ));
    }
    if tv > 0 {
        parts.push(format!(
            "{tv} threshold {}",
            if tv == 1 { "violation" } else { "violations" }
        ));
    }
    if parts.is_empty() {
        // Defensive fallback for a future unconditional Report::passed signal
        // that this formatter has not learned yet.
        return "unspecified failure".to_string();
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_buckets() {
        assert_eq!(fmt_duration(Duration::from_nanos(0)), "0µs");
        assert_eq!(fmt_duration(Duration::from_nanos(123)), "123ns");
        assert_eq!(fmt_duration(Duration::from_micros(450)), "450µs");
        assert_eq!(fmt_duration(Duration::from_micros(12_300)), "12.3ms");
        assert_eq!(fmt_duration(Duration::from_millis(2_450)), "2.45s");
    }

    #[test]
    fn fmt_count_thousands_separator() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(12_345), "12,345");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }
}
