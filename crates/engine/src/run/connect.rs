//! Transport selection + session construction for [`Run::execute`]:
//! [`build_session`] (config → transport → session) and [`connect_session`]
//! (optional trace-decorator wrap + the right `Session` constructor).
//!
//! Split out of `run/mod.rs` to keep that file within the size convention.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mcp_loadtest_auth::{
    BearerChallenge, ClientRegistration, ClientSecret, DiscoveryClient, DynamicClientMetadata,
    EndpointPolicy, LoopbackCallback, OAuthProvider, PreRegisteredClient, ScopeSet, StepUpTracker,
    TokenEndpointAuthMethod as AuthTokenEndpointAuthMethod,
};
use mcp_loadtest_core::config::{
    AuthConfig, Config, OAuthFlow, OAuthRegistration, TokenEndpointAuthMethod,
};
use mcp_loadtest_core::trace::TraceError;
use mcp_loadtest_protocol::mcp::ProtocolVersion;
use mcp_loadtest_protocol::session::{Session, SessionError};
use mcp_loadtest_protocol::transport::guard::HostGuard;
use mcp_loadtest_protocol::transport::headers::RemoteHeaders;
use mcp_loadtest_protocol::transport::http::{HttpTransport, OAuthChallengeHandler};
use mcp_loadtest_protocol::transport::spawn_options::SpawnOptions;
use mcp_loadtest_protocol::transport::sse::SseTransport;
use mcp_loadtest_protocol::transport::stdio::StdioTransport;
use mcp_loadtest_protocol::transport::ws::WsTransport;
use mcp_loadtest_protocol::transport::{Transport, TransportError};
use tokio::time::Instant as TokioInstant;
use url::Url;

use super::RunError;
use crate::scenario::{ScenarioOutcome, teardown};
use crate::trace::{TraceWriter, TracingTransport};

/// Prepared, run-scoped OAuth state shared by the initial session and every
/// pooled/factory session. Tokens remain in the provider's in-memory store.
#[derive(Clone)]
pub(super) struct OAuthRuntime {
    provider: Arc<OAuthProvider>,
    context: mcp_loadtest_auth::AuthorizationContext,
    challenge_handler: Arc<EngineOAuthChallengeHandler>,
}

struct EngineOAuthChallengeHandler {
    policy: EndpointPolicy,
    resource: Url,
    auth: AuthConfig,
    scopes: tokio::sync::Mutex<ScopeSet>,
    step_up: tokio::sync::Mutex<StepUpTracker>,
}

#[async_trait::async_trait]
impl OAuthChallengeHandler for EngineOAuthChallengeHandler {
    async fn reauthorize(
        &self,
        challenge: BearerChallenge,
    ) -> Result<(Arc<OAuthProvider>, mcp_loadtest_auth::AuthorizationContext), TransportError> {
        let discovery = DiscoveryClient::new(self.policy.clone())
            .map_err(|error| oauth_transport_error("discovery setup", error))?;
        let context = discovery
            .discover(self.resource.clone(), Some(&challenge))
            .await
            .map_err(|error| oauth_transport_error("challenge discovery", error))?;
        let challenged = context.initial_scopes(Some(&challenge), self.auth.offline_access);
        let prior = self.scopes.lock().await.clone();
        let scopes = {
            let mut step_up = self.step_up.lock().await;
            step_up
                .next(&prior, &challenged)
                .map_err(|error| oauth_transport_error("scope step-up", error))?
        };
        let callback = if self.auth.flow == OAuthFlow::AuthorizationCode {
            Some(
                LoopbackCallback::bind()
                    .await
                    .map_err(|error| oauth_transport_error("callback setup", error))?,
            )
        } else {
            None
        };
        let registration =
            build_stepup_registration(&self.auth, &self.policy, &context, callback.as_ref())
                .await
                .map_err(|error| {
                    TransportError::Other(format!("OAuth reauthorization failed: {error}"))
                })?;
        let provider = Arc::new(
            OAuthProvider::new(self.policy.clone(), registration)
                .map_err(|error| oauth_transport_error("provider setup", error))?,
        );
        acquire_stepup_grant(&self.auth, &provider, &context, scopes.clone(), callback)
            .await
            .map_err(|error| {
                TransportError::Other(format!("OAuth reauthorization failed: {error}"))
            })?;
        *self.scopes.lock().await = scopes;
        Ok((provider, context))
    }
}

