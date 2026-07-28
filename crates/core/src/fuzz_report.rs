//! Fuzz report — tally of interesting findings from a fuzz run.
//!
//! Companion type to the `mcp-loadtest` `scenario::fuzzer::Fuzzer` scenario.
//! The fuzzer emits one [`FuzzFinding`] per iteration; this module aggregates
//! them into a structured [`FuzzReport`] so callers can post-process / render
//! without re-parsing notes.
//!
//! See DESIGN.md §10.5 differentiator entry on protocol fuzzers.
//!
//! M7 ownership: Agent U.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-iteration classification of a fuzz call.
///
/// Maps malformed-payload outcomes to a coarse class for reporting. The
/// granular JSON-RPC code (when available) is preserved alongside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum FuzzClass {
    /// Server tolerated the malformed input and returned a normal result.
    /// Often interesting — means input validation may be too permissive.
    Accepted,
    /// The server explicitly rejected the deliberate fuzz probe in an
    /// expected way: JSON-RPC parse/invalid-request/method/params error, or
    /// `tools/call` returned `isError: true`.
    ///
    /// JSON-RPC internal error (`-32603`) is deliberately excluded: it means
    /// the probe triggered an unexpected server failure.
    ProtocolError,
    /// JSON-RPC internal error (`-32603`), a server-defined error
    /// (`-32000..=-32099`), or any other unexpected server-side error
    /// response.
    ServerError,
    /// Client-side JSON serialize/deserialize failure — typically a malformed
    /// response from the server (not the request).
    ParseError,
    /// No response within `hang_threshold + grace_period`. Server appears
    /// genuinely stuck on the malformed input — likely a parser bug.
    Deadlock,
    /// Transport-level error (pipe closed, IO failure). Usually means the
    /// server crashed or panicked processing the input.
    Disconnected,
    /// We classified the outcome but didn't fit any of the above. Catch-all.
    Other,
}

/// Whether a JSON-RPC error code is an explicit, healthy rejection of a
/// deliberately malformed fuzz probe.
///
/// `-32603` (`Internal error`) is intentionally not included: a fuzzer must
/// not turn an unexpected server failure into a successful probe.
pub const fn is_expected_rejection_code(code: i64) -> bool {
    matches!(code, -32700 | -32600 | -32601 | -32602)
}

/// One row in the fuzz report — what payload variant was sent, what came back,
/// and any one-line note for forensics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzFinding {
    /// Short label identifying the payload variant (e.g. `"UnknownMethod"`).
    pub payload: String,
    /// Coarse class of the outcome.
    pub class: FuzzClass,
    /// JSON-RPC error code, when the server returned a structured error.
    pub code: Option<i64>,
    /// Short human-readable note (server's error message, or our diagnostic).
    pub note: String,
}

/// Aggregate of all findings collected during a fuzz run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FuzzReport {
    /// Total iterations attempted.
    pub total: u64,
    /// Per-class tallies.
    pub by_class: BTreeMap<FuzzClass, u64>,
    /// Tally of JSON-RPC error codes returned by the server (where applicable).
    pub by_code: BTreeMap<i64, u64>,
    /// Up to `max_findings` interesting per-iteration findings. Useful for
    /// the report renderer; bounded to keep the output readable.
    pub findings: Vec<FuzzFinding>,
}

impl FuzzReport {
    /// Build a report from a slice of findings.
    ///
    /// `max_findings` caps the per-row sample in `findings`; full tallies are
    /// preserved in `by_class` / `by_code`.
    pub fn from_findings(findings: &[FuzzFinding], max_findings: usize) -> Self {
        let mut by_class: BTreeMap<FuzzClass, u64> = BTreeMap::new();
        let mut by_code: BTreeMap<i64, u64> = BTreeMap::new();
        for f in findings {
            *by_class.entry(f.class).or_insert(0) += 1;
            if let Some(code) = f.code {
                *by_code.entry(code).or_insert(0) += 1;
            }
        }
        let truncated: Vec<FuzzFinding> = findings.iter().take(max_findings).cloned().collect();
        Self {
            total: findings.len() as u64,
            by_class,
            by_code,
            findings: truncated,
        }
    }

    /// True iff any finding looked dangerous: deadlock, crash-like disconnect,
    /// or a payload that produced a parser error on our side.
    pub fn has_critical(&self) -> bool {
        self.count(FuzzClass::Deadlock) > 0
            || self.count(FuzzClass::Disconnected) > 0
            || self.count(FuzzClass::ParseError) > 0
    }

    /// Count of findings in a given class.
    pub fn count(&self, class: FuzzClass) -> u64 {
        self.by_class.get(&class).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(payload: &str, class: FuzzClass, code: Option<i64>) -> FuzzFinding {
        FuzzFinding {
            payload: payload.to_string(),
            class,
            code,
            note: "n/a".to_string(),
        }
    }

    #[test]
    fn from_findings_tallies_by_class_and_code() {
        let findings = vec![
            f("UnknownMethod", FuzzClass::ProtocolError, Some(-32601)),
            f("UnknownMethod", FuzzClass::ProtocolError, Some(-32601)),
            f("Nested", FuzzClass::ServerError, Some(-32000)),
            f("NumericMethod", FuzzClass::Accepted, None),
        ];
        let report = FuzzReport::from_findings(&findings, 100);
        assert_eq!(report.total, 4);
        assert_eq!(report.count(FuzzClass::ProtocolError), 2);
        assert_eq!(report.count(FuzzClass::ServerError), 1);
        assert_eq!(report.count(FuzzClass::Accepted), 1);
        assert_eq!(report.by_code.get(&-32601), Some(&2));
        assert_eq!(report.by_code.get(&-32000), Some(&1));
        assert!(!report.has_critical());
    }

    #[test]
    fn has_critical_flags_deadlock() {
        let findings = vec![f("Nested", FuzzClass::Deadlock, None)];
        let report = FuzzReport::from_findings(&findings, 100);
        assert!(report.has_critical());
    }

    #[test]
    fn findings_truncate_to_max() {
        let many: Vec<FuzzFinding> = (0..50)
            .map(|i| f(&format!("p{i}"), FuzzClass::Accepted, None))
            .collect();
        let report = FuzzReport::from_findings(&many, 5);
        assert_eq!(report.total, 50);
        assert_eq!(report.findings.len(), 5);
    }

    #[test]
    fn empty_report_defaults() {
        let report = FuzzReport::from_findings(&[], 10);
        assert_eq!(report.total, 0);
        assert!(!report.has_critical());
    }

    #[test]
    fn expected_rejection_code_boundaries_exclude_internal_error() {
        for code in [-32700, -32600, -32601, -32602] {
            assert!(
                is_expected_rejection_code(code),
                "{code} should be an expected fuzz rejection"
            );
        }
        for code in [-32701, -32603, -32599, -32000, 0] {
            assert!(
                !is_expected_rejection_code(code),
                "{code} must remain an unexpected error"
            );
        }
    }
}
