//! TOML config schema for `mcp-loadtest run --config <path>`.
//!
//! See DESIGN.md §7 for the schema.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

mod example;
mod load;
mod schema;
mod tool_slo;
mod validate;

pub use example::example_config;
pub use load::*;
pub use schema::{CONFIG_SCHEMA_JSON, config_schema, config_schema_pretty};
pub use tool_slo::ToolSlo;
pub use validate::{is_managed_remote_header, sanitize_remote_endpoint, validate_remote_endpoint};

/// Top-level config.
///
/// **Locked for M3.** Field additions OK; removal requires sync.
///
/// `#[non_exhaustive]`: construct via [`Config::from_toml_str`] /
/// [`Config::from_file`] (the canonical path) or [`Config::new`] +
/// `with_*` builders — never a struct literal from another crate, so adding
/// a field stays non-breaking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Config {
    /// Server invocation under test.
    pub server: ServerConfig,
    /// Workload definition.
    pub scenario: ScenarioConfig,
    /// Optional pass/fail thresholds.
    #[serde(default)]
    pub thresholds: ThresholdsConfig,
    /// Output controls (run dir, formats).
    #[serde(default)]
    pub output: OutputConfig,
    /// Protocol-validation controls. Off by default (forward-compatible,
    /// ADR 0005); set `[validation] strict = true` to opt in.
    #[serde(default)]
    pub validation: ValidationConfig,
    /// Optional distributed load-generation controls. When present, the CLI
    /// launches one short-lived worker on every configured SSH agent and
    /// coordinates a single fail-closed run.
    #[serde(default)]
    pub distributed: Option<DistributedConfig>,
}

/// Distributed load-generation controls.
///
/// v0.2 uses short-lived workers launched through the local OpenSSH client.
/// The workers exchange the versioned `mcp-loadtest-dist/1` protocol over
/// stdin/stdout; there is no resident daemon to install or expose.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DistributedConfig {
    /// Require every configured agent to become ready and finish. v0.2
    /// deliberately rejects `false` so partial clusters can never silently
    /// produce a passing result.
    #[serde(default = "distributed_require_all_agents")]
    pub require_all_agents: bool,
    /// Maximum time allowed to establish each SSH connection.
    #[serde(default = "distributed_connect_timeout", with = "humantime_serde")]
    pub connect_timeout: Duration,
    /// Maximum time allowed for every worker to prepare its local sessions.
    #[serde(default = "distributed_ready_timeout", with = "humantime_serde")]
    pub ready_timeout: Duration,
    /// Maximum silence allowed before a worker is considered lost.
    #[serde(default = "distributed_heartbeat_timeout", with = "humantime_serde")]
    pub heartbeat_timeout: Duration,
    /// Lead time between the coordinated start message and traffic start.
    #[serde(default = "distributed_start_lead", with = "humantime_serde")]
    pub start_lead: Duration,
    /// SSH workers participating in the run.
    pub agents: Vec<DistributedAgentConfig>,
}

/// One ephemeral distributed worker reached through OpenSSH.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DistributedAgentConfig {
    /// Stable human-readable name used in diagnostics and run evidence.
    pub name: String,
    /// OpenSSH destination (host or `user@host`).
    pub ssh_host: String,
    /// Optional SSH port.
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// Optional private-key path passed to OpenSSH with `-i`.
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
    /// Optional dedicated known-hosts file. Host-key checking remains
    /// enabled; this only changes which trust store OpenSSH reads.
    #[serde(default)]
    pub known_hosts_file: Option<PathBuf>,
}

fn distributed_require_all_agents() -> bool {
    true
}

fn distributed_connect_timeout() -> Duration {
    Duration::from_secs(20)
}

fn distributed_ready_timeout() -> Duration {
    Duration::from_secs(60)
}

fn distributed_heartbeat_timeout() -> Duration {
    Duration::from_secs(15)
}

fn distributed_start_lead() -> Duration {
    Duration::from_secs(1)
}