fn oauth_transport_error(operation: &'static str, error: impl std::fmt::Display) -> TransportError {
    TransportError::Other(format!("OAuth {operation} failed: {error}"))
}

async fn build_stepup_registration(
    auth: &AuthConfig,
    policy: &EndpointPolicy,
    context: &mcp_loadtest_auth::AuthorizationContext,
    callback: Option<&LoopbackCallback>,
) -> Result<ClientRegistration, RunError> {
    match &auth.registration {
        OAuthRegistration::PreRegistered {
            client_id,
            client_secret_env,
            token_endpoint_auth_method,
        } => {
            let mut client = PreRegisteredClient::new(client_id.clone())
                .map_err(|error| RunError::Config(format!("OAuth client failed: {error}")))?;
            if let Some(name) = client_secret_env {
                client = client.with_client_secret(
                    ClientSecret::from_environment(name.clone()).map_err(|error| {
                        RunError::Config(format!("OAuth credential reference failed: {error}"))
                    })?,
                );
            }
            Ok(ClientRegistration::pre_registered(
                client.with_token_endpoint_auth_method(map_token_method(
                    *token_endpoint_auth_method,
                )?),
            ))
        }
        OAuthRegistration::ClientIdMetadata {
            client_id_metadata_url,
        } => ClientRegistration::client_id_metadata(
            Url::parse(client_id_metadata_url)
                .map_err(|_| RunError::Config("OAuth client metadata URL is invalid".into()))?,
            policy,
        )
        .map_err(|error| RunError::Config(format!("OAuth client metadata failed: {error}"))),
        OAuthRegistration::Dynamic { client_name } => {
            let callback = callback.ok_or_else(|| {
                RunError::Config("dynamic OAuth registration requires a callback".into())
            })?;
            let metadata = DynamicClientMetadata::authorization_code(
                client_name
                    .clone()
                    .unwrap_or_else(|| "mcp-loadtest".to_owned()),
                callback.redirect_uri(),
            )
            .map_err(|error| RunError::Config(format!("OAuth dynamic metadata failed: {error}")))?;
            OAuthProvider::dynamic_register(
                policy.clone(),
                &context.authorization_server,
                &metadata,
            )
            .await
            .map_err(|error| {
                RunError::Config(format!("OAuth dynamic registration failed: {error}"))
            })
        }
        _ => Err(RunError::Config(
            "unsupported OAuth registration strategy".into(),
        )),
    }
}

async fn acquire_stepup_grant(
    auth: &AuthConfig,
    provider: &Arc<OAuthProvider>,
    context: &mcp_loadtest_auth::AuthorizationContext,
    scopes: ScopeSet,
    callback: Option<LoopbackCallback>,
) -> Result<(), RunError> {
    match auth.flow {
        OAuthFlow::ClientCredentials => provider
            .client_credentials(context, scopes)
            .await
            .map(|_| ())
            .map_err(|error| RunError::Config(format!("OAuth client credentials failed: {error}"))),
        OAuthFlow::AuthorizationCode => {
            let callback = callback.ok_or_else(|| {
                RunError::Config("authorization-code OAuth requires a callback".into())
            })?;
            let pending = provider
                .begin_authorization(context, callback.redirect_uri().clone(), scopes)
                .map_err(|error| {
                    RunError::Config(format!("OAuth authorization setup failed: {error}"))
                })?;
            eprintln!(
                "OAuth authorization required. Open this URL in a browser:\n{}",
                pending.authorization_url()
            );
            let callback_url =
                callback
                    .wait(Duration::from_secs(5 * 60))
                    .await
                    .map_err(|error| {
                        RunError::Config(format!("OAuth callback failed or timed out: {error}"))
                    })?;
            provider
                .complete_authorization(context, pending, &callback_url)
                .await
                .map(|_| ())
                .map_err(|error| RunError::Config(format!("OAuth token exchange failed: {error}")))
        }
        _ => Err(RunError::Config("unsupported OAuth flow".into())),
    }
}

