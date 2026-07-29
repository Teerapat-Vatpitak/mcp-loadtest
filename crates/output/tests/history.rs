//! Baseline-history persistence and trend-policy tests.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mcp_loadtest_output::history::{
    HistoryError, HistorySampleV1, HistoryStore, RecordOutcome, TrendDirection, TrendPolicy,
    TrendStatus, analyze_trend, evaluate_and_record, render_trend_markdown, validate_series_name,
};

fn sample(run: usize, p99_ms: f64, rps: f64, passed: bool) -> HistorySampleV1 {
    HistorySampleV1 {
        schema_version: 1,
        series: "main-sustained".to_owned(),
        run_id: format!("01HISTORY{run:016}"),
        started_at: format!("2026-07-29T00:{run:02}:00Z"),
        scenario: "sustained".to_owned(),
        protocol_version: Some("2025-11-25".to_owned()),
        execution_fingerprint: Some("local:1".to_owned()),
        p50_ms: p99_ms / 3.0,
        p95_ms: p99_ms / 2.0,
        p99_ms,
        requests_per_sec: rps,
        error_rate_pct: 0.0,
        deadlock_count: 0,
        hang_count: 0,
        passed,
    }
}

#[test]
fn history_warms_up_then_gates_against_the_median() {
    let current = sample(4, 130.0, 80.0, true);
    let policy = TrendPolicy {
        window: 3,
        min_samples: 3,
        ..TrendPolicy::default()
    };

    let warming =
        analyze_trend(&[sample(1, 90.0, 100.0, true)], &current, &policy).expect("warming trend");
    assert_eq!(warming.status, TrendStatus::WarmingUp);
    assert!(!warming.has_regression);

    let ready = analyze_trend(
        &[
            sample(1, 90.0, 100.0, true),
            sample(2, 100.0, 100.0, true),
            sample(3, 110.0, 100.0, true),
        ],
        &current,
        &policy,
    )
    .expect("ready trend");
    assert_eq!(ready.status, TrendStatus::Regressed);
    assert!(
        ready
            .regressions
            .iter()
            .any(|metric| metric.metric == "latency_p99_ms")
    );
    assert!(
        ready
            .regressions
            .iter()
            .any(|metric| metric.metric == "requests_per_sec")
    );
    let p99 = ready
        .metrics
        .iter()
        .find(|metric| metric.metric == "latency_p99_ms")
        .expect("p99 metric");
    assert_eq!(p99.baseline, 100.0);
    assert_eq!(p99.direction, TrendDirection::Regressed);

    let markdown = render_trend_markdown(&ready);
    assert!(markdown.contains("**REGRESSION**"));
    assert!(markdown.contains("latency_p99_ms"));
}

#[test]
fn failed_and_mismatched_samples_do_not_contaminate_the_cohort() {
    let current = sample(5, 100.0, 100.0, true);
    let mut wrong_protocol = sample(2, 1_000.0, 1.0, true);
    wrong_protocol.protocol_version = Some("2026-07-28".to_owned());
    let mut wrong_topology = sample(3, 1_000.0, 1.0, true);
    wrong_topology.execution_fingerprint = Some("distributed:8".to_owned());
    let report = analyze_trend(
        &[
            sample(1, 1_000.0, 1.0, false),
            wrong_protocol,
            wrong_topology,
            sample(4, 100.0, 100.0, true),
        ],
        &current,
        &TrendPolicy {
            min_samples: 1,
            ..TrendPolicy::default()
        },
    )
    .expect("trend");
    assert_eq!(report.baseline_sample_count, 1);
    assert_eq!(report.status, TrendStatus::Clean);
}

#[test]
fn store_is_idempotent_and_rejects_conflicting_duplicates() {
    let root = temporary_root("idempotent");
    let store = HistoryStore::new(&root);
    let first = sample(1, 100.0, 100.0, true);
    assert_eq!(
        store.record(&first).expect("record first"),
        RecordOutcome::Created
    );
    assert_eq!(
        store.record(&first).expect("record duplicate"),
        RecordOutcome::AlreadyPresent
    );

    let mut conflict = first.clone();
    conflict.p99_ms = 999.0;
    assert!(matches!(
        store.record(&conflict),
        Err(HistoryError::ConflictingDuplicate)
    ));
    assert_eq!(store.load(&first.series).expect("load").len(), 1);
    cleanup(&root);
}

#[test]
fn concurrent_unique_writers_produce_complete_samples() {
    let root = temporary_root("concurrent");
    let store = Arc::new(HistoryStore::new(&root));
    let mut threads = Vec::new();
    for index in 0..16 {
        let store = Arc::clone(&store);
        threads.push(std::thread::spawn(move || {
            store
                .record(&sample(index, 100.0 + index as f64, 100.0, true))
                .expect("record concurrent sample")
        }));
    }
    for thread in threads {
        assert_eq!(
            thread.join().expect("writer thread"),
            RecordOutcome::Created
        );
    }
    let loaded = store.load("main-sustained").expect("load concurrent store");
    assert_eq!(loaded.len(), 16);
    cleanup(&root);
}

#[test]
fn evaluate_records_even_when_the_current_run_regresses() {
    let root = temporary_root("update");
    let store = HistoryStore::new(&root);
    for index in 1..=3 {
        store
            .record(&sample(index, 100.0, 100.0, true))
            .expect("seed");
    }
    let current = sample(4, 200.0, 100.0, true);
    let update = evaluate_and_record(&store, &current, &TrendPolicy::default())
        .expect("evaluate and record");
    assert!(update.trend.has_regression);
    assert_eq!(update.record, RecordOutcome::Created);
    assert_eq!(store.load(&current.series).expect("reload").len(), 4);
    cleanup(&root);
}

#[test]
fn unsafe_series_names_are_rejected_cross_platform() {
    for series in ["", "../escape", "a/b", "with space", "CON", "lpt1.txt"] {
        assert!(validate_series_name(series).is_err(), "{series:?}");
    }
    assert!(validate_series_name("release-0.2_main").is_ok());
}

#[test]
fn corrupt_or_oversized_samples_fail_closed() {
    let root = temporary_root("corrupt");
    let series = root.join("main-sustained");
    std::fs::create_dir_all(&series).expect("create series");
    std::fs::write(series.join("broken.json"), b"{secret payload").expect("write malformed sample");
    let error = HistoryStore::new(&root)
        .load("main-sustained")
        .expect_err("malformed sample must fail");
    assert!(matches!(error, HistoryError::Json { .. }));
    assert!(!error.to_string().contains("secret payload"));

    std::fs::write(series.join("broken.json"), vec![b'x'; 32]).expect("write oversized sample");
    let error = HistoryStore::new(&root)
        .with_limits(16, 10)
        .load("main-sustained")
        .expect_err("oversized sample must fail");
    assert!(matches!(error, HistoryError::SampleTooLarge));
    cleanup(&root);
}

fn temporary_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mcp-loadtest-history-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn cleanup(path: &std::path::Path) {
    assert!(
        path.starts_with(std::env::temp_dir()),
        "cleanup target must remain inside the OS temp directory"
    );
    let _ = std::fs::remove_dir_all(path);
}
