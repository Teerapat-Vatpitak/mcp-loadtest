//! Transport selection + session construction for [`Run::execute`]:
//! [`build_session`] (config → transport → session) and [`connect_session`]
//! (optional trace-decorator wrap + the right `Session` constructor).
//!
//! Split out of `run/mod.rs` to keep that file within the size convention.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mcp_loadtest_core::config::Config;
use mcp_loadtest_core::trace::TraceError;
use mcp_loadtest_protocol::mcp::ProtocolVersion;
use mcp_loadtest_protocol::session::{Session, SessionError};
use mcp_loadtest_protocol::transport::Transport;
use mcp_loadtest_protocol::transport::guard::HostGuard;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use mcp_loadtest_protocol::transport::sse::SseTransport;
use mcp_loadtest_protocol::transport::stdio::StdioTransport;
use mcp_loadtest_protocol::transport::ws::WsTransport;

use super::RunError;
use crate::trace::{TraceWriter, TracingTransport};

/// Build a [`Session`] over whichever transport `config.server.transport`
/// asks for. Validation has already rejected unknown / under-specified
/// configs by the time we get here, but we still surface a clear
/// [`RunError::Config`] for the catch-all case so the error chain stays
/// explicit instead of `panic!()`-ing.
///
/// `stderr_log` / `tee` only apply to the `stdio` transport (the only one
/// with a child process); HTTP/SSE/WS ignore them. The SSRF [`HostGuard`] is
/// built once from `config.server` and passed into every URL-based connect so
/// the allowlist + IP-literal block (ADR 0012) runs before any socket I/O.
///
/// `trace` (ADR 0021): when set, every concrete transport is wrapped in a
/// [`TracingTransport`] before the `Session` is constructed, so the trace
/// covers the handshake and all scenario traffic — see [`connect_session`].
pub(super) async fn build_session(
    config: &Config,
    stderr_log: Option<&Path>,
    tee: bool,
    trace: Option<Arc<TraceWriter>>,
) -> Result<Session, RunError> {
    let guard = HostGuard::from_config(&config.server);
    let startup_timeout = config.server.startup_timeout;
    let advertised = config.server.resolved_protocol_version();

    // Stateless 2026-07-28 mode (ADR 0019): no handshake; validation has
    // already restricted it to stdio/http. Strict-mode version gating does
    // not apply — with no negotiation there is nothing to gate beyond the
    // config validation that selected this mode (ADR 0019 decision 6).
    if advertised.is_stateless() {
        return match config.server.transport.as_str() {
            "stdio" => {
                let command =
                    config.server.command.as_deref().ok_or_else(|| {
                        RunError::Config("stdio transport requires command".into())
                    })?;
                let opts = match stderr_log {
                    None => SpawnOptions::inherit(),
                    Some(path) if tee => SpawnOptions::tee_stderr(path),
                    Some(path) => SpawnOptions::capture_stderr(path),
                };
                let transport =
                    StdioTransport::spawn_with(command, config.server.args.iter(), &opts)
                        .await
                        .map_err(SessionError::from)?;
                Ok(connect_session(transport, trace, startup_timeout, advertised, true).await?)
            }
            "http" => {
                let url = config
                    .server
                    .url
                    .as_deref()
                    .ok_or_else(|| RunError::Config("http transport requires url".into()))?;
                let transport = HttpTransport::connect(url, &guard)
                    .await
                    .map_err(SessionError::Transport)?;
                Ok(connect_session(transport, trace, startup_timeout, advertised, true).await?)
            }
            other => Err(RunError::Config(format!(
                "protocol_version `{}` (stateless) is not supported on the `{other}` transport (ADR 0019)",
                advertised
            ))),
        };
    }

    let session = match config.server.transport.as_str() {
        "stdio" => {
            let command = config
                .server
                .command
                .as_deref()
                .ok_or_else(|| RunError::Config("stdio transport requires command".into()))?;
            let opts = match stderr_log {
                None => SpawnOptions::inherit(),
                Some(path) if tee => SpawnOptions::tee_stderr(path),
                Some(path) => SpawnOptions::capture_stderr(path),
            };
            let transport = StdioTransport::spawn_with(command, config.server.args.iter(), &opts)
                .await
                .map_err(SessionError::from)?;
            connect_session(transport, trace, startup_timeout, advertised, false).await?
        }
        "http" => {
            let url = config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("http transport requires url".into()))?;
            let transport = HttpTransport::connect(url, &guard)
                .await
                .map_err(SessionError::Transport)?;
            connect_session(transport, trace, startup_timeout, advertised, false).await?
        }
        "sse" => {
            let url = config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("sse transport requires url".into()))?;
            let transport = SseTransport::connect(url, &guard)
                .await
                .map_err(SessionError::Transport)?;
            connect_session(transport, trace, startup_timeout, advertised, false).await?
        }
        "ws" => {
            let url = config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("ws transport requires url".into()))?;
            let transport = WsTransport::connect(url, &guard)
                .await
                .map_err(SessionError::Transport)?;
            connect_session(transport, trace, startup_timeout, advertised, false).await?
        }
        other => {
            return Err(RunError::Config(format!(
                "transport `{other}` is not yet supported by Run (parser accepts it for forward-compat)",
            )));
        }
    };

    // ADR 0018: a server negotiating a revision outside the supported set
    // warns by default (inside `Session::initialize`) and gates only under
    // strict validation. The check lives here — not in `Session` — so both
    // the run's initial spawn and every `SessionFactory` respawn (pools,
    // cold_start) enforce the same policy.
    if config.validation.strict && session.negotiated_version().is_none() {
        return Err(RunError::Session(
            SessionError::UnsupportedProtocolVersion {
                got: session.server_protocol_version.clone(),
                advertised: advertised.to_string(),
            },
        ));
    }
    Ok(session)
}

/// Hand a freshly-connected transport to the right [`Session`] constructor,
/// first wrapping it in a [`TracingTransport`] when the run records a trace
/// (ADR 0021). A generic function rather than a closure because the wrap
/// changes the concrete transport type per `build_session` arm.
async fn connect_session<T>(
    transport: T,
    trace: Option<Arc<TraceWriter>>,
    startup_timeout: Duration,
    advertised: ProtocolVersion,
    stateless: bool,
) -> Result<Session, SessionError>
where
    T: Transport + 'static,
{
    match (trace, stateless) {
        (Some(writer), false) => {
            Session::from_transport_with_version(
                TracingTransport::new(transport, writer),
                startup_timeout,
                advertised,
            )
            .await
        }
        (None, false) => {
            Session::from_transport_with_version(transport, startup_timeout, advertised).await
        }
        (Some(writer), true) => {
            Session::from_transport_stateless(
                TracingTransport::new(transport, writer),
                startup_timeout,
                advertised,
            )
            .await
        }
        (None, true) => {
            Session::from_transport_stateless(transport, startup_timeout, advertised).await
        }
    }
}

/// Map a trace-recording failure onto [`RunError`] without growing the enum:
/// I/O failures are [`RunError::Io`]; anything else (header serialization —
/// unreachable in practice) folds into [`RunError::Config`]. A dedicated
/// `RunError::Trace` variant would force `run::factory`'s exhaustive error
/// mapping to change, which this feature deliberately leaves untouched.
pub(super) fn trace_to_run_error(err: TraceError) -> RunError {
    match err {
        TraceError::Io(e) => RunError::Io(e),
        other => RunError::Config(format!("trace: {other}")),
    }
}
