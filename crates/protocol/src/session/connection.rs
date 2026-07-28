//! Stateless connection mode — MCP 2026-07-28 (ADR 0019).
//!
//! The stateless core removes the `initialize`/`notifications/initialized`
//! handshake: protocol version, client identity, and client capabilities
//! travel in a `_meta` block on **every request**, and `server/discover`
//! returns server capabilities on demand. This module owns the `_meta`
//! wrapper types and the stateless constructor; `Session`'s public API is
//! unchanged (ADR 0019 decision 2).
//!
//! Field names follow the official final schema at
//! `5f5440bb26a62e2cf3440b92da5a667efa03b267`. The final-only schema change
//! affects the currently unsupported `subscriptions/listen` method, leaving
//! this subset unchanged. Stateless mode remains explicit rather than default.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::lifecycle::cleanup_failed_startup;
use super::{Session, SessionError};
use crate::mcp::{ClientInfo, DiscoverResult, ProtocolVersion};
use crate::transport::Transport;

/// Per-session constants injected into every stateless request's `_meta`.
pub(crate) struct StatelessMeta {
    pub(crate) version: ProtocolVersion,
    pub(crate) client_info: ClientInfo,
    pub(crate) capabilities: Value,
}

/// The `_meta` block itself (reverse-DNS key names).
#[derive(Serialize)]
struct MetaBlock<'a> {
    #[serde(rename = "io.modelcontextprotocol/protocolVersion")]
    protocol_version: &'a str,
    #[serde(rename = "io.modelcontextprotocol/clientInfo")]
    client_info: &'a ClientInfo,
    #[serde(rename = "io.modelcontextprotocol/clientCapabilities")]
    client_capabilities: &'a Value,
}

/// Borrowing wrapper that flattens the caller's params next to `_meta` at
/// serialize time — no intermediate `Value` tree, preserving the ADR 0006
/// zero-copy hot path. Constraint (acceptable): `#[serde(flatten)]` requires
/// map-shaped params, which every MCP method's params are.
#[derive(Serialize)]
pub(crate) struct WithMeta<'a, P: ?Sized + Serialize> {
    #[serde(flatten)]
    params: &'a P,
    #[serde(rename = "_meta")]
    meta: MetaBlock<'a>,
}

impl<'a, P: ?Sized + Serialize> WithMeta<'a, P> {
    pub(crate) fn new(params: &'a P, meta: &'a StatelessMeta) -> Self {
        Self {
            params,
            meta: MetaBlock {
                protocol_version: meta.version.as_str(),
                client_info: &meta.client_info,
                client_capabilities: &meta.capabilities,
            },
        }
    }
}

impl Session {
    /// Construct a **stateless** (2026-07-28) session over any transport
    /// (ADR 0019; wired for stdio + Streamable HTTP). No
    /// `initialize` is sent; instead one bounded `server/discover` probes
    /// connectivity and capabilities. This is a pinned-modern connection,
    /// not automatic era negotiation: a missing `server/discover` or a
    /// response that does not advertise the selected version fails closed.
    pub async fn from_transport_stateless<T>(
        transport: T,
        startup_timeout: Duration,
        version: ProtocolVersion,
    ) -> Result<Self, SessionError>
    where
        T: Transport + 'static,
    {
        let mut session = Session {
            transport: Box::new(transport),
            next_id: 1,
            // Default to what we speak; overwritten if discover reports one.
            server_protocol_version: version.as_str().to_owned(),
            advertised_version: version,
            negotiated_version: None,
            stateless: Some(StatelessMeta {
                version,
                client_info: ClientInfo {
                    name: "mcp-loadtest".to_owned(),
                    version: crate::VERSION.to_owned(),
                },
                capabilities: serde_json::json!({}),
            }),
            tool_schemas: None,
            tool_output_schemas: None,
        };
        // Streamable HTTP requires the version header on *every* POST,
        // including the first server/discover probe. Other transports no-op.
        session.transport.set_protocol_version(version.as_str());
        let startup_result = match tokio::time::timeout(startup_timeout, session.discover()).await {
            Ok(result) => result,
            Err(_) => Err(SessionError::StartupTimeout(startup_timeout)),
        };
        match startup_result {
            Ok(()) => Ok(session),
            Err(error) => Err(cleanup_failed_startup(session, error).await),
        }
    }

    /// The construct-time `server/discover` probe. Eager (not lazy) by
    /// design: it doubles as the connectivity check the handshake used to
    /// provide, and it makes `cold_start`'s factory-spawn measurement cover
    /// spawn/connect → first discover response with **zero** scenario
    /// changes (ADR 0019 decision 4).
    async fn discover(&mut self) -> Result<(), SessionError> {
        let result: Result<DiscoverResult, SessionError> = self
            .request("server/discover", &serde_json::json!({}))
            .await;
        match result {
            Ok(d) => self.accept_discovery(d),
            Err(SessionError::Server(error)) if error.code == -32022 => {
                // Official client conformance exercises a server asking the
                // client to retry a supported modern version. Retry once,
                // but only when the server's structured payload explicitly
                // contains the version we pinned.
                let supported = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("supported"))
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions
                            .iter()
                            .any(|v| v.as_str() == Some(self.advertised_version.as_str()))
                    });
                if !supported {
                    return Err(SessionError::Server(error));
                }
                let retry: DiscoverResult = self
                    .request("server/discover", &serde_json::json!({}))
                    .await?;
                self.accept_discovery(retry)
            }
            Err(error) => Err(error),
        }
    }

    fn accept_discovery(&mut self, discovery: DiscoverResult) -> Result<(), SessionError> {
        let advertised = self.advertised_version;
        if !discovery
            .supported_versions
            .iter()
            .any(|version| version == advertised.as_str())
        {
            return Err(SessionError::UnsupportedProtocolVersion {
                got: discovery.supported_versions.join(","),
                advertised: advertised.to_string(),
            });
        }
        self.server_protocol_version = advertised.to_string();
        self.negotiated_version = Some(advertised);
        Ok(())
    }
}
