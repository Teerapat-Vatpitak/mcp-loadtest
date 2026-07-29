//! Agent-local safety policy.
//!
//! Controller-supplied `allowed_hosts` never widens this policy. A worker
//! reconstructs the target allowlist from its own policy before handing a
//! plan to the protocol/engine layer.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use mcp_loadtest_core::config::is_managed_remote_header;
use thiserror::Error;
use url::Url;

use crate::protocol::{PrepareFrame, RemoteTarget, RemoteTransport};

/// Local limits and target authorization for a worker.
#[derive(Debug, Clone)]
pub struct AgentPolicy {
    /// Exact hostnames/IP literals this worker may target.
    ///
    /// An empty set permits public hosts and rejects restricted IP literals.
    /// A non-empty set restricts all targets to the listed hosts.
    pub allowed_target_hosts: BTreeSet<String>,
    /// Permit plaintext `http`, `ws`, or HTTP-backed SSE.
    pub allow_plaintext: bool,
    /// Maximum local sessions accepted in one shard.
    pub max_concurrency: u32,
    /// Maximum measurement duration.
    pub max_duration: Duration,
    /// Maximum target startup timeout.
    pub max_startup_timeout: Duration,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            allowed_target_hosts: BTreeSet::new(),
            allow_plaintext: false,
            max_concurrency: 10_000,
            max_duration: Duration::from_secs(24 * 60 * 60),
            max_startup_timeout: Duration::from_secs(5 * 60),
        }
    }
}

impl AgentPolicy {
    /// Validate a prepare frame and return a target whose SSRF escape-hatch
    /// list has been constrained to agent-local policy.
    pub fn authorize(&self, prepare: &PrepareFrame) -> Result<RemoteTarget, PolicyError> {
        if prepare.shard.agent_count == 0
            || prepare.shard.index >= prepare.shard.agent_count
            || prepare.shard.agent_name.is_empty()
        {
            return Err(PolicyError::InvalidShard);
        }
        if prepare.heartbeat_interval_ms == 0 {
            return Err(PolicyError::InvalidHeartbeatInterval);
        }
        if prepare.plan.concurrency == 0
            || prepare.plan.concurrency != prepare.shard.concurrency
            || prepare.plan.concurrency > self.max_concurrency
        {
            return Err(PolicyError::Concurrency {
                requested: prepare.plan.concurrency,
                maximum: self.max_concurrency,
            });
        }
        if prepare.plan.duration_ms == 0
            || Duration::from_millis(prepare.plan.duration_ms) > self.max_duration
        {
            return Err(PolicyError::Duration {
                requested_ms: prepare.plan.duration_ms,
                maximum_ms: millis_u64(self.max_duration),
            });
        }
        if prepare.plan.patterns.is_empty()
            || prepare.plan.patterns.iter().any(|pattern| {
                !pattern.weight.is_finite()
                    || pattern.weight <= 0.0
                    || pattern.steps.is_empty()
                    || pattern
                        .steps
                        .iter()
                        .any(|step| step.tool.is_empty() || !step.args.is_object())
            })
        {
            return Err(PolicyError::InvalidWorkload);
        }

        let target = &prepare.target;
        if target.startup_timeout_ms == 0
            || Duration::from_millis(target.startup_timeout_ms) > self.max_startup_timeout
        {
            return Err(PolicyError::StartupTimeout {
                requested_ms: target.startup_timeout_ms,
                maximum_ms: millis_u64(self.max_startup_timeout),
            });
        }
        let parsed =
            Url::parse(&target.url).map_err(|error| PolicyError::InvalidUrl(error.to_string()))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(PolicyError::UrlCredentials);
        }
        if parsed.fragment().is_some() {
            return Err(PolicyError::UrlFragment);
        }
        let host = parsed
            .host_str()
            .ok_or(PolicyError::MissingHost)?
            .to_ascii_lowercase();
        validate_scheme(target.transport, parsed.scheme())?;

        let secure = matches!(parsed.scheme(), "https" | "wss");
        if !secure && !self.allow_plaintext {
            return Err(PolicyError::PlaintextDenied);
        }
        if !secure && (!target.headers_from_env.is_empty() || target.auth.is_some()) {
            return Err(PolicyError::CredentialOverPlaintext);
        }

