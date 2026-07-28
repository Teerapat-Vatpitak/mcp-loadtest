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
const FORCED_CHILD_REAP_BUDGET: Duration = Duration::from_secs(2);
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

    if let Ok(Ok(_status)) = tokio::time::timeout(GRACEFUL_CHILD_EXIT_BUDGET, child.wait()).await {
        return Ok(());
    }
    // A failed asynchronous wait may still be recoverable by terminating and
    // then waiting again, so use the same bounded cleanup path.

    // `start_kill` only requests termination. Always follow it with a bounded
    // wait so successful shutdown means the process actually exited/reaped.
    // A kill error can race a natural exit, so a successful wait still wins.
    let kill_result = child.start_kill();
    match tokio::time::timeout(FORCED_CHILD_REAP_BUDGET, child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(wait_error)) => Err(TransportError::Io(wait_error)),
        Err(_) => match kill_result {
            Ok(()) => Err(TransportError::Timeout(FORCED_CHILD_REAP_BUDGET)),
            Err(kill_error) => Err(TransportError::Io(kill_error)),
        },
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
