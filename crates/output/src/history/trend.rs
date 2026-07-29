//! Pure rolling-median trend analysis.

use std::cmp::Ordering;
use std::fmt::Write as _;

use super::types::{
    HistoryError, HistorySampleV1, TrendDirection, TrendMetric, TrendPolicy, TrendReport,
    TrendStatus,
};

/// Analyze `current` against the latest comparable passing history samples.
///
/// The current run is always excluded by run id. Samples that failed their
/// own absolute correctness gate or differ in series/scenario/protocol/
/// execution topology do not participate in the baseline.
pub fn analyze_trend(
    samples: &[HistorySampleV1],
    current: &HistorySampleV1,
    policy: &TrendPolicy,
) -> Result<TrendReport, HistoryError> {
    current.validate()?;
    policy.validate()?;

    let mut eligible: Vec<&HistorySampleV1> = samples
        .iter()
        .filter(|sample| {
            sample.passed && sample.run_id != current.run_id && sample.same_cohort(current)
        })
        .collect();
    eligible.sort_by(|left, right| {
        left.started_at
            .cmp(&right.started_at)
            .then_with(|| left.run_id.cmp(&right.run_id))
    });
    let keep_from = eligible.len().saturating_sub(policy.window);
    let eligible = &eligible[keep_from..];

    let metrics = if eligible.is_empty() {
        Vec::new()
    } else {
        build_metrics(eligible, current, policy)
    };
    let warmed_up = eligible.len() >= policy.min_samples;
    let regressions = if warmed_up {
        metrics
            .iter()
            .filter(|metric| metric.gating && metric.direction == TrendDirection::Regressed)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let has_regression = !regressions.is_empty();
    let status = if !warmed_up {
        TrendStatus::WarmingUp
    } else if has_regression {
        TrendStatus::Regressed
    } else {
        TrendStatus::Clean
    };

    Ok(TrendReport {
        series: current.series.clone(),
        current_run_id: current.run_id.clone(),
        status,
        baseline_sample_count: eligible.len(),
        required_sample_count: policy.min_samples,
        metrics,
        regressions,
        has_regression,
    })
}

/// Render a trend report as deterministic Markdown.
pub fn render_trend_markdown(report: &TrendReport) -> String {
    let mut output = String::new();
    output.push_str("# Baseline history\n\n");
    writeln!(output, "- Series: `{}`", markdown_code(&report.series))
        .expect("writing to a String cannot fail");
    writeln!(
        output,
        "- Current run: `{}`",
        markdown_code(&report.current_run_id)
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        "- Baseline samples: {} / {} required",
        report.baseline_sample_count, report.required_sample_count
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        "- Status: **{}**\n",
        match report.status {
            TrendStatus::WarmingUp => "WARMING UP",
            TrendStatus::Clean => "CLEAN",
            TrendStatus::Regressed => "REGRESSION",
        }
    )
    .expect("writing to a String cannot fail");

    if report.metrics.is_empty() {
        output.push_str(
            "> No comparable passing history sample exists yet. The current run can seed the series.\n",
        );
        return output;
    }

    output.push_str("| Metric | Baseline median | Current | Change | Direction | Gate |\n");
    output.push_str("|---|---:|---:|---:|---|---|\n");
    for metric in &report.metrics {
        let change = metric.change_pct.map_or_else(
            || format!("{:+.4}", metric.change),
            |percent| format!("{percent:+.2}%"),
        );
        let direction = match metric.direction {
            TrendDirection::Regressed => "regressed",
            TrendDirection::Improved => "improved",
            TrendDirection::Neutral => "neutral",
        };
        writeln!(
            output,
            "| `{}` | {:.4} | {:.4} | {} | {} | {} |",
            metric.metric,
            metric.baseline,
            metric.current,
            change,
            direction,
            if metric.gating { "yes" } else { "no" },
        )
        .expect("writing to a String cannot fail");
    }

    if report.status == TrendStatus::WarmingUp {
        output.push_str(
            "\n> Relative gates remain disabled until the minimum comparable sample count is reached.\n",
        );
    } else if report.has_regression {
        output.push_str("\n## Regressions\n\n");
        for metric in &report.regressions {
            writeln!(
                output,
                "- `{}`: {:.4} → {:.4}",
                metric.metric, metric.baseline, metric.current
            )
            .expect("writing to a String cannot fail");
        }
    }
    output
}

fn build_metrics(
    samples: &[&HistorySampleV1],
    current: &HistorySampleV1,
    policy: &TrendPolicy,
) -> Vec<TrendMetric> {
    let p50 = median(samples.iter().map(|sample| sample.p50_ms));
    let p95 = median(samples.iter().map(|sample| sample.p95_ms));
    let p99 = median(samples.iter().map(|sample| sample.p99_ms));
    let requests_per_sec = median(samples.iter().map(|sample| sample.requests_per_sec));
    let error_rate = median(samples.iter().map(|sample| sample.error_rate_pct));
    let deadlocks = median(
        samples
            .iter()
            .map(|sample| f64::from(sample.deadlock_count)),
    );
    let hangs = median(samples.iter().map(|sample| f64::from(sample.hang_count)));

    vec![
        informational_ascending("latency_p50_ms", p50, current.p50_ms),
        informational_ascending("latency_p95_ms", p95, current.p95_ms),
        relative_ascending(
            "latency_p99_ms",
            p99,
            current.p99_ms,
            policy.regression.p99_pct,
        ),
        throughput_metric(
            requests_per_sec,
            current.requests_per_sec,
            policy.max_rps_drop_pct,
        ),
        error_rate_metric(
            error_rate,
            current.error_rate_pct,
            policy.regression.error_rate_pp,
        ),
        deadlock_metric(
            deadlocks,
            f64::from(current.deadlock_count),
            policy.regression.deadlock_zero_tolerance,
        ),
        informational_ascending("hang_count", hangs, f64::from(current.hang_count)),
    ]
}

fn informational_ascending(metric: &str, baseline: f64, current: f64) -> TrendMetric {
    let direction = match current.total_cmp(&baseline) {
        Ordering::Greater => TrendDirection::Regressed,
        Ordering::Less => TrendDirection::Improved,
        Ordering::Equal => TrendDirection::Neutral,
    };
    trend_metric(metric, baseline, current, direction, false)
}

fn relative_ascending(
    metric: &str,
    baseline: f64,
    current: f64,
    threshold_pct: f64,
) -> TrendMetric {
    let direction = if baseline <= 0.0 {
        TrendDirection::Neutral
    } else {
        let change = percentage_change(baseline, current).unwrap_or(0.0);
        if change > threshold_pct {
            TrendDirection::Regressed
        } else if change < -threshold_pct {
            TrendDirection::Improved
        } else {
            TrendDirection::Neutral
        }
    };
    trend_metric(metric, baseline, current, direction, true)
}

fn throughput_metric(baseline: f64, current: f64, threshold_pct: Option<f64>) -> TrendMetric {
    let direction = match (baseline > 0.0, threshold_pct) {
        (true, Some(threshold)) => {
            let change = percentage_change(baseline, current).unwrap_or(0.0);
            if change < -threshold {
                TrendDirection::Regressed
            } else if change > threshold {
                TrendDirection::Improved
            } else {
                TrendDirection::Neutral
            }
        }
        _ => TrendDirection::Neutral,
    };
    trend_metric(
        "requests_per_sec",
        baseline,
        current,
        direction,
        threshold_pct.is_some(),
    )
}

fn error_rate_metric(baseline: f64, current: f64, threshold_pp: f64) -> TrendMetric {
    let change = current - baseline;
    let direction = if change > threshold_pp {
        TrendDirection::Regressed
    } else if change < -threshold_pp {
        TrendDirection::Improved
    } else {
        TrendDirection::Neutral
    };
    let mut metric = trend_metric("error_rate_pct", baseline, current, direction, true);
    // Error-rate policy is expressed in percentage points, not relative
    // percent. Keep the generic numeric `change` and suppress change_pct so
    // renderers do not mislabel it.
    metric.change_pct = None;
    metric
}

fn deadlock_metric(baseline: f64, current: f64, zero_tolerance: bool) -> TrendMetric {
    let direction = if current > baseline && zero_tolerance {
        TrendDirection::Regressed
    } else if current < baseline {
        TrendDirection::Improved
    } else {
        TrendDirection::Neutral
    };
    let mut metric = trend_metric(
        "deadlock_count",
        baseline,
        current,
        direction,
        zero_tolerance,
    );
    metric.change_pct = None;
    metric
}

fn trend_metric(
    metric: &str,
    baseline: f64,
    current: f64,
    direction: TrendDirection,
    gating: bool,
) -> TrendMetric {
    TrendMetric {
        metric: metric.to_owned(),
        baseline,
        current,
        change: current - baseline,
        change_pct: percentage_change(baseline, current),
        direction,
        gating,
    }
}

fn percentage_change(baseline: f64, current: f64) -> Option<f64> {
    (baseline > 0.0).then(|| (current - baseline) / baseline * 100.0)
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<f64> = values.collect();
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn markdown_code(value: &str) -> String {
    value.replace('`', "\\`").replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_handles_odd_and_even_sets() {
        assert_eq!(median([3.0, 1.0, 2.0].into_iter()), 2.0);
        assert_eq!(median([4.0, 1.0, 3.0, 2.0].into_iter()), 2.5);
    }
}
