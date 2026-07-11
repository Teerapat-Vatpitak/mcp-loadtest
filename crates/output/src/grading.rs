//! Performance grading — assigns A/B/C/D/F letter grades to a [`Report`]
//! across three dimensions (latency, concurrency, error rate) and an
//! overall worst-of-three rollup.
//!
//! See DESIGN.md §10.5 (parity entry).
//!
//! # Why grades?
//!
//! reaatech/mcp-load-test surfaces a single-letter score that's easier to
//! eyeball in CI summaries than a wall of numbers. We mirror the shape so
//! parity is obvious; the thresholds in [`GradingProfile::default_general`]
//! are documented as recommendations for general-purpose MCP servers — a
//! real production deployment will tune them via an explicit [`GradingProfile`].
//!
//! # Caveat: concurrency grade is a proxy
//!
//! For M5 the concurrency dimension uses `total_requests` as a proxy for
//! "how much load did the server sustain". That conflates run length with
//! server capacity (a long single-threaded run can score "A" on concurrency
//! even though only one request is in flight at a time). M6 will refine
//! this once `BreakingPointDetector` (Agent M) lands by using
//! `max_concurrency_without_break` instead.

use crate::report::Report;
use mcp_loadtest_core::metrics::OutcomeCounts;

/// Letter grade for a single performance dimension or the overall report.
///
/// Ordering: `A < B < C < D < F` — i.e. the discriminant grows as the grade
/// gets *worse*. `Ord` reflects "worse-than" so `grades.iter().max()` returns
/// the worst grade, which is the rollup we want for the overall score.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Grade {
    /// Best — meets the strictest tier.
    A,
    /// Good.
    B,
    /// Acceptable.
    C,
    /// Poor.
    D,
    /// Worst — exceeds even the most lenient tier.
    F,
}

impl Grade {
    /// Single-character display name (`"A"`, `"B"`, ...).
    pub fn name(&self) -> &'static str {
        match self {
            Grade::A => "A",
            Grade::B => "B",
            Grade::C => "C",
            Grade::D => "D",
            Grade::F => "F",
        }
    }
}

impl std::fmt::Display for Grade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Tier thresholds used to assign per-dimension grades.
///
/// Each `[T; 4]` is the upper bound for grades `[A, B, C, D]`. Anything
/// strictly worse than the `D` bound falls to `F`.
///
/// For monotonically-increasing-is-worse dimensions (latency, error rate),
/// the array is sorted ascending — value `<= [0]` → A, `<= [1]` → B, ...
///
/// For monotonically-decreasing-is-worse dimensions (concurrency throughput),
/// the array is sorted descending — value `>= [0]` → A, `>= [1]` → B, ...
///
/// Tweak per-deployment via `GradingProfile { ..GradingProfile::default_general() }`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GradingProfile {
    /// Tier thresholds for p99 latency in **milliseconds** (ascending).
    ///
    /// Default `[50, 100, 250, 500]`: an MCP server whose 99th-percentile
    /// latency stays under 50ms is A; a server above 500ms is F. These
    /// numbers come from "feels snappy in an LLM tool-use loop" rather than
    /// any vendored SLA — adjust for your tool mix.
    pub latency_p99_ms: [f64; 4],

    /// Tier thresholds for sustained request count (descending).
    ///
    /// Default `[100, 50, 20, 5]`: 100+ successful drives → A, fewer than 5
    /// → F. As noted in the module docs this is a **proxy** for true
    /// concurrency capacity until M6 wires in breaking-point output.
    pub concurrency: [u32; 4],

    /// Tier thresholds for error rate as a fraction `0.0..=1.0` (ascending).
    ///
    /// Default `[0.001, 0.01, 0.05, 0.10]`: 0.1% errors or fewer → A,
    /// over 10% → F. Error rate is `1 - successful / total`; if no requests
    /// were attempted the dimension is graded F (nothing to measure).
    pub error_rate: [f64; 4],
}

