//! Scenario outcome data (what every scenario's drive() returns).

use serde::{Deserialize, Serialize};

/// What a scenario reports back to the orchestrator after `drive()` returns.
///
/// **Locked for M2.** Field additions are non-breaking; field removal is.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScenarioOutcome {
    /// Total tool calls attempted (including failures).
    pub total_calls: u64,
    /// Calls that returned successfully within `hang_threshold`.
    pub successful_calls: u64,
    /// Calls that hit `hang_threshold` but returned within `grace_period`.
    pub hang_count: u32,
    /// Calls that didn't respond even after `hang_threshold + grace_period`.
    pub deadlock_count: u32,
    /// Calls that returned an error (server-side or transport).
    pub error_count: u64,
    /// Free-form notes for the report (one per line).
    pub notes: Vec<String>,
    /// Durations, in milliseconds, of calls classified as deadlocks — one
    /// entry per deadlock. Lets machine consumers (e.g. the `serve`
    /// `deadlock_probe` tool) read deadlock durations from a typed field
    /// instead of re-parsing the human-readable `notes` strings. Empty unless
    /// a deadlock was observed; `skip_serializing_if` keeps it out of existing
    /// outputs when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hung_for_ms: Vec<u128>,
}