/// Opt-in protocol-validation block.
///
/// When `strict` is false (the default) the load tester behaves exactly as
/// before — unknown/mismatched schema shapes are tolerated. When true,
/// tool-call args/results are checked against the server's advertised
/// `inputSchema` and mismatches are classified by the protocol layer
/// (`classify_schema_violation`, in the `mcp-loadtest` crate).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationConfig {
    /// Enable strict MCP schema validation. Default `false`.
    #[serde(default)]
    pub strict: bool,
}

/// Server invocation block. See DESIGN.md §7.
///
/// Transport-specific fields:
/// - `transport = "stdio"` (default) → `command` required; `args`, `env`,
///   `working_dir` apply.
/// - `transport = "http"`, `"sse"`, or `"ws"` → `url` required;
///   `command`/`args`/`env` ignored.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ServerConfig {
    /// Stdio: command to spawn (e.g., `"python"`). Required when `transport = "stdio"`.
    #[serde(default)]
    pub command: Option<String>,
    /// Stdio: CLI args (e.g., `["-m", "my_mcp"]`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Stdio: extra env vars merged into the child's env.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Stdio: CWD for the child. Defaults to the parent's CWD.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// HTTP / SSE / WS: endpoint URL. Required for every remote transport.
    #[serde(default)]
    pub url: Option<String>,
    /// Transport: `"stdio"` (M1+), `"http"` / `"sse"` (M4).
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Run startup budget covering transport connect, initialize/discover,
    /// and the required initial tools/list discovery.
    #[serde(default = "default_startup_timeout", with = "humantime_serde")]
    pub startup_timeout: Duration,
    /// HTTP / SSE / WS only: exact-match outbound host allowlist (SSRF guard,
    /// ADR 0012). Empty / unset = allow any *public* host; private /
    /// loopback / link-local IP literals are always blocked unless the exact
    /// literal is listed here (operator escape hatch, e.g. `"127.0.0.1"` for
    /// local testing). No wildcard / suffix matching — entries must be bare
    /// hostnames (no scheme / port / path); validated at config load.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// MCP protocol revision to advertise in `initialize` (ADR 0018):
    /// `"auto"` (the default when unset) advertises
    /// [`crate::ProtocolVersion::DEFAULT_ADVERTISED`]; an explicit supported
    /// revision string (e.g. `"2025-11-25"`) pins it — useful for CI
    /// version-matrix runs. Unsupported values are rejected at config load.
    #[serde(default)]
    pub protocol_version: Option<String>,
    /// HTTP / SSE / WS only: outbound header name to environment-variable
    /// name. The environment variable contains the complete header value
    /// (for example `Authorization = "MCP_AUTHORIZATION"`, where
    /// `MCP_AUTHORIZATION` contains `Bearer ...`).
    ///
    /// Values are resolved only when a remote transport connects and are
    /// never included in config debug output or error messages. Literal
    /// header values are deliberately unsupported so secrets do not have to
    /// live in checked-in TOML.
    #[serde(default)]
    pub headers_from_env: BTreeMap<String, String>,
    /// Optional OAuth flow for remote transports. Secrets are referenced by
    /// environment-variable name and resolved only by the auth runtime.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

/// OAuth configuration for a remote MCP server.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct AuthConfig {
    /// Authentication mechanism. v0.2 supports OAuth.
    #[serde(rename = "type")]
    pub kind: AuthKind,
    /// Grant flow used to obtain an access token.
    #[serde(default)]
    pub flow: OAuthFlow,
    /// Client registration strategy and its strategy-specific fields.
    #[serde(flatten)]
    pub registration: OAuthRegistration,
    /// Initial requested scopes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Request `offline_access` only when the authorization server advertises
    /// support for that scope.
    #[serde(default)]
    pub offline_access: bool,
    /// Maximum bounded 403 insufficient-scope step-up retries.
    #[serde(default = "default_max_step_up_retries")]
    pub max_step_up_retries: u8,
}

/// Authentication mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthKind {
    /// OAuth 2.1 discovery and token acquisition.
    #[serde(rename = "oauth")]
    OAuth,
}