impl GradingProfile {
    /// Recommended defaults for general-purpose MCP servers.
    ///
    /// These are the same defaults documented in [`GradingProfile`]'s field
    /// docs. Production deployments with different
    /// targets should construct their own profile instead of mutating this.
    pub fn default_general() -> Self {
        Self {
            latency_p99_ms: [50.0, 100.0, 250.0, 500.0],
            concurrency: [100, 50, 20, 5],
            error_rate: [0.001, 0.01, 0.05, 0.10],
        }
    }
}

impl Default for GradingProfile {
    fn default() -> Self {
        Self::default_general()
    }
}

/// Per-dimension + overall grades for a finished [`Report`].
///
/// `notes` is a human-readable explanation of which tier each dimension
/// landed in — useful for the markdown/terminal reporters and for CI logs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GradeReport {
    /// Worst of `latency` / `concurrency` / `error_rate`. A single overall
    /// score is what the user sees at a glance in `mcp-loadtest run` summary.
    pub overall: Grade,
    /// Grade for `metrics.latency.p99`.
    pub latency: Grade,
    /// Grade for sustained throughput (proxy: `total_requests`).
    pub concurrency: Grade,
    /// Grade for the failure fraction.
    pub error_rate: Grade,
    /// One line per dimension explaining the assigned tier.
    pub notes: Vec<String>,
}

/// Compute a [`GradeReport`] for a finished run.
///
/// Inspects `report.metrics.latency.p99`, `report.metrics.throughput.total_requests`,
/// and the error rate derived from `report.metrics.outcomes`. Each dimension
/// is graded independently; `overall` is the worst of the three.
pub fn grade(report: &Report, profile: &GradingProfile) -> GradeReport {
    let metrics = &report.metrics;

    let p99_ms = metrics.latency.p99.as_secs_f64() * 1_000.0;
    let (latency, latency_note) = grade_ascending(
        p99_ms,
        profile.latency_p99_ms,
        |g, bound| format!("p99 latency {p99_ms:.1}ms -> {g} (<= {bound:.0}ms)"),
        || {
            format!(
                "p99 latency {p99_ms:.1}ms -> F (> {:.0}ms)",
                profile.latency_p99_ms[3]
            )
        },
    );

    let total = metrics.throughput.total_requests;
    // Lift the u32 profile tiers to u64 to match throughput.total_requests.
    let conc_tiers: [u64; 4] = [
        u64::from(profile.concurrency[0]),
        u64::from(profile.concurrency[1]),
        u64::from(profile.concurrency[2]),
        u64::from(profile.concurrency[3]),
    ];
    let (concurrency, concurrency_note) = grade_descending(
        total,
        conc_tiers,
        |g, bound| format!("sustained requests {total} -> {g} (>= {bound})"),
        || format!("sustained requests {total} -> F (< {})", conc_tiers[3]),
    );

    let (error_rate, error_note) = grade_error_rate(&metrics.outcomes, profile);

    let dims = [latency, concurrency, error_rate];
    // `Grade::Ord` is defined so worse > better; `.max()` picks the worst.
    let overall = dims.iter().copied().max().unwrap_or(Grade::F);

    GradeReport {
        overall,
        latency,
        concurrency,
        error_rate,
        notes: vec![latency_note, concurrency_note, error_note],
    }
}

/// Grade a value where smaller is better (latency, error rate).
///
/// Tiers are `[A, B, C, D]` upper bounds; values strictly above the D bound
/// fall to F.
fn grade_ascending<T, FOk, FFail>(
    value: T,
    tiers: [T; 4],
    on_pass: FOk,
    on_fail: FFail,
) -> (Grade, String)
where
    T: PartialOrd + Copy,
    FOk: Fn(Grade, T) -> String,
    FFail: FnOnce() -> String,
{
    const GRADES: [Grade; 4] = [Grade::A, Grade::B, Grade::C, Grade::D];
    for (g, bound) in GRADES.iter().zip(tiers.iter()) {
        if value <= *bound {
            return (*g, on_pass(*g, *bound));
        }
    }
    (Grade::F, on_fail())
}