/// Complete configured OAuth discovery and grant acquisition once per run.
pub(super) async fn prepare_oauth(
    config: &Config,
    startup_deadline: TokioInstant,
) -> Result<Option<OAuthRuntime>, RunError> {
    let Some(auth) = &config.server.auth else {
        return Ok(None);
    };
    let resource = Url::parse(
        config
            .server
            .url
            .as_deref()
            .ok_or_else(|| RunError::Config("OAuth requires server.url".into()))?,
    )
    .map_err(|_| RunError::Config("OAuth server URL is invalid".into()))?;
    let guard = HostGuard::from_config(&config.server);
    let headers = RemoteHeaders::from_env(&config.server.headers_from_env)
        .map_err(SessionError::Transport)?;
    let policy = EndpointPolicy::strict().with_timeout(remaining(startup_deadline));
    let challenge = startup_connect(startup_deadline, config.server.startup_timeout, async {
        HttpTransport::discover_oauth_challenge(&resource, &guard, headers)
            .await
            .map_err(SessionError::Transport)
    })
    .await?;
    let discovery = DiscoveryClient::new(policy.clone())
        .map_err(|error| RunError::Config(format!("OAuth discovery setup failed: {error}")))?;
    let context = startup_connect(startup_deadline, config.server.startup_timeout, async {
        discovery
            .discover(resource, challenge.as_ref())
            .await
            .map_err(|error| {
                SessionError::Transport(TransportError::Other(format!(
                    "OAuth discovery failed: {error}"
                )))
            })
    })
    .await?;

    let callback =
        if auth.flow == OAuthFlow::AuthorizationCode {
            Some(LoopbackCallback::bind().await.map_err(|error| {
                RunError::Config(format!("OAuth callback setup failed: {error}"))
            })?)
        } else {
            None
        };
    let registration = match &auth.registration {
        OAuthRegistration::PreRegistered {
            client_id,
            client_secret_env,
            token_endpoint_auth_method,
        } => {
            let mut client = PreRegisteredClient::new(client_id.clone()).map_err(|error| {
                RunError::Config(format!("OAuth client configuration failed: {error}"))
            })?;
            if let Some(name) = client_secret_env {
                client = client.with_client_secret(
                    ClientSecret::from_environment(name.clone()).map_err(|error| {
                        RunError::Config(format!("OAuth credential reference failed: {error}"))
                    })?,
                );
            }
            client = client
                .with_token_endpoint_auth_method(map_token_method(*token_endpoint_auth_method)?);
            ClientRegistration::pre_registered(client)
        }
        OAuthRegistration::ClientIdMetadata {
            client_id_metadata_url,
        } => ClientRegistration::client_id_metadata(
            Url::parse(client_id_metadata_url)
                .map_err(|_| RunError::Config("OAuth client metadata URL is invalid".into()))?,
            &policy,
        )
        .map_err(|error| RunError::Config(format!("OAuth client metadata failed: {error}")))?,
        OAuthRegistration::Dynamic { client_name } => {
            let callback_ref = callback.as_ref().ok_or_else(|| {
                RunError::Config("dynamic OAuth registration requires authorization_code".into())
            })?;
            let metadata = DynamicClientMetadata::authorization_code(
                client_name
                    .clone()
                    .unwrap_or_else(|| "mcp-loadtest".to_owned()),
                callback_ref.redirect_uri(),
            )
            .map_err(|error| {
                RunError::Config(format!("OAuth dynamic client metadata failed: {error}"))
            })?;
            startup_connect(startup_deadline, config.server.startup_timeout, async {
                OAuthProvider::dynamic_register(
                    policy.clone(),
                    &context.authorization_server,
                    &metadata,
                )
                .await
                .map_err(|error| {
                    SessionError::Transport(TransportError::Other(format!(
                        "OAuth dynamic registration failed: {error}"
                    )))
                })
            })
            .await?
        }
        _ => {
            return Err(RunError::Config(
                "unsupported OAuth registration strategy".into(),
            ));
        }
    };
    let provider = Arc::new(
        OAuthProvider::new(policy, registration)
            .map_err(|error| RunError::Config(format!("OAuth provider setup failed: {error}")))?,
    );
    let mut scopes = if auth.scopes.is_empty() {
        context.initial_scopes(challenge.as_ref(), auth.offline_access)
    } else {
        ScopeSet::from_tokens(auth.scopes.clone())
    };
    if auth.offline_access
        && context
            .authorization_server
            .supports_scope("offline_access")
    {
        scopes.insert("offline_access");
    }
    match auth.flow {
        OAuthFlow::ClientCredentials => {
            startup_connect(startup_deadline, config.server.startup_timeout, async {
                provider
                    .client_credentials(&context, scopes.clone())
                    .await
                    .map(|_| ())
                    .map_err(|error| {
                        SessionError::Transport(TransportError::Other(format!(
                            "OAuth client credentials failed: {error}"
                        )))
                    })
            })
            .await?;
        }
        OAuthFlow::AuthorizationCode => {
            let callback = callback.expect("authorization-code callback was created");
            let pending = provider
                .begin_authorization(&context, callback.redirect_uri().clone(), scopes.clone())
                .map_err(|error| {
                    RunError::Config(format!("OAuth authorization setup failed: {error}"))
                })?;
            eprintln!(
                "OAuth authorization required. Open this URL in a browser:\n{}",
                pending.authorization_url()
            );
            let callback_url =
                callback
                    .wait(remaining(startup_deadline))
                    .await
                    .map_err(|error| {
                        RunError::Config(format!("OAuth callback failed or timed out: {error}"))
                    })?;
            provider
                .complete_authorization(&context, pending, &callback_url)
                .await
                .map_err(|error| {
                    RunError::Config(format!("OAuth token exchange failed: {error}"))
                })?;
        }
        _ => return Err(RunError::Config("unsupported OAuth flow".into())),
    }
    let challenge_handler = Arc::new(EngineOAuthChallengeHandler {
        policy: EndpointPolicy::strict(),
        resource: Url::parse(
            config
                .server
                .url
                .as_deref()
                .ok_or_else(|| RunError::Config("OAuth requires server.url".into()))?,
        )
        .map_err(|_| RunError::Config("OAuth server URL is invalid".into()))?,
        auth: auth.clone(),
        scopes: tokio::sync::Mutex::new(scopes),
        step_up: tokio::sync::Mutex::new(StepUpTracker::new(usize::from(auth.max_step_up_retries))),
    });
    Ok(Some(OAuthRuntime {
        provider,
        context,
        challenge_handler,
    }))
}

