//! Pure config-to-distributed-wire normalization.
//!
//! Controller and worker runtimes consume only these normalized,
//! secret-free plans. Parsing happens once on the controller so every agent
//! receives identical scenario semantics.

use anyhow::{Context, Result, anyhow};
use mcp_loadtest::config::{Config, OAuthFlow, OAuthRegistration, TokenEndpointAuthMethod};
use mcp_loadtest::scenario::pattern::{ErrorBehavior, Pattern};
use mcp_loadtest_distributed::{
    PatternErrorPolicy, PatternPlan, PatternStepPlan, RemoteClientCredentialsAuth, RemoteTarget,
    RemoteTransport, SupportedScenario, WorkloadPlan,
};
use sha2::{Digest, Sha256};

use crate::cmd_run::parse_patterns;

mod controller;
mod worker;

pub(crate) use controller::run_controller;
pub use worker::run_stdio_agent;

const DEFAULT_DISTRIBUTED_SEED: u64 = 0x4D43_504C_4F41_4432;

/// Normalize a validated config into the global workload plan.
pub(crate) fn workload_plan(config: &Config) -> Result<WorkloadPlan> {
    let scenario = match config.scenario.kind.as_str() {
        "sustained" => SupportedScenario::Sustained,
        "pattern" => SupportedScenario::Pattern,
        other => return Err(anyhow!("scenario `{other}` is not distributable")),
    };
    let concurrency = u32_param(config, "concurrent", 10)?;
    let duration = duration_param(config, "duration", "60s")?;
    let duration_ms = u64::try_from(duration.as_millis())
        .map_err(|_| anyhow!("scenario.duration is too large for distributed execution"))?;
    let patterns = parse_patterns(&config.scenario.params)?
        .iter()
        .map(normalize_pattern)
        .collect::<Result<Vec<_>>>()?;
    let seed = match config.scenario.params.get("seed") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow!("scenario.seed must be an unsigned integer"))?,
        None => DEFAULT_DISTRIBUTED_SEED,
    };
    Ok(WorkloadPlan {
        scenario,
        global_concurrency: concurrency,
        duration_ms,
        patterns,
        seed,
    })
}

/// Normalize the remote MCP target without resolving any secret value.
pub(crate) fn remote_target(config: &Config) -> Result<RemoteTarget> {
    let transport = match config.server.transport.as_str() {
        "http" => RemoteTransport::Http,
        "sse" => RemoteTransport::Sse,
        "ws" => RemoteTransport::Ws,
        other => return Err(anyhow!("transport `{other}` is not distributable")),
    };
    let url = config
        .server
        .url
        .clone()
        .ok_or_else(|| anyhow!("distributed remote target requires server.url"))?;
    let startup_timeout_ms = u64::try_from(config.server.startup_timeout.as_millis())
        .map_err(|_| anyhow!("server.startup_timeout is too large"))?;
    let auth = config
        .server
        .auth
        .as_ref()
        .map(normalize_client_credentials)
        .transpose()?;
    Ok(RemoteTarget {
        transport,
        url,
        startup_timeout_ms,
        protocol_version: config.server.protocol_version.clone(),
        headers_from_env: config.server.headers_from_env.clone(),
        allowed_hosts: config.server.allowed_hosts.clone(),
        strict_validation: config.validation.strict,
        auth,
    })
}

/// SHA-256 of the canonical secret-free config used for mismatch detection.
pub(crate) fn config_digest(config: &Config) -> Result<String> {
    let mut secret_free = config.clone();
    // These are environment-variable *names*, not values, and are safe to
    // hash. Stdio child environment values are irrelevant to a remote target
    // and may contain secrets, so remove them before serialization.
    secret_free.server.env.clear();
    let encoded = serde_json::to_vec(&secret_free).context("serializing distributed config")?;
    Ok(hex_digest(&encoded))
}