/// Grade a value where larger is better (throughput / concurrency).
///
/// Tiers are `[A, B, C, D]` lower bounds; values strictly below the D bound
/// fall to F.
fn grade_descending<T, FOk, FFail>(
    value: T,
    tiers: [T; 4],
    on_pass: FOk,
    on_fail: FFail,
) -> (Grade, String)
where
    T: PartialOrd + Copy,
    FOk: Fn(Grade, T) -> String,
    FFail: FnOnce() -> String,
{
    const GRADES: [Grade; 4] = [Grade::A, Grade::B, Grade::C, Grade::D];
    for (g, bound) in GRADES.iter().zip(tiers.iter()) {
        if value >= *bound {
            return (*g, on_pass(*g, *bound));
        }
    }
    (Grade::F, on_fail())
}

/// Compute error rate from `OutcomeCounts` and grade it. If no requests were
/// recorded we grade F — there's nothing to validate.
fn grade_error_rate(outcomes: &OutcomeCounts, profile: &GradingProfile) -> (Grade, String) {
    let total = outcomes.success
        + outcomes.hang
        + outcomes.deadlock
        + outcomes.timeout
        + outcomes.server_error
        + outcomes.protocol_error
        + outcomes.crash
        + outcomes.malformed
        + outcomes.disconnected
        + outcomes.cancelled;

    if total == 0 {
        return (
            Grade::F,
            "error rate: no requests recorded -> F".to_string(),
        );
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "u64 call counts fit f64's 52-bit mantissa in practice; grading tolerances dwarf the error"
    )]
    let rate = 1.0 - (outcomes.success as f64 / total as f64);

    let pct = rate * 100.0;
    grade_ascending(
        rate,
        profile.error_rate,
        |g, bound| format!("error rate {pct:.2}% -> {g} (<= {:.2}%)", bound * 100.0),
        || {
            format!(
                "error rate {pct:.2}% -> F (> {:.2}%)",
                profile.error_rate[3] * 100.0
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_ord_rejects_lower_grade_as_worse() {
        // Sanity: F is "the worst" — should compare greater than A.
        assert!(Grade::F > Grade::A);
        assert!(Grade::A < Grade::B);
        assert!(Grade::C < Grade::D);
    }

    #[test]
    fn grade_name_round_trips() {
        assert_eq!(Grade::A.name(), "A");
        assert_eq!(Grade::F.name(), "F");
        assert_eq!(format!("{}", Grade::C), "C");
    }

    #[test]
    fn ascending_picks_first_tier_passed() {
        let (g, _) = grade_ascending(
            42.0_f64,
            [50.0, 100.0, 250.0, 500.0],
            |g, _| format!("ok {g}"),
            || "fail".into(),
        );
        assert_eq!(g, Grade::A);
    }

    #[test]
    fn ascending_falls_to_f_above_d_bound() {
        let (g, _) = grade_ascending(
            999.0_f64,
            [50.0, 100.0, 250.0, 500.0],
            |g, _| format!("ok {g}"),
            || "fail".into(),
        );
        assert_eq!(g, Grade::F);
    }

    #[test]
    fn descending_picks_first_tier_passed() {
        let (g, _) = grade_descending(
            150_u32,
            [100, 50, 20, 5],
            |g, _| format!("ok {g}"),
            || "fail".into(),
        );
        assert_eq!(g, Grade::A);
    }

    #[test]
    fn descending_falls_to_f_below_d_bound() {
        let (g, _) = grade_descending(
            2_u32,
            [100, 50, 20, 5],
            |g, _| format!("ok {g}"),
            || "fail".into(),
        );
        assert_eq!(g, Grade::F);
    }

    #[test]
    fn error_rate_zero_requests_grades_f() {
        let outcomes = OutcomeCounts::default();
        let profile = GradingProfile::default_general();
        let (g, note) = grade_error_rate(&outcomes, &profile);
        assert_eq!(g, Grade::F);
        assert!(note.contains("no requests"), "note: {note}");
    }
}
