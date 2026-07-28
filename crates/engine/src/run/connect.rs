//! Transport selection + session construction for [`Run::execute`]:
//! [`build_session`] (config → transport → session) and [`connect_session`]
//! (optional trace-decorator wrap + the right `Session` constructor).
//!
//! Split out of `run/mod.rs` to keep that file within the size convention.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mcp_loadtest_core::config::Config;
use mcp_loadtest_core::trace::TraceError;
use mcp_loadtest_protocol::mcp::ProtocolVersion;
use mcp_loadtest_protocol::session::{Session, SessionError};
use mcp_loadtest_protocol::transport::guard::HostGuard;
use mcp_loadtest_protocol::transport::headers::RemoteHeaders;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use mcp_loadtest_protocol::transport::sse::SseTransport;
use mcp_loadtest_protocol::transport::stdio::StdioTransport;
use mcp_loadtest_protocol::transport::ws::WsTransport;
use mcp_loadtest_protocol::transport::{Transport, TransportError};
use tokio::time::Instant as TokioInstant;

use super::RunError;
use crate::scenario::{ScenarioOutcome, teardown};
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
    startup_deadline: TokioInstant,
) -> Result<Session, RunError> {
    let guard = HostGuard::from_config(&config.server);
    let startup_timeout = config.server.startup_timeout;
    let advertised = config.server.resolved_protocol_version();
    let remote_headers = RemoteHeaders::from_env(&config.server.headers_from_env)
        .map_err(SessionError::Transport)?;

    // Stateless 2026-07-28 mode (ADR 0019): no handshake; validation has
    // already restricted it to stdio/http. Strict-mode version gating does
    // not apply — with no negotiation there is nothing to gate beyond the
    // config validation that selected this mode (ADR 0019 decision 6).
    if advertised.is_stateless() {
        let session = match config.server.transport.as_str() {
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
                let transport = startup_connect(startup_deadline, startup_timeout, async {
                    StdioTransport::spawn_with(command, config.server.args.iter(), &opts)
                        .await
                        .map_err(SessionError::from)
                })
                .await?;
                connect_session_before_deadline(
                    transport,
                    trace,
                    startup_deadline,
                    startup_timeout,
                    advertised,
                    true,
                )
                .await?
            }
            "http" => {
                let url = config
                    .server
                    .url
                    .as_deref()
                    .ok_or_else(|| RunError::Config("http transport requires url".into()))?;
                let transport = startup_connect(startup_deadline, startup_timeout, async {
                    HttpTransport::connect_with_headers(url, &guard, remote_headers.clone())
                        .await
                        .map_err(SessionError::Transport)
                })
                .await?;
                connect_session_before_deadline(
                    transport,
                    trace,
                    startup_deadline,
                    startup_timeout,
                    advertised,
                    true,
                )
                .await?
            }
            other => {
                return Err(RunError::Config(format!(
                    "protocol_version `{}` (stateless) is not supported on the `{other}` transport (ADR 0019)",
                    advertised
                )));
            }
        };
        return finish_before_startup_deadline(
            session,
            startup_deadline,
            startup_timeout,
            "stateless startup deadline cleanup",
        )
        .await;
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
            let transport = startup_connect(startup_deadline, startup_timeout, async {
                StdioTransport::spawn_with(command, config.server.args.iter(), &opts)
                    .await
                    .map_err(SessionError::from)
            })
            .await?;
            connect_session_before_deadline(
                transport,
                trace,
                startup_deadline,
                startup_timeout,
                advertised,
                false,
            )
            .await?
        }
        "http" => {
            let url = config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("http transport requires url".into()))?;
            let transport = startup_connect(startup_deadline, startup_timeout, async {
                HttpTransport::connect_with_headers(url, &guard, remote_headers.clone())
                    .await
                    .map_err(SessionError::Transport)
            })
            .await?;
            connect_session_before_deadline(
                transport,
                trace,
                startup_deadline,
                startup_timeout,
                advertised,
                false,
            )
            .await?
        }
        "sse" => {
            let url = config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("sse transport requires url".into()))?;
            let transport = startup_connect(startup_deadline, startup_timeout, async {
                SseTransport::connect_with_headers(url, &guard, remote_headers.clone())
                    .await
                    .map_err(SessionError::Transport)
            })
            .await?;
            connect_session_before_deadline(
                transport,
                trace,
                startup_deadline,
                startup_timeout,
                advertised,
                false,
            )
            .await?
        }
        "ws" => {
            let url = config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("ws transport requires url".into()))?;
            let transport = startup_connect(startup_deadline, startup_timeout, async {
                WsTransport::connect_with_headers(url, &guard, remote_headers)
                    .await
                    .map_err(SessionError::Transport)
            })
            .await?;
            connect_session_before_deadline(
                transport,
                trace,
                startup_deadline,
                startup_timeout,
                advertised,
                false,
            )
            .await?
        }
        other => {
            return Err(RunError::Config(format!(
                "transport `{other}` is not yet supported by Run (parser accepts it for forward-compat)",
            )));
        }
    };

    let session = finish_before_startup_deadline(
        session,
        startup_deadline,
        startup_timeout,
        "session startup deadline cleanup",
    )
    .await?;

    // ADR 0018: a server negotiating a revision outside the supported set
    // warns by default (inside `Session::initialize`) and gates only under
    // strict validation. The check lives here — not in `Session` — so both
    // the run's initial spawn and every `SessionFactory` respawn (pools,
    // cold_start) enforce the same policy.
    if config.validation.strict && session.negotiated_version().is_none() {
        let error = SessionError::UnsupportedProtocolVersion {
            got: session.server_protocol_version.clone(),
            advertised: advertised.to_string(),
        };
        return Err(RunError::Session(
            shutdown_after_session_error(session, error, "strict protocol-version gate").await,
        ));
    }
    Ok(session)
}

