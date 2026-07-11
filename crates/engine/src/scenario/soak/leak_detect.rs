//! Linear-regression leak / drift detector.
//!
//! Extracted from `scenario/soak.rs` during the M8 file-split pass so the
//! scenario file can stay focused on the scenario loop and the helper can
//! grow its own surface (e.g. weighted regression for late-window emphasis,
//! confidence intervals) without bloating the parent.

/// Simple linear-regression slope on a `(t, y)` timeseries.
///
/// Returns `Some(slope)` where slope is `Δy / Δt` (e.g. MB/sec when `y`
/// is RSS-in-MB and `t` is seconds), or `None` if there's not enough data
/// to fit a line (fewer than 2 distinct `t` values).
///
/// Intercept is intentionally discarded — for leak/drift detection only the
/// slope matters. Caller compares against a configured threshold. Two
/// callers exist today: the soak scenario fits its latency-mean trajectory
/// against `Soak::latency_drift_ms_per_sec`, and the run orchestrator's
/// threshold evaluation (`run/thresholds.rs`) fits the sampled RSS series
/// against the opt-in
/// [`mcp_loadtest_core::config::ThresholdsConfig::rss_leak_mb_per_sec`] budget.
///
/// Numerically the implementation uses the centred-data form to avoid
/// catastrophic cancellation on long soaks where `t` is large but `Δt`
/// per sample is small.
///
/// Must stay `pub` (not `pub(crate)`): `tests/soak.rs` is an integration
/// test compiled as an external crate (Cargo treats `tests/*.rs` as
/// separate binaries), so `use mcp_loadtest::scenario::soak::detect_leak;`
/// only resolves against the public API surface. The parent module
/// `scenario::soak` re-exports this function so the historical path
/// `scenario::soak::detect_leak` keeps resolving after the split.
pub fn detect_leak(samples: &[(f64, f64)]) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let n = samples.len() as f64;
    let t_mean: f64 = samples.iter().map(|(t, _)| *t).sum::<f64>() / n;
    let y_mean: f64 = samples.iter().map(|(_, y)| *y).sum::<f64>() / n;

    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (t, y) in samples {
        let dt = t - t_mean;
        num += dt * (y - y_mean);
        den += dt * dt;
    }
    if den.abs() < f64::EPSILON {
        // All `t` values identical → degenerate; can't fit a slope.
        return None;
    }
    Some(num / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_leak_returns_none_for_empty_or_singleton() {
        assert!(detect_leak(&[]).is_none());
        assert!(detect_leak(&[(0.0, 100.0)]).is_none());
    }

    #[test]
    fn detect_leak_returns_none_when_t_is_degenerate() {
        let samples = [(1.0, 10.0), (1.0, 20.0), (1.0, 30.0)];
        assert!(detect_leak(&samples).is_none());
    }

    #[test]
    fn detect_leak_perfect_line_recovers_slope_exactly() {
        // y = 2t + 5 → slope should be exactly 2.0
        let samples: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 2.0 * i as f64 + 5.0)).collect();
        let slope = detect_leak(&samples).expect("regression failed");
        assert!(
            (slope - 2.0).abs() < 1e-9,
            "expected slope=2.0, got {slope}"
        );
    }

    #[test]
    fn detect_leak_flat_line_returns_zero() {
        let samples: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 42.0)).collect();
        let slope = detect_leak(&samples).expect("regression failed");
        assert!(
            slope.abs() < 1e-9,
            "flat line should give slope≈0, got {slope}"
        );
    }

    #[test]
    fn detect_leak_noisy_line_within_tolerance() {
        // True slope 0.5; add small noise. Slope should be close to 0.5.
        let samples = [
            (0.0, 10.05),
            (1.0, 10.55),
            (2.0, 10.98),
            (3.0, 11.51),
            (4.0, 12.04),
            (5.0, 12.46),
            (6.0, 13.02),
            (7.0, 13.49),
        ];
        let slope = detect_leak(&samples).expect("regression failed");
        assert!(
            (slope - 0.5).abs() < 0.05,
            "noisy regression slope=0.5 expected, got {slope}"
        );
    }

    #[test]
    fn detect_leak_descending_returns_negative_slope() {
        let samples: Vec<(f64, f64)> = (0..5).map(|i| (i as f64, 100.0 - i as f64 * 3.0)).collect();
        let slope = detect_leak(&samples).expect("regression failed");
        assert!(
            (slope + 3.0).abs() < 1e-9,
            "descending line should give slope=-3.0, got {slope}"
        );
    }
}
