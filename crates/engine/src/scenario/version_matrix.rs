//! `version_matrix` scenario — drives the same server once per MCP protocol
//! revision and reports the outcomes side by side.
//!
//! Purpose-built for the multi-revision transition window (ADR 0018): a
//! server may work on the revision its authors tested and deadlock, error,
//! or slow down on another. Each revision gets a **fresh session** spawned
//! through [`SessionFactory::with_version`], a fixed number of
//! [`hang_detect`]-wrapped `tools/call`s, and its own per-tool metric
//! channel under the key `version:<rev>` — so the report's per-tool section
//! becomes the matrix.
//!
//! Revisions are driven sequentially with one session each (no pool): the
//! point is per-revision attribution, not pressure. Combine with
//! `deadlock_probe` / `sustained` for depth on a single revision.
//!
//! [`SessionFactory::with_version`]: mcp_loadtest_protocol::SessionFactory::with_version

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::scenario::{
    RunContext, Scenario, ScenarioOutcome, classify_error, is_terminal_error, teardown,
};
use mcp_loadtest_core::metrics::CallOutcome;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::hang_detector::{HangOutcome, hang_detect};
use mcp_loadtest_protocol::mcp::ProtocolVersion;

/// Drive the same server once per protocol revision and diff the outcomes.
pub struct VersionMatrix {
    /// Revisions to drive, in order. Empty = every supported handshake
    /// revision ([`ProtocolVersion::SUPPORTED`]), oldest first.
    pub versions: Vec<ProtocolVersion>,
    /// `tools/call` invocations issued per revision.
    pub calls_per_version: u32,
    /// Tool to invoke.
    pub tool: String,
    /// Arguments passed on every call.
    pub args: Value,
}

impl VersionMatrix {
    /// The revisions this run will drive (config list, or all supported).
    fn resolved_versions(&self) -> Vec<ProtocolVersion> {
        if self.versions.is_empty() {
            ProtocolVersion::SUPPORTED.to_vec()
        } else {
            self.versions.clone()
        }
    }

    /// Per-tool metric key carrying one revision's samples, e.g.
    /// `version:2025-11-25`.
    pub fn metric_key(version: ProtocolVersion) -> String {
        format!("version:{version}")
    }
}