        let explicitly_allowed = self.allowed_target_hosts.contains(&host);
        if !self.allowed_target_hosts.is_empty() && !explicitly_allowed {
            return Err(PolicyError::HostDenied(host));
        }
        if host.parse::<IpAddr>().is_ok_and(is_restricted_ip) && !explicitly_allowed {
            return Err(PolicyError::RestrictedAddress(host));
        }
        validate_header_refs(target)?;
        if let Some(auth) = &target.auth {
            validate_remote_auth(auth)?;
        }

        // A controller-controlled escape hatch is not an authorization
        // source. Rebuild it solely from the local exact-match decision.
        let mut authorized = target.clone();
        authorized.allowed_hosts = if explicitly_allowed {
            vec![host]
        } else {
            Vec::new()
        };
        Ok(authorized)
    }
}

/// Agent-local policy rejection.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyError {
    /// Shard identity/count/index is malformed.
    #[error("invalid distributed shard metadata")]
    InvalidShard,
    /// Heartbeats must be enabled while preparing and running.
    #[error("heartbeat interval must be greater than zero")]
    InvalidHeartbeatInterval,
    /// Local concurrency exceeded policy or disagreed with the shard.
    #[error("local concurrency {requested} is invalid or exceeds maximum {maximum}")]
    Concurrency {
        /// Requested local sessions.
        requested: u32,
        /// Agent limit.
        maximum: u32,
    },
    /// Measurement duration is zero or over budget.
    #[error("measurement duration {requested_ms}ms is invalid or exceeds {maximum_ms}ms")]
    Duration {
        /// Requested duration.
        requested_ms: u64,
        /// Agent limit.
        maximum_ms: u64,
    },
    /// Pattern structure is unsafe or empty.
    #[error("workload must contain positive weighted patterns with object arguments")]
    InvalidWorkload,
    /// Target startup budget is invalid.
    #[error("startup timeout {requested_ms}ms is invalid or exceeds {maximum_ms}ms")]
    StartupTimeout {
        /// Requested timeout.
        requested_ms: u64,
        /// Agent limit.
        maximum_ms: u64,
    },
    /// URL parsing failed.
    #[error("invalid remote target URL: {0}")]
    InvalidUrl(String),
    /// Target URL omitted its hostname.
    #[error("remote target URL has no host")]
    MissingHost,
    /// Embedded URL credentials are never accepted.
    #[error("remote target URL must not contain username or password")]
    UrlCredentials,
    /// URL fragments are not sent to the server and usually signal a typo.
    #[error("remote target URL must not contain a fragment")]
    UrlFragment,
    /// Scheme and transport disagree.
    #[error("URL scheme `{scheme}` is incompatible with transport `{transport}`")]
    SchemeMismatch {
        /// Configured transport.
        transport: &'static str,
        /// URL scheme.
        scheme: String,
    },
    /// Plaintext target was not enabled locally.
    #[error("plaintext remote targets are disabled by agent policy")]
    PlaintextDenied,
    /// Headers or OAuth credentials must never travel over plaintext.
    #[error("remote credentials require a TLS target")]
    CredentialOverPlaintext,
    /// Exact local host allowlist rejected the target.
    #[error("target host `{0}` is not in the agent-local allowlist")]
    HostDenied(String),
    /// Private/loopback/link-local literal lacks local authorization.
    #[error("restricted target address `{0}` requires agent-local authorization")]
    RestrictedAddress(String),
    /// Outbound header name is malformed or managed by the transport.
    #[error("invalid outbound header name `{0}`")]
    InvalidHeaderName(String),
    /// Environment reference is not portable.
    #[error("invalid environment-variable reference `{0}`")]
    InvalidEnvironmentName(String),
    /// OAuth client id is empty or contains control characters.
    #[error("invalid distributed OAuth client id")]
    InvalidClientId,
    /// Distributed client credentials never request offline access.
    #[error("distributed client credentials must set offline_access = false")]
    OfflineAccess,
    /// Step-up retries are deliberately bounded.
    #[error("distributed OAuth max_step_up_retries must be in 0..=3")]
    StepUpRetries,
    /// Scope token is empty or contains whitespace/control characters.
    #[error("invalid distributed OAuth scope `{0}`")]
    InvalidScope(String),
}

