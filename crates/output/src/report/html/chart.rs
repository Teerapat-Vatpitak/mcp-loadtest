//! Inline SVG latency histogram for the HTML reporter.
//!
//! Extracted from `html.rs` during the M8 file-split pass. The chart is
//! interpolated from the CDF anchors exposed via [`crate::report::Report`]'s
//! `LatencyStats` (raw hdrhistogram buckets aren't surfaced through the
//! locked Report type). Bar heights are interpolated between adjacent
//! percentile anchors — representative shape, not bucket-exact. The chart
//! section is omitted when no latency samples were recorded.

use std::fmt::Write as _;
use std::time::Duration;

use crate::report::Report;
use crate::report::common::fmt_duration;

use super::escape_html;

/// Append a `<h2>Latency distribution</h2><svg>...</svg>` block to `out`.
/// No-op if the report has zero recorded latencies.
pub(super) fn write_latency_chart(out: &mut String, report: &Report) -> std::fmt::Result {
    let lat = &report.metrics.latency;
    if lat.count == 0 {
        return Ok(());
    }
    // CDF anchors from the percentile points we have access to.
    let anchors: [(f64, Duration); 6] = [
        (0.00, lat.min),
        (0.50, lat.p50),
        (0.95, lat.p95),
        (0.99, lat.p99),
        (0.999, lat.p999),
        (1.00, lat.max),
    ];
    const BUCKETS: usize = 20;
    let min_us = lat.min.as_micros().max(1) as f64;
    let max_us = (lat.max.as_micros() as f64).max(min_us + 1.0);
    let log_min = min_us.ln();
    let log_max = max_us.ln();
    let step = (log_max - log_min) / BUCKETS as f64;

    let mut heights = Vec::with_capacity(BUCKETS);
    let mut max_h = 0.0_f64;
    for i in 0..BUCKETS {
        let lo = (log_min + step * i as f64).exp();
        let hi = (log_min + step * (i + 1) as f64).exp();
        let h = (cdf_at(hi, &anchors) - cdf_at(lo, &anchors)).max(0.0);
        if h > max_h {
            max_h = h;
        }
        heights.push((lo, h));
    }
    if max_h <= 0.0 {
        return Ok(());
    }

    const W: u32 = 600;
    const H: u32 = 220;
    const PAD_L: u32 = 40;
    const PAD_B: u32 = 28;
    let plot_w = (W - PAD_L - 8) as f64;
    let plot_h = (H - PAD_B - 8) as f64;
    let bar_w = plot_w / BUCKETS as f64;

    out.push_str("<h2>Latency distribution</h2><div class=\"chart\"><svg viewBox=\"0 0 ");
    let _ = write!(out, "{W} {H}");
    out.push_str(
        "\" xmlns=\"http://www.w3.org/2000/svg\" role=\"img\" aria-label=\"latency histogram\">",
    );
    let _ = write!(
        out,
        "<line x1=\"{PAD_L}\" y1=\"8\" x2=\"{PAD_L}\" y2=\"{}\" stroke=\"#cbd5e1\" />\
<line x1=\"{PAD_L}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#cbd5e1\" />",
        H - PAD_B,
        H - PAD_B,
        W - 8,
        H - PAD_B,
    );
    // Hot bar loop: push_str-based, no per-iter format! allocations for prefix.
    for (i, &(_, h)) in heights.iter().enumerate() {
        let bh = (h / max_h) * plot_h;
        let x = PAD_L as f64 + i as f64 * bar_w + 1.0;
        let y = (H - PAD_B) as f64 - bh;
        let w = (bar_w - 2.0).max(1.0);
        let _ = write!(
            out,
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{bh:.1}\" fill=\"#3b82f6\"/>",
        );
    }
    for &i in &[0_usize, BUCKETS / 3, 2 * BUCKETS / 3, BUCKETS - 1] {
        let (lo, _) = heights[i];
        let cx = PAD_L as f64 + i as f64 * bar_w + bar_w / 2.0;
        let label_raw = fmt_duration(Duration::from_micros(lo as u64));
        let label = escape_html(&label_raw);
        let _ = write!(
            out,
            "<text x=\"{cx:.1}\" y=\"{}\" font-size=\"10\" fill=\"#64748b\" text-anchor=\"middle\">{label}</text>",
            H - PAD_B + 14,
        );
    }
    out.push_str("<text x=\"4\" y=\"14\" font-size=\"10\" fill=\"#64748b\">fraction</text>");
    out.push_str("</svg></div>");
    out.push_str("<p class=\"meta\" style=\"margin-top:8px;\">Buckets log-spaced over the observed range; bar heights interpolated from min / p50 / p95 / p99 / p999 / max anchors.</p>");
    Ok(())
}

/// CDF at `value_us` via linear interpolation between the percentile anchors.
fn cdf_at(value_us: f64, anchors: &[(f64, Duration)]) -> f64 {
    let first = anchors[0];
    if value_us <= first.1.as_micros() as f64 {
        return first.0;
    }
    let last = anchors[anchors.len() - 1];
    if value_us >= last.1.as_micros() as f64 {
        return last.0;
    }
    for w in anchors.windows(2) {
        let lo_us = w[0].1.as_micros() as f64;
        let hi_us = w[1].1.as_micros() as f64;
        if value_us >= lo_us && value_us <= hi_us {
            let span = (hi_us - lo_us).max(1.0);
            let t = (value_us - lo_us) / span;
            return w[0].0 + (w[1].0 - w[0].0) * t;
        }
    }
    last.0
}
