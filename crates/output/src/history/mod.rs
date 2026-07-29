//! Multi-run baseline history and trend regression analysis.
//!
//! History is deliberately post-run: it consumes the stable v1 metrics wire
//! document rather than changing the runtime `Report` contract. A store keeps
//! one compact JSON sample per run, so history artifacts from independent
//! machines can be merged without an append-file lock.

mod store;
mod trend;
mod types;

pub use store::{HistoryStore, RecordOutcome};
pub use trend::{analyze_trend, render_trend_markdown};
pub use types::{
    HISTORY_SAMPLE_SCHEMA_VERSION, HistoryError, HistorySampleV1, TrendDirection, TrendMetric,
    TrendPolicy, TrendReport, TrendStatus, validate_series_name,
};

/// Result of evaluating prior history and then recording the current sample.
#[derive(Debug)]
pub struct HistoryUpdate {
    /// Trend result calculated before the current sample was stored.
    pub trend: TrendReport,
    /// Whether the current sample was newly created or already present.
    pub record: RecordOutcome,
}

/// Evaluate the prior rolling baseline, then record `current`.
///
/// Recording happens even when the trend regresses. Future baselines ignore
/// failed absolute runs and use a median window, so one bad observation does
/// not silently become the new normal.
pub fn evaluate_and_record(
    store: &HistoryStore,
    current: &HistorySampleV1,
    policy: &TrendPolicy,
) -> Result<HistoryUpdate, HistoryError> {
    let prior = store.load(&current.series)?;
    let trend = analyze_trend(&prior, current, policy)?;
    let record = store.record(current)?;
    Ok(HistoryUpdate { trend, record })
}
