//! Prometheus text exposition reporter.
//!
//! The output follows the Prometheus 0.0.4 text format: UTF-8, one sample per
//! line, `HELP`/`TYPE` metadata before samples, and a mandatory final line
//! feed. It represents one completed load-test run and is suitable for a
//! textfile collector or artifact ingestion.

use std::fmt::Write as _;

use crate::report::{Report, ReportError, Reporter};

/// Prometheus 0.0.4 text reporter.
#[derive(Debug, Default, Clone, Copy)]
pub struct PrometheusReporter;

impl Reporter for PrometheusReporter {
    fn render(&self, report: &Report) -> Result<String, ReportError> {
        validate_finite_metrics(report)?;

        let mut output = String::with_capacity(4_096);
        metric_header(
            &mut output,
            "mcp_loadtest_info",
            "Static identity for the completed mcp-loadtest run.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_info{{scenario=\"{}\",protocol_version=\"{}\"}} 1",
            escape_label(&report.scenario_name),
            escape_label(report.server_info.protocol_version.as_deref().unwrap_or("")),
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_run_passed",
            "Whether the completed run passed all correctness and threshold gates.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_run_passed {}",
            u8::from(report.passed())
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_run_duration_seconds",
            "Full lifecycle duration of the completed run in seconds.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_run_duration_seconds {}",
            float(report.duration.as_secs_f64())
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_requests_total",
            "Requests observed by the recorder, partitioned by outcome.",
            "counter",
        );
        let outcomes = &report.metrics.outcomes;
        for (label, count) in [
            ("success", outcomes.success),
            ("expected_rejection", outcomes.expected_rejection),
            ("hang", outcomes.hang),
            ("deadlock", outcomes.deadlock),
            ("timeout", outcomes.timeout),
            ("server_error", outcomes.server_error),
            ("protocol_error", outcomes.protocol_error),
            ("crash", outcomes.crash),
            ("malformed", outcomes.malformed),
            ("disconnected", outcomes.disconnected),
            ("cancelled", outcomes.cancelled),
        ] {
            writeln!(
                output,
                "mcp_loadtest_requests_total{{outcome=\"{label}\"}} {count}"
            )
            .expect("writing to a String cannot fail");
        }

        metric_header(
            &mut output,
            "mcp_loadtest_requests_per_second",
            "Mean aggregate request throughput of the completed run.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_requests_per_second {}",
            float(report.metrics.throughput.requests_per_sec)
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_call_latency_seconds",
            "Observed call latency summary for the completed run.",
            "summary",
        );
        let latency = &report.metrics.latency;
        for (quantile, value) in [
            ("0.5", latency.p50),
            ("0.95", latency.p95),
            ("0.99", latency.p99),
            ("0.999", latency.p999),
        ] {
            writeln!(
                output,
                "mcp_loadtest_call_latency_seconds{{quantile=\"{quantile}\"}} {}",
                float(value.as_secs_f64())
            )
            .expect("writing to a String cannot fail");
        }
        let latency_sum = latency.mean.as_secs_f64() * latency.count as f64;
        writeln!(
            output,
            "mcp_loadtest_call_latency_seconds_sum {}",
            float(latency_sum)
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "mcp_loadtest_call_latency_seconds_count {}",
            latency.count
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_process_resident_memory_bytes",
            "Resident memory observations for the server process.",
            "gauge",
        );
        for (statistic, megabytes) in [
            ("baseline", report.process.baseline_rss_mb),
            ("peak", report.process.peak_rss_mb),
            ("final", report.process.final_rss_mb),
        ] {
            writeln!(
                output,
                "mcp_loadtest_process_resident_memory_bytes{{stat=\"{statistic}\"}} {}",
                float(megabytes * 1_048_576.0)
            )
            .expect("writing to a String cannot fail");
        }

        metric_header(
            &mut output,
            "mcp_loadtest_process_cpu_percent",
            "Mean CPU usage percentage observed for the server process.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_process_cpu_percent {}",
            float(report.process.avg_cpu_pct)
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_process_open_file_descriptors",
            "Best-effort open file descriptor observations.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_process_open_file_descriptors{{stat=\"peak\"}} {}",
            report.process.peak_fd
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "mcp_loadtest_process_open_file_descriptors{{stat=\"final\"}} {}",
            report.process.final_fd
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_process_threads",
            "Best-effort server thread-count observations.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_process_threads{{stat=\"peak\"}} {}",
            report.process.peak_threads
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "mcp_loadtest_process_threads{{stat=\"final\"}} {}",
            report.process.final_threads
        )
        .expect("writing to a String cannot fail");

        metric_header(
            &mut output,
            "mcp_loadtest_correctness_events_total",
            "Scenario-level correctness events observed during the completed run.",
            "counter",
        );
        for (signal, count) in [
            (
                "deadlock",
                u64::from(report.scenario_outcome.deadlock_count),
            ),
            ("hang", u64::from(report.scenario_outcome.hang_count)),
            ("divergence", report.scenario_outcome.divergence_count),
            (
                "incomplete_worker",
                report.scenario_outcome.incomplete_worker_count,
            ),
            (
                "teardown_failure",
                report.scenario_outcome.teardown_failure_count,
            ),
        ] {
            writeln!(
                output,
                "mcp_loadtest_correctness_events_total{{signal=\"{signal}\"}} {count}"
            )
            .expect("writing to a String cannot fail");
        }

        metric_header(
            &mut output,
            "mcp_loadtest_threshold_violations",
            "Number of configured threshold violations in the completed run.",
            "gauge",
        );
        writeln!(
            output,
            "mcp_loadtest_threshold_violations {}",
            report.threshold_violations.len()
        )
        .expect("writing to a String cannot fail");

        if let Some(coverage) = &report.coverage {
            metric_header(
                &mut output,
                "mcp_loadtest_tool_coverage_ratio",
                "Fraction of registered MCP tools exercised by the run.",
                "gauge",
            );
            writeln!(
                output,
                "mcp_loadtest_tool_coverage_ratio {}",
                float(coverage.coverage_pct() / 100.0)
            )
            .expect("writing to a String cannot fail");
        }

        debug_assert!(output.ends_with('\n'));
        Ok(output)
    }
}

