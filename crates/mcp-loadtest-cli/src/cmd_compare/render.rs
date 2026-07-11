//! Output rendering for the `compare` subcommand. The markdown renderer is
//! the default human-readable output; the JSON path serializes
//! [`CompareReport`] directly via serde and lives in `run` itself, so this
//! module only owns the markdown side.

use super::types::{
    ARROW_IMPROVEMENT, ARROW_REGRESSION, ComparableReport, CompareReport, Direction,
};

/// Render the diff as a Markdown report.
pub(super) fn render_markdown(
    cmp: &CompareReport,
    base: &ComparableReport,
    cur: &ComparableReport,
) -> String {
    let mut out = String::new();
    out.push_str("# Compare baselines\n\n");

    out.push_str(&format!(
        "- Baseline: `{}` (scenario `{}`)\n",
        base.run_id, base.scenario.name
    ));
    out.push_str(&format!(
        "- Current:  `{}` (scenario `{}`)\n\n",
        cur.run_id, cur.scenario.name
    ));

    let banner = if cmp.has_regression {
        "**STATUS: REGRESSION DETECTED**"
    } else {
        "**STATUS: no regressions detected**"
    };
    out.push_str(banner);
    out.push_str("\n\n");

    out.push_str("## Metrics\n\n");
    out.push_str("| Metric | Baseline | Current | Δ | Direction |\n");
    out.push_str("|---|---:|---:|---:|---|\n");
    for m in &cmp.metrics {
        let arrow = match m.direction {
            Direction::Regressed => ARROW_REGRESSION,
            Direction::Improved => ARROW_IMPROVEMENT,
            Direction::Neutral => "—",
        };
        let change_str = format_change(&m.metric, m.change);
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            m.metric, m.baseline, m.current, change_str, arrow
        ));
    }
    out.push('\n');

    if !cmp.regressions.is_empty() {
        out.push_str("## Regressions\n\n");
        for r in &cmp.regressions {
            out.push_str(&format!(
                "- {} `{}`: {} → {} (Δ {})\n",
                ARROW_REGRESSION,
                r.metric,
                r.baseline,
                r.current,
                format_change(&r.metric, r.change),
            ));
        }
        out.push('\n');
        out.push_str(
            "> Regression rules: latency p99 > +10%, error rate > +0.5pp, any deadlock increase.\n",
        );
    } else {
        out.push_str(
            "> No regressions detected by the standard rules (latency p99 > +10%, error rate > +0.5pp, deadlock uptick).\n",
        );
    }
    out
}

/// Format the numeric `change` for a metric, including units where helpful.
fn format_change(metric: &str, change: f64) -> String {
    if metric.starts_with("latency_") {
        format!("{:+.2} ms", change)
    } else if metric == "error_rate_pct" {
        format!("{:+.2} pp", change)
    } else if metric == "requests_per_sec" {
        format!("{:+.2} rps", change)
    } else {
        format!("{:+.0}", change)
    }
}
