//! Session construction and teardown: the [`Session`] constructor family
//! (`from_transport*` / `spawn*`), the private `initialize` handshake, and
//! graceful `shutdown`.
//!
//! Split out of `session/mod.rs` to keep that file within the size
//! convention. All methods here are `impl Session` blocks, so private struct
//! fields stay reachable.

use std::ffi::OsStr;
use std::time::Duration;

use super::{DEFAULT_STARTUP_TIMEOUT, Session, SessionError};
use crate::mcp::{ClientInfo, InitializeParams, InitializeResult, ProtocolVersion};
use crate::transport::spawn_options::SpawnOptions;
use crate::transport::stdio::StdioTransport;
use crate::transport::{Transport, TransportError};

/// Startup failures still own a live transport. Give teardown enough room to
/// complete stdio's composed shutdown budget instead of relying on
/// `kill_on_drop`, which requests termination but cannot prove reap/log drain.
pub(super) const FAILED_STARTUP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Preserve the original startup error when cleanup succeeds. If cleanup is
/// uncertain, surface both facts as one fail-closed transport error.
pub(super) async fn cleanup_failed_startup(
    session: Session,
    startup_error: SessionError,
) -> SessionError {
    match tokio::time::timeout(FAILED_STARTUP_SHUTDOWN_TIMEOUT, session.shutdown()).await {
        Ok(Ok(())) => startup_error,
        Ok(Err(cleanup_error)) => SessionError::Transport(TransportError::Other(format!(
            "session startup failed ({startup_error}); startup teardown failed ({cleanup_error})"
        ))),
        Err(_) => SessionError::Transport(TransportError::Other(format!(
            "session startup failed ({startup_error}); startup teardown exceeded \
             {FAILED_STARTUP_SHUTDOWN_TIMEOUT:?}"
        ))),
    }
}

impl Session {
    /// Construct a session from any [`Transport`]. Performs the `initialize`
    /// + `notifications/initialized` handshake; returns ready-to-use Session.
    ///
    /// Stdio callers can use the [`Session::spawn`] convenience instead.
    pub async fn from_transport<T>(transport: T) -> Result<Self, SessionError>
    where
        T: Transport + 'static,
    {
        Self::from_transport_with(transport, DEFAULT_STARTUP_TIMEOUT).await
    }

    /// Like [`Session::from_transport`], but with a caller-supplied
    /// `initialize` time budget instead of the default `DEFAULT_STARTUP_TIMEOUT`.
    ///
    /// `Run` passes `config.server.startup_timeout` through here so the
    /// configured budget actually governs the handshake (the no-arg
    /// constructors keep the 10s default for direct callers).
    pub async fn from_transport_with<T>(
        transport: T,
        startup_timeout: Duration,
    ) -> Result<Self, SessionError>
    where
        T: Transport + 'static,
    {
        Self::from_transport_with_version(
            transport,
            startup_timeout,
            ProtocolVersion::DEFAULT_ADVERTISED,
        )
        .await
    }

    /// Convenience: spawn `command` with `args` over a [`StdioTransport`] and
    /// run the handshake. Stderr inherits the parent's (the historical
    /// default). Equivalent to
    /// `Session::from_transport(StdioTransport::spawn(...).await?)`.
    ///
    /// The 2-arg signature is unchanged across 0.x — it now delegates to
    /// [`Session::spawn_with`] with [`SpawnOptions::default`]. See ADR 0013.
    pub async fn spawn<I, S>(command: &str, args: I) -> Result<Self, SessionError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::spawn_with(command, args, SpawnOptions::default()).await
    }

    /// Like [`Session::spawn`], but lets the caller control the spawned
    /// server's stderr disposition via [`SpawnOptions`] (capture to a file or
    /// tee to a file + the parent's stderr). Used by `Run` to wire the
    /// `--capture-stderr` / `--tee-stderr` CLI flags.
    pub async fn spawn_with<I, S>(
        command: &str,
        args: I,
        opts: SpawnOptions,
    ) -> Result<Self, SessionError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::spawn_with_timeout(command, args, opts, DEFAULT_STARTUP_TIMEOUT).await
    }

    /// Like [`Session::spawn_with`], but with a caller-supplied `initialize`
    /// time budget. `Run` uses this so a stdio server's handshake honours
    /// `config.server.startup_timeout`. The budget covers only the
    /// `initialize` round-trip, not the process spawn itself.
    pub async fn spawn_with_timeout<I, S>(
        command: &str,
        args: I,
        opts: SpawnOptions,
        startup_timeout: Duration,
    ) -> Result<Self, SessionError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let t = StdioTransport::spawn_with(command, args, &opts).await?;
        Self::from_transport_with(t, startup_timeout).await
    }

    /// Process id of the spawned server, if applicable + still alive.
    /// HTTP / SSE transports return `None`.
    pub fn pid(&self) -> Option<u32> {
        self.transport.pid()
    }

    pub(super) async fn initialize(&mut self) -> Result<(), SessionError> {
        let params = InitializeParams {
            protocol_version: self.advertised_version.as_str().to_owned(),
            capabilities: serde_json::json!({}),
            client_info: ClientInfo {
                name: "mcp-loadtest".to_owned(),
                version: crate::VERSION.to_owned(),
            },
        };
        let result: InitializeResult = self.request("initialize", &params).await?;
        self.server_protocol_version = result.protocol_version;
        self.negotiated_version = self.classify_negotiated_version();
        // Streamable HTTP attaches the negotiated version as the
        // `MCP-Protocol-Version` header on every request from here on
        // (2025-06-18+ requirement); other transports no-op.
        let negotiated = self.server_protocol_version.clone();
        self.transport.set_protocol_version(&negotiated);

        // Notify the server we're ready. No response expected.
        self.notify("notifications/initialized", &serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Close the underlying transport gracefully.
    pub async fn shutdown(self) -> Result<(), SessionError> {
        self.transport.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::*;

    struct FailingStartupTransport {
        shutdown_called: Arc<AtomicBool>,
        pending_request: bool,
    }

    #[async_trait]
    impl Transport for FailingStartupTransport {
        async fn request(&mut self, _body: &str) -> Result<String, TransportError> {
            if self.pending_request {
                std::future::pending().await
            } else {
                Err(TransportError::Closed)
            }
        }

        async fn notify(&mut self, _body: &str) -> Result<(), TransportError> {
            Ok(())
        }

        async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
            self.shutdown_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn initialize_error_explicitly_shuts_down_transport() {
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let result = Session::from_transport_with(
            FailingStartupTransport {
                shutdown_called: Arc::clone(&shutdown_called),
                pending_request: false,
            },
            Duration::from_secs(1),
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("initialize transport failure must surface"),
        };

        assert!(matches!(
            error,
            SessionError::Transport(TransportError::Closed)
        ));
        assert!(
            shutdown_called.load(Ordering::SeqCst),
            "failed constructor must explicitly shut down its live transport"
        );
    }

    #[tokio::test]
    async fn initialize_timeout_explicitly_shuts_down_transport() {
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let budget = Duration::from_millis(10);
        let result = Session::from_transport_with(
            FailingStartupTransport {
                shutdown_called: Arc::clone(&shutdown_called),
                pending_request: true,
            },
            budget,
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("initialize timeout must surface"),
        };

        assert!(matches!(error, SessionError::StartupTimeout(value) if value == budget));
        assert!(
            shutdown_called.load(Ordering::SeqCst),
            "timed-out constructor must explicitly shut down its live transport"
        );
    }
}