/// OAuth grant flow.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthFlow {
    /// Interactive authorization code with PKCE.
    #[default]
    AuthorizationCode,
    /// Non-interactive client credentials extension.
    ClientCredentials,
}

/// OAuth client registration strategy.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "registration", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthRegistration {
    /// Existing client credentials supplied by the operator.
    PreRegistered {
        /// Public OAuth client identifier.
        client_id: String,
        /// Optional environment variable containing the client secret.
        #[serde(default)]
        client_secret_env: Option<String>,
        /// Token endpoint client authentication policy.
        #[serde(default)]
        token_endpoint_auth_method: TokenEndpointAuthMethod,
    },
    /// Client ID Metadata Document URL (CIMD).
    ClientIdMetadata {
        /// HTTPS URL that is itself the OAuth client identifier.
        client_id_metadata_url: String,
    },
    /// Dynamic client registration using discovered server metadata.
    Dynamic {
        /// Optional human-readable client name sent during registration.
        #[serde(default)]
        client_name: Option<String>,
    },
}

/// Token endpoint authentication method.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TokenEndpointAuthMethod {
    /// Select from authorization-server metadata and available credentials.
    #[default]
    Auto,
    /// Public client; no client authentication.
    None,
    /// HTTP Basic client authentication.
    ClientSecretBasic,
    /// Form-encoded client authentication.
    ClientSecretPost,
}

fn default_max_step_up_retries() -> u8 {
    2
}

/// Safe deserialization mirror for [`AuthConfig`].
///
/// `serde(deny_unknown_fields)` cannot be combined with a flattened tagged
/// enum: the parent rejects the enum's `registration` discriminator before
/// the enum sees it. This explicit wire keeps the desired flat TOML shape,
/// rejects strategy-incompatible fields, and ensures unknown-field errors
/// mention only a key name rather than echoing a line that may contain a
/// credential.
#[derive(Deserialize)]
struct AuthConfigWire {
    #[serde(rename = "type")]
    kind: AuthKind,
    #[serde(default)]
    flow: OAuthFlow,
    registration: String,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret_env: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<TokenEndpointAuthMethod>,
    #[serde(default)]
    client_id_metadata_url: Option<String>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    offline_access: bool,
    #[serde(default = "default_max_step_up_retries")]
    max_step_up_retries: u8,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde::de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for AuthConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = AuthConfigWire::deserialize(deserializer)?;
        if let Some(name) = wire.unknown.keys().next() {
            return Err(<D::Error as serde::de::Error>::custom(format!(
                "unknown field `{name}` in server.auth configuration"
            )));
        }

        let registration = match wire.registration.as_str() {
            "pre_registered" => {
                if wire.client_id_metadata_url.is_some() || wire.client_name.is_some() {
                    return Err(<D::Error as serde::de::Error>::custom(
                        "server.auth pre_registered does not accept client_id_metadata_url or client_name",
                    ));
                }
                OAuthRegistration::PreRegistered {
                    client_id: wire.client_id.ok_or_else(|| {
                        <D::Error as serde::de::Error>::custom(
                            "server.auth.client_id is required for pre_registered",
                        )
                    })?,
                    client_secret_env: wire.client_secret_env,
                    token_endpoint_auth_method: wire.token_endpoint_auth_method.unwrap_or_default(),
                }
            }
            "client_id_metadata" => {
                if wire.client_id.is_some()
                    || wire.client_secret_env.is_some()
                    || wire.token_endpoint_auth_method.is_some()
                    || wire.client_name.is_some()
                {
                    return Err(<D::Error as serde::de::Error>::custom(
                        "server.auth client_id_metadata accepts only client_id_metadata_url",
                    ));
                }
                OAuthRegistration::ClientIdMetadata {
                    client_id_metadata_url: wire.client_id_metadata_url.ok_or_else(|| {
                        <D::Error as serde::de::Error>::custom(
                            "server.auth.client_id_metadata_url is required",
                        )
                    })?,
                }
            }
            "dynamic" => {
                if wire.client_id.is_some()
                    || wire.client_secret_env.is_some()
                    || wire.token_endpoint_auth_method.is_some()
                    || wire.client_id_metadata_url.is_some()
                {
                    return Err(<D::Error as serde::de::Error>::custom(
                        "server.auth dynamic registration accepts only client_name",
                    ));
                }
                OAuthRegistration::Dynamic {
                    client_name: wire.client_name,
                }
            }
            other => {
                return Err(<D::Error as serde::de::Error>::custom(format!(
                    "server.auth.registration: unknown value `{other}`"
                )));
            }
        };

