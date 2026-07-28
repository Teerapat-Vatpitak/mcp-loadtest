//! Shared fail-closed lifecycle accounting for scenario-owned sessions.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

use mcp_loadtest_protocol::Session;

use super::ScenarioOutcome;

/// Scenario-owned sessions get a scheduling margin above stdio's composed
/// composed internal shutdown budget. Keeping this deadline strictly
/// greater prevents an outer timer from pre-empting forced kill/reap and stderr
/// pump cleanup.
pub(crate) const SCENARIO_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// Shut down one owned session and turn lifecycle uncertainty into a typed,
/// report-gating outcome signal.
pub(crate) async fn shutdown_session(
    session: Session,
    outcome: &mut ScenarioOutcome,
    context: impl Display,
) {
    record_shutdown(
        session.shutdown(),
        SCENARIO_SHUTDOWN_TIMEOUT,
        outcome,
        context,
    )
    .await;
}

async fn record_shutdown<F, E>(
    shutdown: F,
    timeout: Duration,
    outcome: &mut ScenarioOutcome,
    context: impl Display,
) where
    F: Future<Output = Result<(), E>>,
    E: Display,
{
    let context = context.to_string();
    match tokio::time::timeout(timeout, shutdown).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            outcome.teardown_failure_count = outcome.teardown_failure_count.saturating_add(1);
            outcome
                .notes
                .push(format!("{context}: teardown failed: {error}"));
        }
        Err(_) => {
            outcome.teardown_failure_count = outcome.teardown_failure_count.saturating_add(1);
            outcome
                .notes
                .push(format!("{context}: teardown exceeded {timeout:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future;

    use mcp_loadtest_protocol::transport::stdio::StdioTransport;

    use super::*;

    #[test]
    fn outer_budget_has_margin_above_stdio_composite_budget() {
        assert!(
            SCENARIO_SHUTDOWN_TIMEOUT > StdioTransport::SHUTDOWN_BUDGET,
            "outer scenario timeout must not race stdio's internal fallback"
        );
    }

    #[tokio::test]
    async fn shutdown_error_is_a_typed_failure_with_context() {
        let mut outcome = ScenarioOutcome::default();
        record_shutdown(
            future::ready(Err::<(), _>("transport closed")),
            Duration::from_secs(1),
            &mut outcome,
            "race_check worker 3",
        )
        .await;

        assert_eq!(outcome.teardown_failure_count, 1);
        assert_eq!(
            outcome.notes,
            vec!["race_check worker 3: teardown failed: transport closed"]
        );
    }

    #[tokio::test]
    async fn shutdown_timeout_is_a_typed_failure_not_a_warning_only() {
        let mut outcome = ScenarioOutcome::default();
        record_shutdown(
            future::pending::<Result<(), &'static str>>(),
            Duration::from_millis(10),
            &mut outcome,
            "version_matrix row",
        )
        .await;

        assert_eq!(outcome.teardown_failure_count, 1);
        assert_eq!(
            outcome.notes,
            vec!["version_matrix row: teardown exceeded 10ms"]
        );
    }

    #[tokio::test]
    async fn clean_shutdown_does_not_emit_a_signal_or_note() {
        let mut outcome = ScenarioOutcome::default();
        record_shutdown(
            future::ready(Ok::<(), &'static str>(())),
            Duration::from_secs(1),
            &mut outcome,
            "clean",
        )
        .await;

        assert_eq!(outcome.teardown_failure_count, 0);
        assert!(outcome.notes.is_empty());
    }
}
