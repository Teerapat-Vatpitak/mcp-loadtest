//! Bounded, cancellation-safe teardown for [`StdioTransport`].
//!
//! Each phase has its own budget. The composite budget is exposed through
//! [`StdioTransport::SHUTDOWN_BUDGET`] so callers that add an outer timeout can
//! leave a scheduling margin instead of racing an inner deadline.

use std::io;
use std::time::Duration;

use super::StdioTransport;
use crate::transport::TransportError;

/// Let an EOF-aware server perform its normal cleanup first.
const GRACEFUL_CHILD_EXIT_BUDGET: Duration = Duration::from_secs(5);
/// After requesting termination, wait for the OS to confirm exit/reap.
///
/// Windows can keep a process observable as live for more than two seconds
/// after accepting `TerminateProcess` when many subprocesses exit together.
/// Give forced cleanup the same bounded allowance as graceful cleanup; the
/// final direct process-table probe still decides success versus timeout.
const FORCED_CHILD_REAP_BUDGET: Duration = Duration::from_secs(5);
/// After the child exits, let the stderr pump drain the closed pipe through
/// EOF. Cancelling before this phase completes can discard bytes that were
/// already written by the child but still queued in the pipe/reader.
const STDERR_PUMP_DRAIN_BUDGET: Duration = Duration::from_secs(2);
/// If EOF draining stalls, let the cancellation arm flush bytes that have
/// already reached the capture file before aborting the task.
const STDERR_PUMP_CANCEL_BUDGET: Duration = Duration::from_secs(2);

/// Sum of the phase budgets, excluding scheduler overhead.
pub(super) const SHUTDOWN_BUDGET: Duration = Duration::from_secs(
    GRACEFUL_CHILD_EXIT_BUDGET.as_secs()
        + FORCED_CHILD_REAP_BUDGET.as_secs()
        + STDERR_PUMP_DRAIN_BUDGET.as_secs()
        + STDERR_PUMP_CANCEL_BUDGET.as_secs(),
);

/// Close stdin, terminate/reap the child if graceful EOF shutdown stalls, then
/// stop the stderr pump without ever detaching its [`tokio::task::JoinHandle`].
pub(super) async fn run(transport: &mut StdioTransport) -> Result<(), TransportError> {
    // Closing the parent's pipe handle is the graceful shutdown signal for a
    // line-framed stdio server.
    drop(transport.stdin.take());

    let child_result = stop_and_reap_child(transport).await;
    let pump_result = stop_stderr_pump(transport).await;

    match (child_result, pump_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(child_error), Err(pump_error)) => Err(TransportError::Other(format!(
            "child teardown failed: {child_error}; stderr pump teardown failed: {pump_error}"
        ))),
    }
}

async fn stop_and_reap_child(transport: &mut StdioTransport) -> Result<(), TransportError> {
    let Some(child) = transport.child.as_mut() else {
        return Ok(());
    };

    match tokio::time::timeout(GRACEFUL_CHILD_EXIT_BUDGET, child.wait()).await {
        Ok(Ok(_status)) => return Ok(()),
        Ok(Err(_wait_error)) => {
            // A failed asynchronous wait may still be recoverable by
            // terminating and then waiting again, so use the same bounded
            // cleanup path below. Preserve an immediately observable OS
            // status first: on Windows the registered wait callback can fail
            // or lag even though `try_wait` can already see the process exit.
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
        }
        Err(_) => {
            // Tokio's Windows process waiter is notified through
            // RegisterWaitForSingleObject. Under process-heavy contention the
            // callback can arrive after the OS process has already exited.
            // Reconcile directly with the OS before escalating a delayed
            // notification into an unnecessary forced termination.
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
        }
    }

    // `start_kill` only requests termination. Always follow it with a bounded
    // wait so successful shutdown means the process actually exited/reaped.
    // A kill error can race a natural exit, so a successful wait still wins.
    let kill_result = child.start_kill();
    match tokio::time::timeout(FORCED_CHILD_REAP_BUDGET, child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(wait_error)) => match child.try_wait() {
            Ok(Some(_status)) => Ok(()),
            Ok(None) => Err(TransportError::Io(wait_error)),
            Err(probe_error) => Err(TransportError::Other(format!(
                "forced child wait failed ({wait_error}); final exit-status probe failed: \
                 {probe_error}"
            ))),
        },
        Err(_) => {
            // Do not report a false teardown failure merely because Tokio's
            // async process-exit callback was delayed. `try_wait` directly
            // observes and reaps the child when the OS has completed the
            // termination request.
            resolve_forced_wait_timeout(
                child.try_wait().map(|status| status.is_some()),
                kill_result,
            )
        }
    }
}

/// Reconcile an expired async forced-wait with the process table before
/// reporting a timeout. Tokio's registered Windows wait notification can lag
/// the observable process state under contention; only `Ok(false)` proves the
/// child is still live after the final direct probe.
fn resolve_forced_wait_timeout(
    exit_probe: io::Result<bool>,
    kill_result: io::Result<()>,
) -> Result<(), TransportError> {
    match exit_probe {
        Ok(true) => Ok(()),
        Ok(false) => match kill_result {
            Ok(()) => Err(TransportError::Timeout(FORCED_CHILD_REAP_BUDGET)),
            Err(kill_error) => Err(TransportError::Io(kill_error)),
        },
        Err(probe_error) => Err(TransportError::Io(probe_error)),
    }
}