fn normalize_pattern(pattern: &Pattern) -> Result<PatternPlan> {
    if !pattern.weight.is_finite() || pattern.weight <= 0.0 {
        return Err(anyhow!(
            "distributed patterns require finite positive weights"
        ));
    }
    let think_time_ms = u64::try_from(pattern.think_time.as_millis())
        .map_err(|_| anyhow!("pattern think_time is too large"))?;
    let on_step_error = match pattern.on_step_error {
        ErrorBehavior::Continue => PatternErrorPolicy::Continue,
        ErrorBehavior::Abort => PatternErrorPolicy::Abort,
    };
    let steps = pattern
        .steps
        .iter()
        .map(|step| {
            if step.tool.is_empty() || !step.args.is_object() {
                return Err(anyhow!(
                    "distributed pattern steps require a tool and object args"
                ));
            }
            Ok(PatternStepPlan {
                tool: step.tool.clone(),
                args: step.args.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PatternPlan {
        name: pattern.name.clone(),
        weight: pattern.weight,
        think_time_ms,
        on_step_error,
        steps,
    })
}

fn normalize_client_credentials(
    auth: &mcp_loadtest::config::AuthConfig,
) -> Result<RemoteClientCredentialsAuth> {
    if auth.flow != OAuthFlow::ClientCredentials {
        return Err(anyhow!(
            "distributed OAuth supports only client_credentials"
        ));
    }
    let OAuthRegistration::PreRegistered {
        client_id,
        client_secret_env,
        token_endpoint_auth_method,
    } = &auth.registration
    else {
        return Err(anyhow!(
            "distributed client_credentials requires pre_registered OAuth"
        ));
    };
    let client_secret_env = client_secret_env
        .clone()
        .ok_or_else(|| anyhow!("distributed OAuth requires client_secret_env"))?;
    Ok(RemoteClientCredentialsAuth {
        client_id: client_id.clone(),
        client_secret_env,
        token_endpoint_auth_method: map_token_auth_method(*token_endpoint_auth_method),
        scopes: auth.scopes.clone(),
        offline_access: auth.offline_access,
        max_step_up_retries: auth.max_step_up_retries,
    })
}

fn map_token_auth_method(method: TokenEndpointAuthMethod) -> TokenEndpointAuthMethod {
    method
}

fn u32_param(config: &Config, field: &str, default: u32) -> Result<u32> {
    match config.scenario.params.get(field) {
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow!("scenario.{field} must be an integer in 1..=u32::MAX")),
        None => Ok(default),
    }
}

fn duration_param(config: &Config, field: &str, default: &str) -> Result<std::time::Duration> {
    let raw = config
        .scenario
        .params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(default);
    let duration =
        humantime::parse_duration(raw).with_context(|| format!("parsing scenario.{field}"))?;
    if duration.is_zero() {
        return Err(anyhow!("scenario.{field} must be > 0"));
    }
    Ok(duration)
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_normalizes_without_semantic_loss() {
        let config = Config::from_toml_str(
            r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [scenario]
            type = "sustained"
            concurrent = 4
            duration = "2s"
            seed = 42
            [[scenario.patterns]]
            name = "write"
            weight = 2.0
            think_time = "10ms"
            on_step_error = "abort"
            [[scenario.patterns.steps]]
            tool = "write"
            args = { value = 1 }
            [[distributed.agents]]
            name = "east"
            ssh_host = "east"
            [[distributed.agents]]
            name = "west"
            ssh_host = "west"
            "#,
        )
        .expect("config");
        let plan = workload_plan(&config).expect("plan");
        assert_eq!(plan.global_concurrency, 4);
        assert_eq!(plan.duration_ms, 2_000);
        assert_eq!(plan.seed, 42);
        assert_eq!(plan.patterns[0].on_step_error, PatternErrorPolicy::Abort);
    }

    #[test]
    fn remote_auth_contains_only_environment_reference() {
        let config = Config::from_toml_str(
            r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [server.auth]
            type = "oauth"
            flow = "client_credentials"
            registration = "pre_registered"
            client_id = "ci"
            client_secret_env = "MCP_CLIENT_SECRET"
            scopes = ["mcp:read"]
            [scenario]
            type = "sustained"
            concurrent = 2
            tool = "echo"
            [[distributed.agents]]
            name = "east"
            ssh_host = "east"
            [[distributed.agents]]
            name = "west"
            ssh_host = "west"
            "#,
        )
        .expect("config");
        let target = remote_target(&config).expect("target");
        let encoded = serde_json::to_string(&target).expect("serialize");
        assert!(encoded.contains("MCP_CLIENT_SECRET"));
        assert!(!encoded.contains("secret_value"));
    }
}