#[async_trait]
impl Scenario for VersionMatrix {
    async fn drive(&self, _session: &mut Session, ctx: &RunContext) -> ScenarioOutcome {
        let mut outcome = ScenarioOutcome::default();
        let Some(factory) = &ctx.session_factory else {
            outcome.notes.push(
                "version_matrix requires a session factory (Run::execute attaches one); \
                 no-op without it"
                    .to_owned(),
            );
            return outcome;
        };

        for version in self.resolved_versions() {
            if ctx.is_cancelled() {
                outcome
                    .notes
                    .push(format!("cancelled before version {version}"));
                break;
            }
            let key = Self::metric_key(version);

            let mut session = match factory.with_version(version).spawn().await {
                Ok(s) => s,
                Err(e) => {
                    outcome.error_count += 1;
                    ctx.metrics
                        .record_tool(&key, Duration::ZERO, classify_error(&e));
                    outcome.notes.push(format!("{key}: spawn failed: {e}"));
                    continue; // isolate rows — the next revision still runs
                }
            };
            match session.negotiated_version() {
                Some(n) if n == version => {}
                Some(n) => outcome
                    .notes
                    .push(format!("{key}: server negotiated {n} instead")),
                None => outcome.notes.push(format!(
                    "{key}: server answered unknown version `{}`",
                    session.server_protocol_version
                )),
            }

            let (mut ok, mut hangs, mut deadlocks, mut errors) = (0u64, 0u32, 0u32, 0u64);
            for iter in 0..self.calls_per_version {
                if ctx.is_cancelled() {
                    outcome
                        .notes
                        .push(format!("{key}: cancelled at iter={iter}"));
                    break;
                }
                outcome.total_calls += 1;
                let call = session.call_tool(&self.tool, &self.args);
                match hang_detect(call, ctx.hang_threshold, ctx.grace_period).await {
                    HangOutcome::Ok { result, duration } => {
                        if super::is_logical_tool_error(&result) {
                            errors += 1;
                            outcome.error_count += 1;
                            ctx.metrics
                                .record_tool(&key, duration, CallOutcome::ServerError);
                        } else {
                            ok += 1;
                            outcome.successful_calls += 1;
                            ctx.metrics
                                .record_tool(&key, duration, CallOutcome::Success);
                        }
                    }
                    HangOutcome::Slow { result, duration } => {
                        if super::is_logical_tool_error(&result) {
                            errors += 1;
                            outcome.error_count += 1;
                            ctx.metrics
                                .record_tool(&key, duration, CallOutcome::ServerError);
                        } else {
                            hangs += 1;
                            outcome.hang_count += 1;
                            ctx.metrics.record_tool(&key, duration, CallOutcome::Hang);
                        }
                    }
                    HangOutcome::Deadlock { hung_for } => {
                        deadlocks += 1;
                        outcome.deadlock_count += 1;
                        outcome.hung_for_ms.push(hung_for.as_millis());
                        ctx.metrics
                            .record_tool(&key, hung_for, CallOutcome::Deadlock);
                        outcome.notes.push(format!(
                            "{key}: deadlock at iter={iter} hung_for={}ms",
                            hung_for.as_millis()
                        ));
                        // The session is wedged; further calls on this row
                        // would only re-hang. Attribution recorded — move on.
                        break;
                    }
                    HangOutcome::Err(e) => {
                        errors += 1;
                        outcome.error_count += 1;
                        ctx.metrics
                            .record_tool(&key, Duration::ZERO, classify_error(&e));
                        outcome
                            .notes
                            .push(format!("{key}: error at iter={iter}: {e}"));
                        if is_terminal_error(&e) {
                            break;
                        }
                    }
                    // `HangOutcome` is `#[non_exhaustive]`
                    // (mcp-loadtest-protocol): a cross-crate wildcard is
                    // mandatory. Only Ok/Slow/Deadlock/Err exist today; count
                    // any future variant as an error so it is never silently
                    // dropped from the outcome.
                    other => {
                        errors += 1;
                        outcome.error_count += 1;
                        outcome.notes.push(format!(
                            "{key}: unexpected hang outcome at iter={iter}: {other:?}"
                        ));
                    }
                }
            }
            outcome.notes.push(format!(
                "{key}: ok={ok} hangs={hangs} deadlocks={deadlocks} errors={errors}"
            ));

            // Bounded teardown; a wedged row must not stall the matrix, but an
            // uncertain lifecycle must remain a typed report-gating signal.
            teardown::shutdown_session(session, &mut outcome, format!("{key} row")).await;
        }

        outcome
    }

    fn config_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "versions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Protocol revisions to drive (e.g. [\"2025-03-26\", \"2025-11-25\"]). Empty/omitted = every supported revision."
                },
                "calls_per_version": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 10,
                    "description": "tools/call invocations issued per revision."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name to invoke (must be in tools/list)."
                },
                "args": {
                    "type": "object",
                    "description": "Arguments JSON object passed to the tool on every call."
                }
            },
            "required": ["tool"]
        })
    }

    fn name(&self) -> &'static str {
        "version_matrix"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix() -> VersionMatrix {
        VersionMatrix {
            versions: Vec::new(),
            calls_per_version: 2,
            tool: "echo".to_string(),
            args: serde_json::json!({}),
        }
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(matrix().name(), "version_matrix");
    }

    #[test]
    fn config_schema_advertises_required_tool() {
        let schema = matrix().config_schema();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v == "tool"));
    }

    #[test]
    fn empty_versions_resolves_to_all_supported() {
        assert_eq!(matrix().resolved_versions(), ProtocolVersion::SUPPORTED);
    }

    #[test]
    fn explicit_versions_are_kept_in_order() {
        let m = VersionMatrix {
            versions: vec![ProtocolVersion::V2025_11_25, ProtocolVersion::V2025_03_26],
            ..matrix()
        };
        assert_eq!(
            m.resolved_versions(),
            vec![ProtocolVersion::V2025_11_25, ProtocolVersion::V2025_03_26]
        );
    }

    #[test]
    fn metric_key_embeds_the_revision() {
        assert_eq!(
            VersionMatrix::metric_key(ProtocolVersion::V2025_11_25),
            "version:2025-11-25"
        );
    }
}
