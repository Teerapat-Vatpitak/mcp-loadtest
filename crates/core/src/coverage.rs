//! Coverage tracking — tools registered (per `tools/list`) vs tools exercised
//! (calls actually made). M7 differentiator.
//!
//! M7 ownership: Agent V.
//!
//! Coverage is built at end-of-run by the `mcp-loadtest` run orchestrator
//! (`Run::execute`):
//!
//! 1. Right after `Session` connects, call `session.list_tools()` once and
//!    remember the names — that's the **registered** set.
//! 2. While the scenario drives traffic, each `record_tool(tool, ...)` call
//!    bumps a per-tool counter inside the [`crate::metrics::Recorder`].
//! 3. At end-of-run, snapshot the per-tool counters → that's the **exercised**
//!    map.
//! 4. Combine the two via [`CoverageReport::build`].
//!
//! Per-tool latency SLOs are layered on top via [`ToolSlo`] entries in
//! `ThresholdsConfig::tool_slos` — the run orchestrator evaluates each SLO
//! against the per-tool snapshot and emits a `ThresholdViolation` when the
//! configured p99 budget is exceeded.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Coverage of registered vs. exercised tools for one run.
///
/// **Locked field surface for M7-additive.** New fields are non-breaking; field
/// removal requires a sync.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverageReport {
    /// Tool names returned by `tools/list` at the start of the run.
    pub registered: Vec<String>,
    /// Per-tool call counts collected via [`crate::metrics::Recorder::record_tool`].
    /// Sorted by tool name (BTreeMap) so the report is stable across runs.
    pub exercised: BTreeMap<String, u64>,
    /// Registered tool names that never appeared in `exercised` (i.e. the
    /// scenario never invoked them). Sorted ascending.
    pub unexercised: Vec<String>,
}

impl CoverageReport {
    /// Fraction of `registered` tools that were exercised at least once.
    ///
    /// Returns `100.0` when no tools were registered (no tools to cover; treat
    /// as vacuously full coverage rather than `NaN` so callers can compare
    /// against a budget without a special case).
    pub fn coverage_pct(&self) -> f64 {
        if self.registered.is_empty() {
            return 100.0;
        }
        let covered = self
            .registered
            .iter()
            .filter(|t| self.exercised.contains_key(t.as_str()))
            .count();
        (covered as f64 / self.registered.len() as f64) * 100.0
    }

    /// Build a coverage report from the raw `tools/list` set + the per-tool
    /// counter snapshot.
    ///
    /// `registered` is what the server advertises; `exercised` is what the
    /// scenario actually called. The `unexercised` field is derived: every
    /// `registered` name that's missing from `exercised` (or present with a
    /// zero count) is listed, sorted ascending.
    pub fn build(registered: Vec<String>, exercised: BTreeMap<String, u64>) -> Self {
        let mut unexercised: Vec<String> = registered
            .iter()
            .filter(|name| exercised.get(name.as_str()).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        unexercised.sort();
        Self {
            registered,
            exercised,
            unexercised,
        }
    }
}

/// Per-tool latency SLO — lives in [`crate::config`]; re-exported here so
/// `coverage::ToolSlo` keeps resolving for path compatibility.
pub use crate::config::ToolSlo;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_pct_empty_registered() {
        let c = CoverageReport::default();
        assert_eq!(c.coverage_pct(), 100.0);
    }

    #[test]
    fn coverage_pct_partial() {
        let mut exercised = BTreeMap::new();
        exercised.insert("a".to_string(), 5);
        let c = CoverageReport::build(
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
            exercised,
        );
        // 1 / 4 covered → 25%
        let pct = c.coverage_pct();
        assert!((pct - 25.0).abs() < 1e-9, "expected 25%, got {pct}");
    }

    #[test]
    fn coverage_pct_full() {
        let mut exercised = BTreeMap::new();
        exercised.insert("a".to_string(), 1);
        exercised.insert("b".to_string(), 1);
        let c = CoverageReport::build(vec!["a".to_string(), "b".to_string()], exercised);
        assert!((c.coverage_pct() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn build_lists_unexercised_sorted() {
        let mut exercised = BTreeMap::new();
        exercised.insert("a".to_string(), 1);
        let c = CoverageReport::build(
            vec!["c".to_string(), "a".to_string(), "b".to_string()],
            exercised,
        );
        assert_eq!(c.unexercised, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn build_treats_zero_count_as_unexercised() {
        let mut exercised = BTreeMap::new();
        exercised.insert("a".to_string(), 0);
        exercised.insert("b".to_string(), 3);
        let c = CoverageReport::build(vec!["a".to_string(), "b".to_string()], exercised);
        assert_eq!(c.unexercised, vec!["a".to_string()]);
    }

    #[test]
    fn build_preserves_registered_order() {
        let exercised = BTreeMap::new();
        let c = CoverageReport::build(
            vec!["zeta".to_string(), "alpha".to_string(), "mu".to_string()],
            exercised,
        );
        // Registered order is preserved (input order from tools/list).
        assert_eq!(
            c.registered,
            vec!["zeta".to_string(), "alpha".to_string(), "mu".to_string()]
        );
        // Unexercised is sorted independently for stable diffs.
        assert_eq!(
            c.unexercised,
            vec!["alpha".to_string(), "mu".to_string(), "zeta".to_string()]
        );
    }
}