fn validate_scheme(transport: RemoteTransport, scheme: &str) -> Result<(), PolicyError> {
    let valid = match transport {
        RemoteTransport::Http | RemoteTransport::Sse => matches!(scheme, "http" | "https"),
        RemoteTransport::Ws => matches!(scheme, "ws" | "wss"),
    };
    if valid {
        Ok(())
    } else {
        let transport = match transport {
            RemoteTransport::Http => "http",
            RemoteTransport::Sse => "sse",
            RemoteTransport::Ws => "ws",
        };
        Err(PolicyError::SchemeMismatch {
            transport,
            scheme: scheme.to_owned(),
        })
    }
}

fn validate_header_refs(target: &RemoteTarget) -> Result<(), PolicyError> {
    for (header, environment) in &target.headers_from_env {
        if !is_http_token(header) || is_managed_remote_header(&header.to_ascii_lowercase()) {
            return Err(PolicyError::InvalidHeaderName(header.clone()));
        }
        if !is_portable_env_name(environment) {
            return Err(PolicyError::InvalidEnvironmentName(environment.clone()));
        }
    }
    Ok(())
}

fn validate_remote_auth(
    auth: &crate::protocol::RemoteClientCredentialsAuth,
) -> Result<(), PolicyError> {
    if auth.client_id.is_empty()
        || auth.client_id.len() > 4_096
        || auth.client_id.chars().any(char::is_control)
    {
        return Err(PolicyError::InvalidClientId);
    }
    if !is_portable_env_name(&auth.client_secret_env) {
        return Err(PolicyError::InvalidEnvironmentName(
            auth.client_secret_env.clone(),
        ));
    }
    if auth.offline_access
        || auth
            .scopes
            .iter()
            .any(|scope| scope.eq_ignore_ascii_case("offline_access"))
    {
        return Err(PolicyError::OfflineAccess);
    }
    if auth.max_step_up_retries > 3 {
        return Err(PolicyError::StepUpRetries);
    }
    for scope in &auth.scopes {
        if scope.is_empty()
            || scope.len() > 1_024
            || scope
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(PolicyError::InvalidScope(scope.clone()));
        }
    }
    Ok(())
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_portable_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_restricted_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_multicast()
                || address == Ipv4Addr::BROADCAST
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || is_ipv6_unique_local(address)
                || is_ipv6_link_local(address)
        }
    }
}

