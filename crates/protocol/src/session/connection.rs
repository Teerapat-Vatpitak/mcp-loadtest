//! Stateless connection mode — MCP 2026-07-28 (ADR 0019).
//!
//! The stateless core removes the `initialize`/`notifications/initialized`
//! handshake: protocol version, client identity, and client capabilities
//! travel in a `_meta` block on **every request**, and `server/discover`
//! returns server capabilities on demand. This module owns the `_meta`
//! wrapper types and the stateless constructor; `Session`'s public API is
//! unchanged (ADR 0019 decision 2).
//!
//! Field names follow the **release candidate** (`io.modelcontextprotocol/*`
//! keys); they are re-verified against the final spec on 2026-07-29 — see
//! ADR 0019's open questions.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::{Session, SessionError};
use crate::mcp::{ClientInfo, DiscoverResult, ProtocolVersion};
use crate::transport::Transport;

/// Per-session constants injected into every stateless request's `_meta`.
pub(crate) struct StatelessMeta {
    pub(crate) version: ProtocolVersion,
    pub(crate) client_info: ClientInfo,
    pub(crate) capabilities: Value,
}

/// The `_meta` block itself (RC reverse-DNS key names).
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
    /// connectivity and capabilities. A server answering `-32601` (method
    /// not found) is tolerated — the RC positions discover as an optional
    /// up-front call / backward-compatibility probe — but any transport
    /// failure fails construction, mirroring the handshake constructors.
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
        match tokio::time::timeout(startup_timeout, session.discover()).await {
            Ok(result) => result?,
            Err(_) => return Err(SessionError::StartupTimeout(startup_timeout)),
        }
        // Streamable HTTP also carries the revision as the
        // MCP-Protocol-Version header on every request; other transports
        // no-op.
        let negotiated = session.server_protocol_version.clone();
        session.transport.set_protocol_version(&negotiated);
        Ok(session)
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
            Ok(d) => {
                let advertised = self.advertised_version;
                let confirmed = d.protocol_version.as_deref() == Some(advertised.as_str())
                    || d.protocol_versions.iter().any(|v| v == advertised.as_str());
                if let Some(v) = d.protocol_version {
                    self.server_protocol_version = v;
                }
                if confirmed {
                    self.negotiated_version = Some(advertised);
                } else {
                    tracing::warn!(
                        advertised = %advertised,
                        reported = %self.server_protocol_version,
                        "server/discover did not confirm the stateless revision: \
                         continuing permissively (ADR 0019)"
                    );
                }
                Ok(())
            }
            Err(SessionError::Server(e)) if e.code == -32601 => {
                tracing::warn!(
                    "server does not implement server/discover (-32601): continuing \
                     without capability discovery (RC backward-compatibility probe)"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
