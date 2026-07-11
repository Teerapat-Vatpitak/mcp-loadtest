//! Integration tests for `metrics::process::ProcessSampler`.
//!
//! Drives the sampler against the test process's own PID (always
//! discoverable by sysinfo) and against a known-bad PID (`u32::MAX`) to
//! exercise the missing-process branch. We deliberately avoid spawning a
//! Python fixture here — that's covered in `scenarios_basic.rs` /
//! `deadlock.rs` indirectly. Sampling against `std::process::id()` keeps
//! this test fast (< 3s) and platform-portable.

use std::time::{Duration, Instant};

use mcp_loadtest_engine::process::ProcessSampler;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn samples_self_pid_for_2_seconds() {
    // Sample our own process — guaranteed visible to sysinfo regardless of
    // permissions, on every supported OS.
    let pid = std::process::id();
    let cancel = CancellationToken::new();
    let sampler = ProcessSampler::spawn(pid, Duration::from_millis(250), cancel.clone());

    // 2 seconds at 250ms cadence → expect ~7 samples (the very first tick
    // is consumed by `interval.tick().await` above the loop, so the first
    // sampled tick lands at ~250ms; we should see at least 2).
    tokio::time::sleep(Duration::from_secs(2)).await;
    cancel.cancel();

    let stats = sampler.finish().await;

    assert!(
        stats.samples.len() >= 2,
        "expected ≥2 samples in 2s @ 250ms; got {}",
        stats.samples.len()
    );
    assert!(
        stats.peak_rss_mb > 0.0,
        "peak RSS should be > 0 for a live test process; got {}",
        stats.peak_rss_mb
    );
    assert!(
        stats.final_rss_mb > 0.0,
        "final RSS should be > 0 for a live test process; got {}",
        stats.final_rss_mb
    );
    // peak >= final by definition.
    assert!(stats.peak_rss_mb >= stats.final_rss_mb);

    // Samples should be in chronological order.
    let mut prev = 0.0_f64;
    for s in &stats.samples {
        assert!(
            s.at_secs >= prev,
            "samples not chronological: {prev} → {}",
            s.at_secs
        );
        prev = s.at_secs;
    }

    // ── M6 Agent T: fd + thread aggregates ──
    //
    // Thread count: sysinfo's `process.tasks()` is **Linux-only** today.
    // On macOS and Windows it returns `None`, so the per-sample threads
    // field is `0` and the aggregate stays at `0`. We only insist that
    // *if* the platform reports anything, the aggregate is non-zero and
    // peak >= final.
    if stats.peak_threads > 0 {
        assert!(
            stats.final_threads > 0,
            "if threads were observed, final_threads should be non-zero too; got peak={} final={}",
            stats.peak_threads,
            stats.final_threads
        );
        assert!(
            stats.peak_threads >= stats.final_threads,
            "peak_threads ({}) must be >= final_threads ({})",
            stats.peak_threads,
            stats.final_threads
        );
    } else {
        // On macOS / Windows this is expected — sysinfo doesn't expose
        // thread lists there.
        eprintln!(
            "info: peak_threads=0 — expected on non-Linux (sysinfo limitation), \
             actual platform: {}",
            std::env::consts::OS
        );
    }

    // fd count: Linux fills via /proc/<pid>/fd; macOS + Windows degrade
    // to 0 (see `metrics::process::best_effort_fd_count`).
    if cfg!(target_os = "linux") {
        assert!(
            stats.peak_fd > 0,
            "Linux should always report >0 fds for the test process; got peak_fd={}",
            stats.peak_fd
        );
        assert!(stats.peak_fd >= stats.final_fd);
    } else {
        // Non-Linux: degraded to 0 — documented in ProcessSample::fd.
        eprintln!(
            "info: peak_fd=0 — expected on non-Linux (sysinfo 0.32 has no portable fd \
             accessor), actual platform: {}",
            std::env::consts::OS
        );
    }
}

/// The orchestrator's `rss_leak_mb_per_sec` threshold feeds
/// `ProcessStats::samples` straight into `scenario::soak::detect_leak` as
/// `(at_secs, rss_mb)` points. Verify a real sampler run produces a series
/// the regression can actually fit: chronological, finite, non-degenerate
/// time axis.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rss_samples_feed_leak_detector() {
    let pid = std::process::id();
    let cancel = CancellationToken::new();
    let sampler = ProcessSampler::spawn(pid, Duration::from_millis(200), cancel.clone());

    // ~2s at 200ms cadence → expect well over the 3-sample minimum the
    // threshold check documents.
    tokio::time::sleep(Duration::from_secs(2)).await;
    cancel.cancel();
    let stats = sampler.finish().await;

    if stats.samples.len() < 3 {
        // Extreme scheduler starvation — the slope check would skip this
        // series too, so there is nothing meaningful to assert.
        eprintln!(
            "info: only {} samples collected; skipping slope assertions",
            stats.samples.len()
        );
        return;
    }

    for s in &stats.samples {
        assert!(
            s.at_secs.is_finite() && s.rss_mb.is_finite(),
            "sampler must emit finite (at_secs, rss_mb): ({}, {})",
            s.at_secs,
            s.rss_mb
        );
    }

    let series: Vec<(f64, f64)> = stats
        .samples
        .iter()
        .map(|s| (s.at_secs, s.rss_mb))
        .collect();
    let slope = mcp_loadtest_engine::scenario::soak::detect_leak(&series)
        .expect("≥3 chronological samples with distinct at_secs must fit a slope");
    assert!(
        slope.is_finite(),
        "fitted RSS slope must be finite, got {slope}"
    );
    // A short idle test process doesn't leak ~gigabytes per second in
    // either direction; this loose bound catches unit blunders (bytes vs
    // MB, ms vs s) without being flaky.
    assert!(
        slope.abs() < 1024.0,
        "RSS slope of the test process should be far below 1 GiB/s, got {slope} MB/s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_sampling_promptly() {
    let pid = std::process::id();
    let cancel = CancellationToken::new();
    let sampler = ProcessSampler::spawn(pid, Duration::from_millis(100), cancel.clone());

    // Cancel quickly; finish() should return well before any further ticks.
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();

    let start = Instant::now();
    let stats = sampler.finish().await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "finish should return promptly after cancel; took {elapsed:?}"
    );
    // A handful of samples at most — exact count depends on tokio scheduling
    // jitter, but it must not be in the dozens.
    assert!(
        stats.samples.len() <= 10,
        "expected at most a handful of samples; got {}",
        stats.samples.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_pid_returns_empty_without_panicking() {
    // u32::MAX is virtually guaranteed to never be a real PID.
    let cancel = CancellationToken::new();
    let sampler = ProcessSampler::spawn(u32::MAX, Duration::from_millis(100), cancel.clone());

    // Let a few ticks fire so we exercise the "process gone" branch
    // multiple times.
    tokio::time::sleep(Duration::from_millis(350)).await;
    cancel.cancel();

    let stats = sampler.finish().await;

    assert!(
        stats.samples.is_empty(),
        "no samples expected for invalid PID; got {}",
        stats.samples.len()
    );
    assert_eq!(stats.peak_rss_mb, 0.0);
    assert_eq!(stats.final_rss_mb, 0.0);
    assert_eq!(stats.avg_cpu_pct, 0.0);
    assert_eq!(stats.peak_fd, 0);
    assert_eq!(stats.final_fd, 0);
    assert_eq!(stats.peak_threads, 0);
    assert_eq!(stats.final_threads, 0);
}
