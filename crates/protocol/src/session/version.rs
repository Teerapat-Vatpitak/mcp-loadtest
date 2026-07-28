//! Protocol-version negotiation surface of [`Session`] (ADR 0018).
//!
//! Split out of `session.rs` to keep that file within the size convention.
//! Policy: the client advertises one supported revision
//! ([`ProtocolVersion::DEFAULT_ADVERTISED`] unless the config pins another);
//! whatever the server answers is accepted when it parses to a supported
//! revision, warned about otherwise. Gating on an unknown revision is the
//! run orchestrator's job (strict mode only) — `Session` itself stays
//! permissive so direct library users are never gated on it.

use std::ffi::OsStr;
use std::time::Duration;

use super::lifecycle::cleanup_failed_startup;
use super::{Session, SessionError};
use crate::mcp::ProtocolVersion;
use crate::transport::Transport;
use crate::transport::spawn_options::SpawnOptions;
use crate::transport::stdio::StdioTransport;

impl Session {
    /// Like [`Session::from_transport_with`], but advertising an explicit
    /// protocol revision in `initialize` instead of
    /// [`ProtocolVersion::DEFAULT_ADVERTISED`]. `Run` resolves the revision
    /// from `[server] protocol_version` and threads it through here.
    pub async fn from_transport_with_version<T>(
        transport: T,
        startup_timeout: Duration,
        advertised: ProtocolVersion,
    ) -> Result<Self, SessionError>
    where
        T: Transport + 'static,
    {
        let mut session = Session {
            transport: Box::new(transport),
            next_id: 1,
            server_protocol_version: String::new(),
            advertised_version: advertised,
            negotiated_version: None,
            stateless: None,
            tool_schemas: None,
            tool_output_schemas: None,
        };
        let startup_result = match tokio::time::timeout(startup_timeout, session.initialize()).await
        {
            Ok(result) => result,
            Err(_) => Err(SessionError::StartupTimeout(startup_timeout)),
        };
        match startup_result {
            Ok(()) => Ok(session),
            Err(error) => Err(cleanup_failed_startup(session, error).await),
        }
    }

    /// Like [`Session::spawn_with_timeout`], but advertising an explicit
    /// protocol revision (see [`Session::from_transport_with_version`]).
    pub async fn spawn_with_timeout_and_version<I, S>(
        command: &str,
        args: I,
        opts: SpawnOptions,
        startup_timeout: Duration,
        advertised: ProtocolVersion,
    ) -> Result<Self, SessionError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let t = StdioTransport::spawn_with(command, args, &opts).await?;
        Self::from_transport_with_version(t, startup_timeout, advertised).await
    }

    /// The revision this session advertised in `initialize`.
    pub fn advertised_version(&self) -> ProtocolVersion {
        self.advertised_version
    }

    /// Typed form of the server's negotiated `protocolVersion`: `Some` when
    /// it parses to a supported revision, `None` when the server answered
    /// with an unknown version (the raw string stays available in
    /// [`Session::server_protocol_version`]).
    pub fn negotiated_version(&self) -> Option<ProtocolVersion> {
        self.negotiated_version
    }

    /// Parse the server's reply into the typed form, warning once (ADR 0018)
    /// when it falls outside the supported set. Called from `initialize`.
    pub(super) fn classify_negotiated_version(&self) -> Option<ProtocolVersion> {
        let parsed = ProtocolVersion::parse(&self.server_protocol_version);
        if parsed.is_none() {
            tracing::warn!(
                advertised = %self.advertised_version,
                got = %self.server_protocol_version,
                "server negotiated an unsupported protocol version (ADR 0018): \
                 continuing permissively; `[validation] strict = true` gates this"
            );
        }
        parsed
    }
}