fn is_ipv6_unique_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xfe00 == 0xfc00
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mcp_loadtest_core::config::TokenEndpointAuthMethod;
    use serde_json::json;

    use super::*;
    use crate::protocol::{
        AgentShard, AgentWorkloadPlan, PatternPlan, PatternStepPlan, PrepareFrame,
        RemoteClientCredentialsAuth, SupportedScenario,
    };

    fn prepare(url: &str) -> PrepareFrame {
        PrepareFrame {
            job_id: "job".to_owned(),
            config_digest: "sha256:test".to_owned(),
            target: RemoteTarget {
                transport: RemoteTransport::Http,
                url: url.to_owned(),
                startup_timeout_ms: 10_000,
                protocol_version: None,
                headers_from_env: BTreeMap::new(),
                // Deliberately controller-controlled: policy must erase it.
                allowed_hosts: vec!["127.0.0.1".to_owned()],
                strict_validation: false,
                auth: None,
            },
            plan: AgentWorkloadPlan {
                scenario: SupportedScenario::Sustained,
                concurrency: 1,
                duration_ms: 1_000,
                patterns: vec![PatternPlan {
                    name: "echo".to_owned(),
                    weight: 1.0,
                    think_time_ms: 0,
                    on_step_error: crate::protocol::PatternErrorPolicy::Continue,
                    steps: vec![PatternStepPlan {
                        tool: "echo".to_owned(),
                        args: json!({}),
                    }],
                }],
                seed: 1,
            },
            shard: AgentShard {
                agent_name: "east".to_owned(),
                index: 0,
                agent_count: 1,
                concurrency: 1,
            },
            heartbeat_interval_ms: 5_000,
        }
    }

    #[test]
    fn public_tls_target_is_allowed_by_default() {
        let authorized = AgentPolicy::default()
            .authorize(&prepare("https://api.example.com/mcp"))
            .unwrap();
        assert!(authorized.allowed_hosts.is_empty());
    }

    #[test]
    fn controller_allowlist_cannot_authorize_loopback() {
        assert_eq!(
            AgentPolicy {
                allow_plaintext: true,
                ..AgentPolicy::default()
            }
            .authorize(&prepare("http://127.0.0.1/mcp"))
            .unwrap_err(),
            PolicyError::RestrictedAddress("127.0.0.1".to_owned())
        );
    }

    #[test]
    fn local_allowlist_can_authorize_loopback() {
        let policy = AgentPolicy {
            allowed_target_hosts: BTreeSet::from(["127.0.0.1".to_owned()]),
            allow_plaintext: true,
            ..AgentPolicy::default()
        };
        let authorized = policy.authorize(&prepare("http://127.0.0.1/mcp")).unwrap();
        assert_eq!(authorized.allowed_hosts, vec!["127.0.0.1"]);
    }

    #[test]
    fn credentials_still_require_tls_when_plaintext_is_enabled() {
        let mut input = prepare("http://public.example/mcp");
        input
            .target
            .headers_from_env
            .insert("Authorization".to_owned(), "MCP_TOKEN".to_owned());
        let policy = AgentPolicy {
            allow_plaintext: true,
            ..AgentPolicy::default()
        };
        assert_eq!(
            policy.authorize(&input).unwrap_err(),
            PolicyError::CredentialOverPlaintext
        );
    }

    #[test]
    fn distributed_oauth_recipe_requires_tls_and_portable_secret_env() {
        let auth = RemoteClientCredentialsAuth {
            client_id: "load-generator".to_owned(),
            client_secret_env: "MCP_CLIENT_SECRET".to_owned(),
            token_endpoint_auth_method: TokenEndpointAuthMethod::ClientSecretBasic,
            scopes: vec!["mcp:read".to_owned()],
            offline_access: false,
            max_step_up_retries: 2,
        };

        let mut plaintext = prepare("http://public.example/mcp");
        plaintext.target.auth = Some(auth.clone());
        assert_eq!(
            AgentPolicy {
                allow_plaintext: true,
                ..AgentPolicy::default()
            }
            .authorize(&plaintext)
            .unwrap_err(),
            PolicyError::CredentialOverPlaintext
        );

        let mut bad_env = prepare("https://public.example/mcp");
        bad_env.target.auth = Some(RemoteClientCredentialsAuth {
            client_secret_env: "NOT-PORTABLE".to_owned(),
            ..auth.clone()
        });
        assert_eq!(
            AgentPolicy::default().authorize(&bad_env).unwrap_err(),
            PolicyError::InvalidEnvironmentName("NOT-PORTABLE".to_owned())
        );

        let mut secure = prepare("https://public.example/mcp");
        secure.target.auth = Some(auth);
        assert!(AgentPolicy::default().authorize(&secure).is_ok());
    }

    #[test]
    fn distributed_oauth_rejects_offline_access_and_unbounded_step_up() {
        let mut input = prepare("https://public.example/mcp");
        input.target.auth = Some(RemoteClientCredentialsAuth {
            client_id: "load-generator".to_owned(),
            client_secret_env: "MCP_CLIENT_SECRET".to_owned(),
            token_endpoint_auth_method: TokenEndpointAuthMethod::ClientSecretPost,
            scopes: vec!["offline_access".to_owned()],
            offline_access: false,
            max_step_up_retries: 2,
        });
        assert_eq!(
            AgentPolicy::default().authorize(&input).unwrap_err(),
            PolicyError::OfflineAccess
        );

        if let Some(auth) = &mut input.target.auth {
            auth.scopes = vec!["mcp:read".to_owned()];
            auth.max_step_up_retries = 4;
        }
        assert_eq!(
            AgentPolicy::default().authorize(&input).unwrap_err(),
            PolicyError::StepUpRetries
        );
    }
}
