//! Re-export of [`SessionFactory`] (lives in `mcp-loadtest-protocol`),
//! plus the [`RunError`] → [`SessionError`] adapter that must stay beside
//! `RunError`.
//!
//! # Error mapping
//!
//! The underlying spawn path (`run::build_session`) returns [`RunError`]; the
//! factory surfaces [`SessionError`] instead because that is the error type
//! scenario code already buckets via `scenario::classify_error`. The mapping
//! is lossless for `RunError::Session` and `RunError::Io`; the
//! `RunError::Config` case — unreachable in practice, since the config was
//! validated and the run's *initial* spawn already succeeded with the same
//! config — is folded into [`TransportError::Other`].

pub use mcp_loadtest_protocol::factory::SessionFactory;

use mcp_loadtest_protocol::session::SessionError;
use mcp_loadtest_protocol::transport::TransportError;

use crate::run::RunError;

/// Map the spawn path's [`RunError`] onto [`SessionError`] (see module docs
/// for why the factory speaks `SessionError`).
pub(crate) fn run_error_to_session_error(err: RunError) -> SessionError {
    match err {
        RunError::Session(e) => e,
        RunError::Io(e) => SessionError::Io(e),
        // Unreachable in practice (config already validated + initial spawn
        // succeeded); folded into the transport bucket so the enum mapping
        // stays total without inventing a new SessionError variant.
        RunError::Config(msg) => SessionError::Transport(TransportError::Other(format!(
            "config error during respawn: {msg}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_error_session_maps_lossless() {
        let err = run_error_to_session_error(RunError::Session(SessionError::Transport(
            TransportError::Closed,
        )));
        assert!(matches!(
            err,
            SessionError::Transport(TransportError::Closed)
        ));
    }

    #[test]
    fn run_error_io_maps_to_session_io() {
        let io = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed");
        let err = run_error_to_session_error(RunError::Io(io));
        assert!(matches!(err, SessionError::Io(_)));
    }

    #[test]
    fn run_error_config_folds_into_transport_other() {
        let err = run_error_to_session_error(RunError::Config("bad transport".into()));
        match err {
            SessionError::Transport(TransportError::Other(msg)) => {
                assert!(msg.contains("bad transport"), "got: {msg}");
            }
            other => panic!("expected Transport(Other), got {other:?}"),
        }
    }
}