fn remaining(deadline: TokioInstant) -> Duration {
    deadline.saturating_duration_since(TokioInstant::now())
}

async fn startup_connect<T, F>(
    deadline: TokioInstant,
    configured_budget: Duration,
    future: F,
) -> Result<T, RunError>
where
    F: Future<Output = Result<T, SessionError>>,
{
    match tokio::time::timeout_at(deadline, future).await {
        Ok(result) => result.map_err(RunError::Session),
        Err(_) => Err(RunError::Session(SessionError::StartupTimeout(
            configured_budget,
        ))),
    }
}

async fn finish_before_startup_deadline(
    session: Session,
    deadline: TokioInstant,
    configured_budget: Duration,
    context: &str,
) -> Result<Session, RunError> {
    if TokioInstant::now() < deadline {
        return Ok(session);
    }
    Err(RunError::Session(
        shutdown_after_session_error(
            session,
            SessionError::StartupTimeout(configured_budget),
            context,
        )
        .await,
    ))
}

async fn connect_session_before_deadline<T>(
    transport: T,
    trace: Option<Arc<TraceWriter>>,
    deadline: TokioInstant,
    configured_budget: Duration,
    advertised: ProtocolVersion,
    stateless: bool,
) -> Result<Session, RunError>
where
    T: Transport + 'static,
{
    connect_session(transport, trace, remaining(deadline), advertised, stateless)
        .await
        .map_err(|error| match error {
            SessionError::StartupTimeout(_) => {
                RunError::Session(SessionError::StartupTimeout(configured_budget))
            }
            other => RunError::Session(other),
        })
}

/// A session rejected after construction is still live. Explicitly shut it
/// down and preserve the primary error; if teardown is uncertain, combine the
/// two so callers cannot mistake an incomplete lifecycle for a clean failure.
pub(super) async fn shutdown_after_session_error(
    session: Session,
    error: SessionError,
    context: &str,
) -> SessionError {
    let mut cleanup = ScenarioOutcome::default();
    teardown::shutdown_session(session, &mut cleanup, context).await;
    if cleanup.teardown_failure_count == 0 {
        error
    } else {
        SessionError::Transport(TransportError::Other(format!(
            "{error}; {}",
            cleanup.notes.join("; ")
        )))
    }
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
