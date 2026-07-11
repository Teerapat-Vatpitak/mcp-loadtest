//! Diff construction — pure functions that build a [`CompareReport`] from
//! two [`ComparableReport`]s and classify per-metric regressions against
//! the canonical thresholds.

use super::types::{ComparableReport, CompareReport, Direction, MetricDiff, RegressionThresholds};

/// Build the structured diff from two reports. Pure function, easily tested.
///
/// `thresholds` controls which metric deltas count as regressions; pass
/// [`RegressionThresholds::default`] for the historical 10% p99 / 0.5pp
/// error-rate / deadlock-zero-tolerance policy.
pub fn build_report(
    base: &ComparableReport,
    cur: &ComparableReport,
    thresholds: &RegressionThresholds,
) -> CompareReport {
    let mut metrics = Vec::new();

    // --- Latency p99 (ms) ------------------------------------------------
    let p99_change = cur.latency_ms.p99 - base.latency_ms.p99;
    let p99_dir = classify_p99(base.latency_ms.p99, cur.latency_ms.p99, thresholds.p99_pct);
    metrics.push(MetricDiff {
        metric: "latency_p99_ms".into(),
        baseline: format!("{:.2}", base.latency_ms.p99),
        current: format!("{:.2}", cur.latency_ms.p99),
        change: p99_change,
        direction: p99_dir,
    });

    // --- Latency p95 (ms) — informational, not a regression gate ---------
    let p95_change = cur.latency_ms.p95 - base.latency_ms.p95;
    let p95_dir = if cur.latency_ms.p95 > base.latency_ms.p95 {
        Direction::Regressed
    } else if cur.latency_ms.p95 < base.latency_ms.p95 {
        Direction::Improved
    } else {
        Direction::Neutral
    };
    // p95 isn't a regression-gate by the rules, so override Regressed→Neutral
    // unless p99 also regressed. Cleaner: leave the raw direction as-is for
    // display, but it does NOT feed into `regressions` below.
    metrics.push(MetricDiff {
        metric: "latency_p95_ms".into(),
        baseline: format!("{:.2}", base.latency_ms.p95),
        current: format!("{:.2}", cur.latency_ms.p95),
        change: p95_change,
        direction: p95_dir,
    });

    // --- Latency p50 (ms) — informational --------------------------------
    let p50_change = cur.latency_ms.p50 - base.latency_ms.p50;
    let p50_dir = if cur.latency_ms.p50 > base.latency_ms.p50 {
        Direction::Regressed
    } else if cur.latency_ms.p50 < base.latency_ms.p50 {
        Direction::Improved
    } else {
        Direction::Neutral
    };
    metrics.push(MetricDiff {
        metric: "latency_p50_ms".into(),
        baseline: format!("{:.2}", base.latency_ms.p50),
        current: format!("{:.2}", cur.latency_ms.p50),
        change: p50_change,
        direction: p50_dir,
    });

    // --- Throughput (rps) — higher is better ----------------------------
    let rps_change = cur.throughput.requests_per_sec - base.throughput.requests_per_sec;
    let rps_dir = if cur.throughput.requests_per_sec > base.throughput.requests_per_sec * 1.001 {
        Direction::Improved
    } else if cur.throughput.requests_per_sec < base.throughput.requests_per_sec * 0.999 {
        Direction::Regressed
    } else {
        Direction::Neutral
    };
    metrics.push(MetricDiff {
        metric: "requests_per_sec".into(),
        baseline: format!("{:.2}", base.throughput.requests_per_sec),
        current: format!("{:.2}", cur.throughput.requests_per_sec),
        change: rps_change,
        direction: rps_dir,
    });

    // --- Error rate (percent) -- regression if it grew > 0.5 pp ----------
    let base_rate = error_rate_pct(base);
    let cur_rate = error_rate_pct(cur);
    let err_change = cur_rate - base_rate;
    let err_dir = classify_error_rate(base_rate, cur_rate, thresholds.error_rate_pp);
    metrics.push(MetricDiff {
        metric: "error_rate_pct".into(),
        baseline: format!("{:.2}", base_rate),
        current: format!("{:.2}", cur_rate),
        change: err_change,
        direction: err_dir,
    });

    // --- Deadlock count — any uptick is a regression --------------------
    let dl_change = cur.deadlock_count as f64 - base.deadlock_count as f64;
    let dl_dir = if cur.deadlock_count > base.deadlock_count {
        // An uptick only gates the build when zero-tolerance is on (default).
        // With it off, the diff still shows the change but it stays out of
        // the `regressions` filter below.
        if thresholds.deadlock_zero_tolerance {
            Direction::Regressed
        } else {
            Direction::Neutral
        }
    } else if cur.deadlock_count < base.deadlock_count {
        Direction::Improved
    } else {
        Direction::Neutral
    };
    metrics.push(MetricDiff {
        metric: "deadlock_count".into(),
        baseline: base.deadlock_count.to_string(),
        current: cur.deadlock_count.to_string(),
        change: dl_change,
        direction: dl_dir,
    });

    // --- Hang count — informational, similar to p95 ---------------------
    let hg_change = cur.hang_count as f64 - base.hang_count as f64;
    let hg_dir = if cur.hang_count > base.hang_count {
        Direction::Regressed
    } else if cur.hang_count < base.hang_count {
        Direction::Improved
    } else {
        Direction::Neutral
    };
    metrics.push(MetricDiff {
        metric: "hang_count".into(),
        baseline: base.hang_count.to_string(),
        current: cur.hang_count.to_string(),
        change: hg_change,
        direction: hg_dir,
    });

    // Regression filter: only the gating metrics count toward the
    // "has_regression" flag and the regressions list. p95/p50/hang are
    // informational only.
    let regressions: Vec<MetricDiff> = metrics
        .iter()
        .filter(|m| {
            m.direction == Direction::Regressed
                && matches!(
                    m.metric.as_str(),
                    "latency_p99_ms" | "error_rate_pct" | "deadlock_count"
                )
        })
        .cloned()
        .collect();

    let has_regression = !regressions.is_empty();

    CompareReport {
        baseline_run_id: base.run_id.clone(),
        current_run_id: cur.run_id.clone(),
        scenario: cur.scenario.name.clone(),
        metrics,
        regressions,
        has_regression,
    }
}

/// Returns Regressed if p99 grew by more than `p99_pct` percent.
pub(super) fn classify_p99(base: f64, cur: f64, p99_pct: f64) -> Direction {
    if base <= 0.0 {
        // Baseline had no p99 sample (zero-measurement run); any non-zero
        // current p99 is "more data, not necessarily worse".
        if cur > 0.0 {
            return Direction::Neutral;
        }
        return Direction::Neutral;
    }
    let pct_change = (cur - base) / base * 100.0;
    if pct_change > p99_pct {
        Direction::Regressed
    } else if pct_change < -p99_pct {
        Direction::Improved
    } else {
        Direction::Neutral
    }
}

/// Error rate as a percentage (0.0..=100.0). Zero requests → 0.
fn error_rate_pct(r: &ComparableReport) -> f64 {
    if r.throughput.total_requests == 0 {
        return 0.0;
    }
    r.errors.total as f64 / r.throughput.total_requests as f64 * 100.0
}

/// Returns Regressed if error rate grew by more than `error_rate_pp`
/// percentage points.
pub(super) fn classify_error_rate(base_pct: f64, cur_pct: f64, error_rate_pp: f64) -> Direction {
    let delta = cur_pct - base_pct;
    if delta > error_rate_pp {
        Direction::Regressed
    } else if delta < -error_rate_pp {
        Direction::Improved
    } else {
        Direction::Neutral
    }
}
