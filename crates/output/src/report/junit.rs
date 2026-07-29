//! JUnit XML reporter for CI test-result surfaces.
//!
//! JUnit XML has no single normative schema shared by every CI vendor. This
//! renderer deliberately emits the conservative `testsuites` → `testsuite` →
//! `testcase` shape understood by Jenkins, GitHub-oriented test reporters,
//! GitLab, and Azure DevOps. One load-test run is one testcase; all typed
//! correctness signals are aggregated into its failure body.

use std::fmt::Write as _;

use crate::report::{Report, ReportError, Reporter, format_iso8601_utc};

/// JUnit XML reporter.
///
/// Free-form scenario notes and server identity are intentionally excluded:
/// they may contain response payloads, command arguments, or credentials.
#[derive(Debug, Default, Clone, Copy)]
pub struct JunitReporter;

impl Reporter for JunitReporter {
    fn render(&self, report: &Report) -> Result<String, ReportError> {
        let failed = !report.passed();
        let failures = u8::from(failed);
        let scenario = xml_escape(&report.scenario_name);
        let run_id = xml_escape(&report.run_id);
        let timestamp = xml_escape(&format_iso8601_utc(report.started_at));
        let duration = format_seconds(report.duration.as_secs_f64());

        let mut output = String::with_capacity(1_024);
        output.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        writeln!(
            output,
            "<testsuites name=\"mcp-loadtest\" tests=\"1\" failures=\"{failures}\" errors=\"0\" time=\"{duration}\">"
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "  <testsuite name=\"mcp-loadtest.{scenario}\" tests=\"1\" failures=\"{failures}\" errors=\"0\" skipped=\"0\" time=\"{duration}\" timestamp=\"{timestamp}\">"
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "    <testcase classname=\"mcp-loadtest.{scenario}\" name=\"run {run_id}\" time=\"{duration}\">"
        )
        .expect("writing to a String cannot fail");

        if failed {
            let summary = failure_summary(report);
            let message = xml_escape(
                summary
                    .lines()
                    .next()
                    .unwrap_or("mcp-loadtest correctness gate failed"),
            );
            let body = xml_escape(&summary);
            writeln!(
                output,
                "      <failure type=\"mcp-loadtest.correctness\" message=\"{message}\">{body}</failure>"
            )
            .expect("writing to a String cannot fail");
        }

        let summary = xml_escape(&safe_run_summary(report));
        writeln!(output, "      <system-out>{summary}</system-out>")
            .expect("writing to a String cannot fail");
        output.push_str("    </testcase>\n");
        output.push_str("  </testsuite>\n");
        output.push_str("</testsuites>\n");
        Ok(output)
    }
}

fn safe_run_summary(report: &Report) -> String {
    format!(
        "run_id={}\nscenario={}\npassed={}\ntotal_requests={}\nsuccessful_requests={}\np99_ms={:.3}\nrequests_per_second={:.6}",
        sanitize_xml_chars(&report.run_id),
        sanitize_xml_chars(&report.scenario_name),
        report.passed(),
        report.metrics.throughput.total_requests,
        report.metrics.throughput.successful_requests,
        report.metrics.latency.p99.as_secs_f64() * 1_000.0,
        report.metrics.throughput.requests_per_sec,
    )
}

fn failure_summary(report: &Report) -> String {
    let outcome = &report.scenario_outcome;
    let recorded = &report.metrics.outcomes;
    let mut reasons = Vec::new();

    if outcome.total_calls == 0 {
        reasons.push("no calls were attempted".to_owned());
    } else if outcome.successful_calls == 0 {
        reasons.push("no scenario-level call succeeded".to_owned());
    }
    if outcome.successful_calls > outcome.total_calls {
        reasons.push(format!(
            "successful call count {} exceeds attempted call count {}",
            outcome.successful_calls, outcome.total_calls
        ));
    }
    push_nonzero(
        &mut reasons,
        "scenario deadlock",
        u64::from(outcome.deadlock_count),
    );
    push_nonzero(
        &mut reasons,
        "response divergence",
        outcome.divergence_count,
    );
    push_nonzero(
        &mut reasons,
        "incomplete pooled worker",
        outcome.incomplete_worker_count,
    );
    push_nonzero(
        &mut reasons,
        "teardown failure",
        outcome.teardown_failure_count,
    );

    if matches!(
        report.scenario_name.as_str(),
        "race_check" | "deadlock_probe" | "fuzzer"
    ) {
        push_nonzero(
            &mut reasons,
            "diagnostic scenario error",
            outcome.error_count,
        );
        push_nonzero(
            &mut reasons,
            "diagnostic scenario hang",
            u64::from(outcome.hang_count),
        );
        push_nonzero(&mut reasons, "recorded diagnostic hang", recorded.hang);
    }

    for (label, count) in [
        ("recorded deadlock", recorded.deadlock),
        ("recorded timeout", recorded.timeout),
        ("recorded protocol error", recorded.protocol_error),
        ("recorded crash", recorded.crash),
        ("recorded malformed response", recorded.malformed),
        ("recorded disconnect", recorded.disconnected),
        ("recorded cancellation", recorded.cancelled),
    ] {
        push_nonzero(&mut reasons, label, count);
    }

    for violation in &report.threshold_violations {
        reasons.push(format!(
            "threshold {}: expected {}, actual {}",
            violation.kind.name(),
            sanitize_xml_chars(&violation.expected),
            sanitize_xml_chars(&violation.actual),
        ));
    }

    if reasons.is_empty() {
        reasons.push("mcp-loadtest correctness gate failed".to_owned());
    }
    reasons.join("\n")
}

fn push_nonzero(reasons: &mut Vec<String>, label: &str, count: u64) {
    if count > 0 {
        let suffix = if count == 1 { "" } else { "s" };
        reasons.push(format!("{count} {label}{suffix}"));
    }
}

fn format_seconds(value: f64) -> String {
    format!("{value:.6}")
}

fn xml_escape(value: &str) -> String {
    let sanitized = sanitize_xml_chars(value);
    let mut escaped = String::with_capacity(sanitized.len());
    for character in sanitized.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn sanitize_xml_chars(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if is_xml_1_0_character(character) {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

fn is_xml_1_0_character(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{a}' | '\u{d}')
        || matches!(character, '\u{20}'..='\u{d7ff}')
        || matches!(character, '\u{e000}'..='\u{fffd}')
        || matches!(character, '\u{10000}'..='\u{10ffff}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_markup_quotes_and_invalid_controls() {
        assert_eq!(xml_escape("a<&>\"'\u{0}z"), "a&lt;&amp;&gt;&quot;&apos;�z");
    }
}
