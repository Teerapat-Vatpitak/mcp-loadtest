//! Per-tool metrics state — extracted from `metrics/mod.rs` during the M8
//! file-split pass.
//!
//! `PerToolState` wraps one `ShardedHistogram` + one set of `OutcomeCounters`
//! per tool name. Created lazily the first time `Recorder::record_tool` sees
//! a new tool, then shared by `Arc` so the per-tool counters can be updated
//! without holding the outer map lock.

use std::time::Instant;

use super::histogram::ShardedHistogram;
use super::throughput::OutcomeCounters;

/// Per-tool histogram + outcome counters. Created lazily the first time
/// [`crate::metrics::Recorder::record_tool`] sees a new tool name. Shared by
/// `Arc` so a brief write-lock on the outer map creates the entry, then the
/// per-tool counters can be updated without holding the map lock.
pub(super) struct PerToolState {
    /// When this tool was first seen — used for rps.
    pub(super) start: Instant,
    pub(super) latency: ShardedHistogram,
    pub(super) outcomes: OutcomeCounters,
}

impl PerToolState {
    pub(super) fn new() -> Self {
        Self {
            start: Instant::now(),
            latency: ShardedHistogram::new(),
            outcomes: OutcomeCounters::default(),
        }
    }
}
