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
    /// Response sets that diverged for identical inputs.
    ///
    /// This is a first-class correctness signal rather than a human-readable
    /// note so [`crate::report::Report::passed`] can reliably gate CI. The
    /// field is omitted when zero to keep existing report JSON stable, and
    /// defaults to zero when older reports are deserialized.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub divergence_count: u64,
    /// Requested pooled workers that never completed their assigned workload.
    ///
    /// Spawn failures, cancelled spawns and worker-task failures increment
    /// this typed signal. It lets [`crate::report::Report::passed`] reject a
    /// silently downgraded concurrency level without treating ordinary
    /// application-level tool errors as unconditional failures.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub incomplete_worker_count: u64,
    /// Session or transport teardown attempts that errored or exceeded their
    /// outer lifecycle deadline.
    ///
    /// This is separate from tool-call errors: a workload can collect valid
    /// measurements and still leave its server lifecycle in an unknown state.
    /// Any non-zero value therefore makes [`crate::report::Report::passed`]
    /// fail closed. Older reports deserialize it as zero and clean reports omit
    /// it from JSON.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub teardown_failure_count: u64,
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

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_failure_is_backward_compatible_and_skips_zero() {
        let older: ScenarioOutcome = serde_json::from_str(
            r#"{
                "total_calls": 1,
                "successful_calls": 1,
                "hang_count": 0,
                "deadlock_count": 0,
                "error_count": 0,
                "notes": []
            }"#,
        )
        .expect("older outcome shape should deserialize");
        assert_eq!(older.teardown_failure_count, 0);

        let clean = serde_json::to_value(&older).expect("serialize clean outcome");
        assert!(clean.get("teardown_failure_count").is_none());

        let failed = ScenarioOutcome {
            teardown_failure_count: 2,
            ..older
        };
        let failed = serde_json::to_value(failed).expect("serialize failed teardown");
        assert_eq!(failed["teardown_failure_count"], 2);
    }
}
