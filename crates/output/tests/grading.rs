//! Integration tests for `analysis::grading`.
//!
//! Constructs synthetic [`Report`] fixtures and asserts the grading
//! function produces the expected per-dimension and overall grade for
//! the documented thresholds in [`GradingProfile::default_general`].

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{ProcessStats, Report, ServerInfo};
use mcp_loadtest_output::grading::{Grade, GradingProfile, grade};

/// Build a [`Report`] fixture with the given latency p99, total requests,
/// and (success, total) outcome split. Other fields are stubbed at their
/// defaults — grading only inspects metrics.
fn fixture(p99: Duration, total: u64, successful: u64) -> Report {
    assert!(successful <= total, "successful must be <= total");
    let failed = total - successful;
    let outcomes = OutcomeCounts {
        success: successful,
        // Lump failures into ServerError; grade_error_rate sums all variants.
        server_error: failed,
        ..OutcomeCounts::default()
    };

    let metrics = ScenarioMetrics {
        latency: LatencyStats {
            p50: Duration::ZERO,
            p95: Duration::ZERO,
            p99,
            p999: p99,
            mean: p99,
            min: Duration::ZERO,
            max: p99,
            count: total,
        },
        throughput: ThroughputStats {
            total_requests: total,
            successful_requests: successful,
            requests_per_sec: 0.0,
        },
        outcomes,
    };

    Report {
        run_id: "01TESTGRADE".into(),
        started_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_secs(1),
        scenario_name: "synthetic".into(),
        server_info: ServerInfo {
            command: "true".into(),
            args: Vec::new(),
            pid: None,
            protocol_version: None,
        },
        metrics,
        process: ProcessStats::default(),
        scenario_outcome: ScenarioOutcome::default(),
        trace_path: Option::<PathBuf>::None,
        threshold_violations: Vec::new(),
        coverage: None,
    }
}

#[test]
fn default_profile_grades_clean_run_as_a() {
    // p99=20ms (well under 50ms A bound); 1000 requests (well over 100 A bound);
    // 0 errors → all three dimensions land on A, so overall is A.
    let report = fixture(Duration::from_millis(20), 1000, 1000);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(
        g.latency,
        Grade::A,
        "p99=20ms should be A; notes={:?}",
        g.notes
    );
    assert_eq!(
        g.concurrency,
        Grade::A,
        "1000 requests should be A; notes={:?}",
        g.notes
    );
    assert_eq!(
        g.error_rate,
        Grade::A,
        "0 errors should be A; notes={:?}",
        g.notes
    );
    assert_eq!(g.overall, Grade::A);

    // Notes should be one per dimension.
    assert_eq!(g.notes.len(), 3, "expected 3 notes, got {:?}", g.notes);
}

#[test]
fn latency_grade_drops_with_p99() {
    // p99=400ms is above the C bound (250ms) but at-or-below D bound (500ms) → D.
    // Concurrency + error stay A so overall is D.
    let report = fixture(Duration::from_millis(400), 1000, 1000);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(
        g.latency,
        Grade::D,
        "p99=400ms should be D; notes={:?}",
        g.notes
    );
    assert_eq!(g.concurrency, Grade::A);
    assert_eq!(g.error_rate, Grade::A);
    assert_eq!(g.overall, Grade::D);

    // The p99 note should mention the actual ms and the tier bound.
    assert!(
        g.notes[0].contains("400") && g.notes[0].contains("D"),
        "latency note should mention 400ms and grade D, got: {}",
        g.notes[0]
    );
}

#[test]
fn latency_grade_drops_with_high_p99_to_c() {
    // p99=200ms: above B bound (100ms) but at-or-below C bound (250ms) → C.
    let report = fixture(Duration::from_millis(200), 1000, 1000);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(g.latency, Grade::C);
    assert_eq!(g.overall, Grade::C);
}

#[test]
fn error_rate_grade_drops_with_errors() {
    // 200/1000 = 20% errors > 10% D bound → F.
    let report = fixture(Duration::from_millis(20), 1000, 800);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(
        g.error_rate,
        Grade::F,
        "20% errors should be F; notes={:?}",
        g.notes
    );
    assert_eq!(g.overall, Grade::F, "F on any dimension drops overall to F");
}

#[test]
fn overall_is_worst_of_three() {
    // p99=10ms (A); 1000 requests (A); 50% errors (F).
    // Overall must be F because rollup is the worst dimension.
    let report = fixture(Duration::from_millis(10), 1000, 500);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(g.latency, Grade::A);
    assert_eq!(g.concurrency, Grade::A);
    assert_eq!(g.error_rate, Grade::F);
    assert_eq!(
        g.overall,
        Grade::F,
        "overall must be worst of latency/concurrency/error"
    );
}

#[test]
fn concurrency_grade_drops_when_few_requests_driven() {
    // 3 total requests (< D bound 5) → F on concurrency alone, even with
    // perfect latency and no errors.
    let report = fixture(Duration::from_millis(10), 3, 3);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(
        g.concurrency,
        Grade::F,
        "3 requests should be F; notes={:?}",
        g.notes
    );
    assert_eq!(g.latency, Grade::A);
    assert_eq!(g.error_rate, Grade::A);
    assert_eq!(g.overall, Grade::F);
}

#[test]
fn empty_run_grades_f() {
    // 0 requests: error_rate has nothing to measure → F; concurrency F; latency
    // p99=0 still passes A bound. Overall is F.
    let report = fixture(Duration::ZERO, 0, 0);
    let g = grade(&report, &GradingProfile::default_general());

    assert_eq!(g.error_rate, Grade::F);
    assert_eq!(g.concurrency, Grade::F);
    assert_eq!(g.overall, Grade::F);
}

#[test]
fn custom_profile_overrides_defaults() {
    // p99=400ms with a stricter custom profile: A bound 100, B 200, C 300, D 350.
    // 400ms is above all bounds → F.
    let custom = GradingProfile {
        latency_p99_ms: [100.0, 200.0, 300.0, 350.0],
        concurrency: [10, 5, 2, 1],
        error_rate: [0.001, 0.005, 0.01, 0.02],
    };
    let report = fixture(Duration::from_millis(400), 1000, 1000);
    let g = grade(&report, &custom);

    assert_eq!(g.latency, Grade::F);
    assert_eq!(g.overall, Grade::F);
}