        Ok(Self {
            kind: wire.kind,
            flow: wire.flow,
            registration,
            scopes: wire.scopes,
            offline_access: wire.offline_access,
            max_step_up_retries: wire.max_step_up_retries,
        })
    }
}

/// Deserialization mirror for [`ServerConfig`].
///
/// Unknown values are consumed as [`serde::de::IgnoredAny`] and rejected only
/// after the table has been read. `toml` otherwise attaches the complete
/// offending source line to an `unknown_field` error, which can echo a
/// misspelled literal credential. This keeps the same deny-unknown-fields
/// contract as the public schema while ensuring diagnostics contain only the
/// unknown key name.
#[derive(Deserialize)]
struct ServerConfigWire {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    working_dir: Option<PathBuf>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default = "default_transport")]
    transport: String,
    #[serde(default = "default_startup_timeout", with = "humantime_serde")]
    startup_timeout: Duration,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    #[serde(default)]
    protocol_version: Option<String>,
    #[serde(default)]
    headers_from_env: BTreeMap<String, String>,
    #[serde(default)]
    auth: Option<AuthConfig>,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde::de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for ServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ServerConfigWire::deserialize(deserializer)?;
        if let Some(name) = wire.unknown.keys().next() {
            return Err(<D::Error as serde::de::Error>::custom(format!(
                "unknown field `{name}` in server configuration"
            )));
        }
        Ok(Self {
            command: wire.command,
            args: wire.args,
            env: wire.env,
            working_dir: wire.working_dir,
            url: wire.url,
            transport: wire.transport,
            startup_timeout: wire.startup_timeout,
            allowed_hosts: wire.allowed_hosts,
            protocol_version: wire.protocol_version,
            headers_from_env: wire.headers_from_env,
            auth: wire.auth,
        })
    }
}

fn default_transport() -> String {
    "stdio".to_string()
}
fn default_startup_timeout() -> Duration {
    Duration::from_secs(10)
}

impl ServerConfig {
    /// Build a stdio-transport `ServerConfig` from a command + args. Defaults
    /// match what an inline literal would set (empty env, no working_dir, no
    /// url, 10s startup timeout). Used by the CLI and the in-process
    /// `serve` tool handlers — keeps the field defaults in one place so
    /// adding a future field doesn't require updating four call sites.
    pub fn stdio(command: String, args: Vec<String>) -> Self {
        Self {
            command: Some(command),
            args,
            env: BTreeMap::new(),
            working_dir: None,
            url: None,
            transport: default_transport(),
            startup_timeout: default_startup_timeout(),
            allowed_hosts: Vec::new(),
            protocol_version: None,
            headers_from_env: BTreeMap::new(),
            auth: None,
        }
    }

    /// The typed revision this config advertises in `initialize`: an
    /// explicit supported pin, or
    /// [`ProtocolVersion::DEFAULT_ADVERTISED`] for `"auto"` / unset.
    /// Validation has already rejected anything else, so an unparseable
    /// value here (only reachable by mutating the struct directly) falls
    /// back to the default rather than panicking.
    pub fn resolved_protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
            .as_deref()
            .filter(|s| *s != "auto")
            .and_then(ProtocolVersion::parse)
            .unwrap_or(ProtocolVersion::DEFAULT_ADVERTISED)
    }
}

/// Workload block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScenarioConfig {
    /// Scenario name: `"sustained"`, `"deadlock_probe"`, etc.
    #[serde(rename = "type")]
    pub kind: String,
    /// Free-form per-scenario params (validated at construct time).
    #[serde(flatten)]
    pub params: serde_json::Value,
}