fn map_token_method(
    method: TokenEndpointAuthMethod,
) -> Result<AuthTokenEndpointAuthMethod, RunError> {
    Ok(match method {
        TokenEndpointAuthMethod::Auto => AuthTokenEndpointAuthMethod::Auto,
        TokenEndpointAuthMethod::None => AuthTokenEndpointAuthMethod::None,
        TokenEndpointAuthMethod::ClientSecretBasic => {
            AuthTokenEndpointAuthMethod::ClientSecretBasic
        }
        TokenEndpointAuthMethod::ClientSecretPost => AuthTokenEndpointAuthMethod::ClientSecretPost,
        _ => {
            return Err(RunError::Config(
                "unsupported OAuth token authentication method".into(),
            ));
        }
    })
}

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
    oauth: Option<OAuthRuntime>,
) -> Result<Session, RunError> {
    let guard = HostGuard::from_config(&config.server);
    let startup_timeout = config.server.startup_timeout;
    let advertised = config.server.resolved_protocol_version();
    let auto_negotiate = matches!(
        config.server.protocol_version.as_deref(),
        None | Some("auto")
    );
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
                    ConnectionMode::Stateless,
                )
                .await?
            }
            "http" => {
                let url = config
                    .server
                    .url
                    .as_deref()
                    .ok_or_else(|| RunError::Config("http transport requires url".into()))?;
                let oauth = oauth.clone();
                let transport = startup_connect(startup_deadline, startup_timeout, async {
                    match oauth {
                        Some(runtime) => {
                            HttpTransport::connect_with_oauth_handler(
                                url,
                                &guard,
                                remote_headers.clone(),
                                runtime.provider,
                                runtime.context,
                                runtime.challenge_handler,
                            )
                            .await
                        }
                        None => {
                            HttpTransport::connect_with_headers(url, &guard, remote_headers.clone())
                                .await
                        }
                    }
                    .map_err(SessionError::Transport)
                })
                .await?;
                connect_session_before_deadline(
                    transport,
                    trace,
                    startup_deadline,
                    startup_timeout,
                    advertised,
                    ConnectionMode::Stateless,
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
                if auto_negotiate {
                    ConnectionMode::Auto
                } else {
                    ConnectionMode::Legacy
                },
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
                match oauth {
                    Some(runtime) => {
                        HttpTransport::connect_with_oauth_handler(
                            url,
                            &guard,
                            remote_headers.clone(),
                            runtime.provider,
                            runtime.context,
                            runtime.challenge_handler,
                        )
                        .await
                    }
                    None => {
                        HttpTransport::connect_with_headers(url, &guard, remote_headers.clone())
                            .await
                    }
                }
                .map_err(SessionError::Transport)
            })
            .await?;
            connect_session_before_deadline(
                transport,
                trace,
                startup_deadline,
                startup_timeout,
                advertised,
                if auto_negotiate {
                    ConnectionMode::Auto
                } else {
                    ConnectionMode::Legacy
                },
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
                ConnectionMode::Legacy,
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
                ConnectionMode::Legacy,
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
    mode: ConnectionMode,
) -> Result<Session, RunError>
where
    T: Transport + 'static,
{
    connect_session(transport, trace, remaining(deadline), advertised, mode)
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
#[derive(Clone, Copy)]
enum ConnectionMode {
    /// Probe 2026-07-28 and fall back to 2025-11-25 for a legacy server.
    Auto,
    /// Use the explicitly selected legacy initialization handshake.
    Legacy,
    /// Use the explicitly selected final stateless protocol.
    Stateless,
}

async fn connect_session<T>(
    transport: T,
    trace: Option<Arc<TraceWriter>>,
    startup_timeout: Duration,
    advertised: ProtocolVersion,
    mode: ConnectionMode,
) -> Result<Session, SessionError>
where
    T: Transport + 'static,
{
    match (trace, mode) {
        (Some(writer), ConnectionMode::Auto) => {
            Session::from_transport_auto(TracingTransport::new(transport, writer), startup_timeout)
                .await
        }
        (None, ConnectionMode::Auto) => {
            Session::from_transport_auto(transport, startup_timeout).await
        }
        (Some(writer), ConnectionMode::Legacy) => {
            Session::from_transport_with_version(
                TracingTransport::new(transport, writer),
                startup_timeout,
                advertised,
            )
            .await
        }
        (None, ConnectionMode::Legacy) => {
            Session::from_transport_with_version(transport, startup_timeout, advertised).await
        }
        (Some(writer), ConnectionMode::Stateless) => {
            Session::from_transport_stateless(
                TracingTransport::new(transport, writer),
                startup_timeout,
                advertised,
            )
            .await
        }
        (None, ConnectionMode::Stateless) => {
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