fn metric_header(output: &mut String, name: &str, help: &str, metric_type: &str) {
    writeln!(output, "# HELP {name} {}", escape_help(help))
        .expect("writing to a String cannot fail");
    writeln!(output, "# TYPE {name} {metric_type}").expect("writing to a String cannot fail");
}

fn validate_finite_metrics(report: &Report) -> Result<(), ReportError> {
    let latency = &report.metrics.latency;
    for (name, value) in [
        ("duration", report.duration.as_secs_f64()),
        (
            "requests_per_sec",
            report.metrics.throughput.requests_per_sec,
        ),
        ("latency.p50", latency.p50.as_secs_f64()),
        ("latency.p95", latency.p95.as_secs_f64()),
        ("latency.p99", latency.p99.as_secs_f64()),
        ("latency.p999", latency.p999.as_secs_f64()),
        (
            "latency.sum",
            latency.mean.as_secs_f64() * latency.count as f64,
        ),
        ("process.baseline_rss_mb", report.process.baseline_rss_mb),
        ("process.peak_rss_mb", report.process.peak_rss_mb),
        ("process.final_rss_mb", report.process.final_rss_mb),
        ("process.avg_cpu_pct", report.process.avg_cpu_pct),
    ] {
        if !value.is_finite() {
            return Err(ReportError::Other(format!(
                "prometheus output rejected non-finite metric `{name}`"
            )));
        }
    }
    Ok(())
}

fn float(value: f64) -> String {
    if value == 0.0 {
        "0".to_owned()
    } else {
        value.to_string()
    }
}

fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str(r"\\"),
            '"' => escaped.push_str(r#"\""#),
            '\n' => escaped.push_str(r"\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn escape_help(value: &str) -> String {
    value.replace('\\', r"\\").replace('\n', r"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_escaping_is_prometheus_compatible() {
        assert_eq!(escape_label("a\\b\"\nc"), "a\\\\b\\\"\\nc");
    }

    #[test]
    fn help_escaping_covers_backslash_and_newline() {
        assert_eq!(escape_help("a\\b\nc"), "a\\\\b\\nc");
    }
}