impl ScenarioConfig {
    /// Build a scenario block from a kind name + free-form params object.
    pub fn new(kind: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            params,
        }
    }
}

/// Threshold block. All fields optional — missing means "no constraint".
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ThresholdsConfig {
    #[serde(default, with = "humantime_serde::option")]
    /// p50 latency budget.
    pub p50_latency: Option<Duration>,
    #[serde(default, with = "humantime_serde::option")]
    /// p95 latency budget.
    pub p95_latency: Option<Duration>,
    #[serde(default, with = "humantime_serde::option")]
    /// p99 latency budget.
    pub p99_latency: Option<Duration>,
    #[serde(default, with = "humantime_serde::option")]
    /// p999 latency budget.
    pub p999_latency: Option<Duration>,
    /// Max acceptable error rate (0.0..=1.0).
    #[serde(default)]
    pub error_rate: Option<f64>,
    #[serde(default, with = "humantime_serde::option")]
    /// Per-call hang threshold (also used by `hang_detect`).
    pub hang_timeout: Option<Duration>,
    /// Max RSS growth (MB) tolerated during the run.
    ///
    /// For stdio scenarios that create factory-owned child processes (for
    /// example pooled concurrency or cold start), this gate currently fails
    /// closed because the sampler observes only the initial child. It never
    /// treats that idle process as representative of the whole workload.
    #[serde(default)]
    pub memory_growth_mb: Option<f64>,
    /// Max RSS growth *rate* (MB per second) tolerated during the run.
    ///
    /// Opt-in: when set, the orchestrator fits a least-squares line
    /// (`detect_leak`, in the `mcp-loadtest` crate's soak scenario) to the
    /// sampled RSS timeseries (`ProcessStats::samples`) and flags a
    /// violation when the slope exceeds this budget. Complements
    /// `memory_growth_mb`: the slope catches a slow, steady leak that
    /// stays under the absolute-growth bar within a single run, while the
    /// absolute peak-over-baseline check catches step-jumps that a
    /// flat-then-spike trajectory's fitted slope underestimates.
    ///
    /// Needs enough data to fit a line — at least 3 process samples
    /// spanning a non-zero time window (i.e. a run lasting a few sample
    /// intervals, 500ms each by default). When this threshold is configured,
    /// insufficient or non-finite evidence is a threshold violation rather
    /// than a skipped check, so missing observability cannot produce PASS.
    /// The same fail-closed rule applies when stdio factory sessions put
    /// workload processes outside the sampler's single-PID scope.
    #[serde(default)]
    pub rss_leak_mb_per_sec: Option<f64>,
    /// Per-tool latency SLOs. Each entry maps a tool name to a p99 latency
    /// budget evaluated against per-tool metrics at end-of-run. Missing or
    /// empty means "no per-tool checks". M7 differentiator — see
    /// [`ToolSlo`].
    #[serde(default)]
    pub tool_slos: Vec<ToolSlo>,
}

/// Output block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OutputConfig {
    /// Where per-run dirs are created.
    #[serde(default = "default_report_dir")]
    pub report_dir: PathBuf,
    /// Output formats to emit (`"markdown"`, `"json"`, `"terminal"`).
    #[serde(default = "default_formats")]
    pub formats: Vec<String>,
    /// Optional one-shot OTLP/HTTP JSON export after the report is complete.
    #[serde(default)]
    pub otlp: Option<OtlpOutputConfig>,
    /// Optional rolling baseline history and regression gate.
    #[serde(default)]
    pub history: Option<HistoryOutputConfig>,
}

/// OTLP/HTTP JSON exporter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OtlpOutputConfig {
    /// Full metrics endpoint, normally ending in `/v1/metrics`.
    pub endpoint: String,
    /// Outbound header name to environment-variable name.
    #[serde(default)]
    pub headers_from_env: BTreeMap<String, String>,
    /// Per-attempt request timeout.
    #[serde(default = "default_otlp_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// Fail the command when the collector ultimately rejects the batch.
    #[serde(default)]
    pub fail_on_error: bool,
    /// Exact-match OTLP collector host allowlist.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Maximum attempts including the first request.
    #[serde(default = "default_otlp_max_attempts")]
    pub max_attempts: u8,
}