async fn stop_stderr_pump(transport: &mut StdioTransport) -> Result<(), TransportError> {
    let Some(handle) = transport.stderr_pump.as_mut() else {
        return Ok(());
    };

    // After a successful child phase its stderr pipe is closed. Let the pump
    // observe EOF and drain every queued byte before cancellation. If child
    // teardown failed and the pipe stayed open, this bounded wait falls through
    // to the cancellation path below. Await through the field rather than
    // taking the JoinHandle: if an outer timeout cancels this future,
    // StdioTransport::drop still owns and aborts the task.
    if let Ok(joined) = tokio::time::timeout(STDERR_PUMP_DRAIN_BUDGET, &mut *handle).await {
        transport.stderr_pump.take();
        return map_pump_join(joined);
    }

    // EOF draining exceeded its bound. Cancellation may flush bytes already
    // written to the file, but it deliberately does not claim a clean
    // teardown: unread pipe bytes are now uncertain, so even a clean task join
    // below returns the original drain timeout.
    transport.pump_cancel.cancel();
    let Some(handle) = transport.stderr_pump.as_mut() else {
        return Err(TransportError::Other(
            "stderr pump ownership was lost after drain timeout".to_owned(),
        ));
    };
    match tokio::time::timeout(STDERR_PUMP_CANCEL_BUDGET, &mut *handle).await {
        Ok(joined) => {
            transport.stderr_pump.take();
            match map_pump_join(joined) {
                Ok(()) => Err(TransportError::Timeout(STDERR_PUMP_DRAIN_BUDGET)),
                Err(error) => Err(error),
            }
        }
        Err(_) => {
            // Keep the handle in the struct while awaiting the abort, too. A
            // second cancellation therefore still reaches Drop's backstop.
            handle.abort();
            let abort_result = handle.await;
            transport.stderr_pump.take();

            match abort_result {
                Err(join_error) if join_error.is_cancelled() => Err(TransportError::Timeout(
                    STDERR_PUMP_DRAIN_BUDGET + STDERR_PUMP_CANCEL_BUDGET,
                )),
                Err(join_error) => Err(TransportError::Other(format!(
                    "stderr pump abort failed: {join_error}"
                ))),
                Ok(Err(io_error)) => Err(TransportError::Io(io_error)),
                Ok(Ok(())) => Err(TransportError::Timeout(
                    STDERR_PUMP_DRAIN_BUDGET + STDERR_PUMP_CANCEL_BUDGET,
                )),
            }
        }
    }
}

/// Flatten the task join and its explicit capture/read I/O result. This is the
/// fail-closed boundary: a pump that exits normally but reports I/O failure is
/// still an unsuccessful transport shutdown.
fn map_pump_join(
    joined: Result<io::Result<()>, tokio::task::JoinError>,
) -> Result<(), TransportError> {
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(io_error)) => Err(TransportError::Io(io_error)),
        Err(join_error) => Err(TransportError::Other(format!(
            "stderr pump task failed: {join_error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_forced_wait_notification_accepts_proven_process_exit() {
        resolve_forced_wait_timeout(Ok(true), Ok(()))
            .expect("a direct OS exit observation must override a delayed async callback");
    }

    #[test]
    fn forced_wait_timeout_remains_fail_closed_for_live_process() {
        let error = resolve_forced_wait_timeout(Ok(false), Ok(()))
            .expect_err("a process still live after the final probe must fail");
        assert!(matches!(
            error,
            TransportError::Timeout(FORCED_CHILD_REAP_BUDGET)
        ));
    }

    #[test]
    fn forced_wait_final_probe_error_is_not_a_false_success() {
        let error = resolve_forced_wait_timeout(
            Err(io::Error::other("injected process-table failure")),
            Ok(()),
        )
        .expect_err("an uncertain final process state must fail");
        assert!(matches!(error, TransportError::Io(ref io_error)
                if io_error.to_string() == "injected process-table failure"));
    }

    #[test]
    fn forced_wait_live_process_preserves_kill_error() {
        let error = resolve_forced_wait_timeout(
            Ok(false),
            Err(io::Error::other("injected termination failure")),
        )
        .expect_err("a live process plus failed termination must fail");
        assert!(matches!(error, TransportError::Io(ref io_error)
                if io_error.to_string() == "injected termination failure"));
    }

    #[test]
    fn pump_io_error_makes_shutdown_mapping_fail_closed() {
        let joined: Result<io::Result<()>, tokio::task::JoinError> =
            Ok(Err(io::Error::other("injected capture failure")));

        match map_pump_join(joined) {
            Err(TransportError::Io(error)) => {
                assert_eq!(error.to_string(), "injected capture failure");
            }
            other => panic!("pump I/O failure must map to TransportError::Io, got {other:?}"),
        }
    }
}