/// Rolling history baseline configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HistoryOutputConfig {
    /// Portable series identifier (for example `main-sustained`).
    pub series: String,
    /// Root directory containing one subdirectory per series.
    #[serde(default = "default_history_directory")]
    pub directory: PathBuf,
    /// Latest eligible passed samples used for the rolling median.
    #[serde(default = "default_history_window")]
    pub window: usize,
    /// Samples required before relative regression gates activate.
    #[serde(default = "default_history_min_samples")]
    pub min_samples: usize,
    /// Fail during warm-up instead of recording evidence and passing.
    #[serde(default)]
    pub require_history: bool,
    /// Maximum permitted p99 latency increase in percent.
    #[serde(default = "default_history_p99_regression_pct")]
    pub max_p99_regression_pct: f64,
    /// Maximum permitted error-rate increase in percentage points.
    #[serde(default = "default_history_error_rate_regression_pp")]
    pub max_error_rate_regression_pp: f64,
    /// Optional maximum permitted aggregate throughput drop in percent.
    #[serde(default = "default_history_rps_drop_pct")]
    pub max_rps_drop_pct: Option<f64>,
    /// Treat any increase in deadlocks as a regression.
    #[serde(default = "history_deadlock_zero_tolerance")]
    pub deadlock_zero_tolerance: bool,
}

fn default_otlp_timeout() -> Duration {
    Duration::from_secs(10)
}

fn default_otlp_max_attempts() -> u8 {
    3
}

fn default_history_directory() -> PathBuf {
    PathBuf::from("./runs/history")
}

fn default_history_window() -> usize {
    10
}

fn default_history_min_samples() -> usize {
    3
}

fn default_history_p99_regression_pct() -> f64 {
    10.0
}

fn default_history_error_rate_regression_pp() -> f64 {
    0.5
}

fn default_history_rps_drop_pct() -> Option<f64> {
    Some(10.0)
}

fn history_deadlock_zero_tolerance() -> bool {
    true
}

impl OutputConfig {
    /// Build an output block from an explicit run dir + format list.
    pub fn new(report_dir: PathBuf, formats: Vec<String>) -> Self {
        Self {
            report_dir,
            formats,
            otlp: None,
            history: None,
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            report_dir: default_report_dir(),
            formats: default_formats(),
            otlp: None,
            history: None,
        }
    }
}

fn default_report_dir() -> PathBuf {
    PathBuf::from("./runs")
}
fn default_formats() -> Vec<String> {
    vec!["terminal".into(), "markdown".into(), "json".into()]
}

impl Config {
    /// Construct a `Config` programmatically (thresholds / output /
    /// validation default). Pair with the `with_*` builders. The usual
    /// path is [`Config::from_toml_str`] / [`Config::from_file`]; this is
    /// for callers building a run without a TOML file.
    pub fn new(server: ServerConfig, scenario: ScenarioConfig) -> Self {
        Self {
            server,
            scenario,
            thresholds: ThresholdsConfig::default(),
            output: OutputConfig::default(),
            validation: ValidationConfig::default(),
            distributed: None,
        }
    }

    /// Builder: set the pass/fail thresholds block.
    #[must_use]
    pub fn with_thresholds(mut self, thresholds: ThresholdsConfig) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Builder: set the output (run dir / formats) block.
    #[must_use]
    pub fn with_output(mut self, output: OutputConfig) -> Self {
        self.output = output;
        self
    }

    /// Builder: set the protocol-validation block.
    #[must_use]
    pub fn with_validation(mut self, validation: ValidationConfig) -> Self {
        self.validation = validation;
        self
    }

    /// Builder: enable distributed load generation.
    #[must_use]
    pub fn with_distributed(mut self, distributed: DistributedConfig) -> Self {
        self.distributed = Some(distributed);
        self
    }
}
