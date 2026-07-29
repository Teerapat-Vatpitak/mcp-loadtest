//! Parsing entry points for the config schema: [`Config::from_toml_str`],
//! [`Config::from_file`], [`ConfigError`], and [`split_server_command`]. The
//! only file in the config module that touches `toml` parsing or `std::fs`.

use super::{Config, validate};

/// Errors returned by `Config::from_*`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// File I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// TOML parse error.
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    /// Semantic validation failure (e.g., unknown scenario name).
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// Parse a shell-style server command string (e.g., `"python -m foo"`) into
/// `(command, args)` by whitespace-splitting. Does NOT honor quoting — callers
/// that need real shell parsing should switch to `shell-words` or similar.
///
/// Returns [`ConfigError::Invalid`] when the input is empty / all whitespace.
/// Centralized here so the CLI and the in-process tool handlers don't keep
/// copy-pasting the same five lines.
pub fn split_server_command(s: &str) -> Result<(String, Vec<String>), ConfigError> {
    let mut parts = s.split_whitespace();
    let command = parts
        .next()
        .ok_or_else(|| ConfigError::Invalid("server command is empty".into()))?
        .to_string();
    let args = parts.map(str::to_string).collect();
    Ok((command, args))
}

impl Config {
    /// Validate a config regardless of how it was constructed.
    ///
    /// `from_toml_str` / `from_file` call this automatically. Programmatic
    /// builders remain infallible for API compatibility, so execution
    /// boundaries such as `Run::execute` must call this method before
    /// creating artifacts, resolving credentials, or starting traffic.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate::validate(self)
    }

    /// Parse a TOML string into a `Config` and run semantic validation.
    ///
    /// Returns [`ConfigError::Toml`] on syntactic failures and
    /// [`ConfigError::Invalid`] on semantic violations (unknown transport /
    /// scenario kind, out-of-range thresholds).
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read `path` from disk and parse it. I/O errors are returned as
    /// [`ConfigError::Io`]; everything else is delegated to
    /// [`Config::from_toml_str`].
    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)?;
        Self::from_toml_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::{ServerConfig, example_config};
    use crate::version::ProtocolVersion;

    #[test]
    fn example_config_round_trips() {
        let s = example_config();
        let cfg = Config::from_toml_str(&s).expect("example_config must parse");
        assert_eq!(cfg.server.command.as_deref(), Some("python"));
        assert_eq!(cfg.server.transport, "stdio");
        assert_eq!(cfg.scenario.kind, "sustained");
    }

    #[test]
    fn http_transport_requires_url() {
        let toml_in = r#"
            [server]
            transport = "http"
            [scenario]
            type = "sustained"
        "#;
        let err = Config::from_toml_str(toml_in).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(ref m) if m.contains("server.url")));
    }

    #[test]
    fn http_transport_with_url_parses() {
        let toml_in = r#"
            [server]
            transport = "http"
            url = "http://127.0.0.1:8080/mcp"
            [scenario]
            type = "sustained"
            tool = "echo"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("http config must parse");
        assert_eq!(cfg.server.transport, "http");
        assert_eq!(cfg.server.url.as_deref(), Some("http://127.0.0.1:8080/mcp"));
        assert!(cfg.server.command.is_none());
    }

    #[test]
    fn stdio_transport_requires_command() {
        let toml_in = r#"
            [server]
            transport = "stdio"
            [scenario]
            type = "sustained"
        "#;
        let err = Config::from_toml_str(toml_in).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(ref m) if m.contains("server.command")));
    }

    #[test]
    fn rejects_unknown_transport() {
        let toml_in = r#"
            [server]
            command = "python"
            transport = "carrier-pigeon"
            [scenario]
            type = "sustained"
        "#;
        let err = Config::from_toml_str(toml_in).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_zero_hang_timeout() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "sustained"
            [thresholds]
            hang_timeout = "0s"
        "#;
        let err = Config::from_toml_str(toml_in).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn parses_valid_distributed_config_with_defaults() {
        let toml_in = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [scenario]
            type = "sustained"
            tool = "echo"
            concurrent = 4
            [[distributed.agents]]
            name = "east"
            ssh_host = "loadgen-east"
            [[distributed.agents]]
            name = "west"
            ssh_host = "runner@loadgen-west"
            ssh_port = 2222
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("distributed config must parse");
        let distributed = cfg.distributed.expect("distributed block");
        assert!(distributed.require_all_agents);
        assert_eq!(distributed.connect_timeout, Duration::from_secs(20));
        assert_eq!(distributed.ready_timeout, Duration::from_secs(60));
        assert_eq!(distributed.heartbeat_timeout, Duration::from_secs(15));
        assert_eq!(distributed.start_lead, Duration::from_secs(1));
        assert_eq!(distributed.agents.len(), 2);
        assert_eq!(distributed.agents[1].ssh_port, Some(2222));
    }

    #[test]
    fn rejects_distributed_stdio_and_partial_cluster_policy() {
        let stdio = r#"
            [server]
            command = "python"
            [scenario]
            type = "sustained"
            tool = "echo"
            [[distributed.agents]]
            name = "east"
            ssh_host = "loadgen-east"
            [[distributed.agents]]
            name = "west"
            ssh_host = "loadgen-west"
        "#;
        let err = Config::from_toml_str(stdio).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("remote stdio")),
            "got {err}"
        );

        let partial = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [scenario]
            type = "pattern"
            concurrent = 2
            [distributed]
            require_all_agents = false
            [[distributed.agents]]
            name = "east"
            ssh_host = "loadgen-east"
            [[distributed.agents]]
            name = "west"
            ssh_host = "loadgen-west"
        "#;
        let err = Config::from_toml_str(partial).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("requires true")),
            "got {err}"
        );
    }

    #[test]
    fn rejects_duplicate_agents_and_undersharded_concurrency() {
        let duplicate = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [scenario]
            type = "sustained"
            tool = "echo"
            concurrent = 2
            [[distributed.agents]]
            name = "same"
            ssh_host = "loadgen-east"
            [[distributed.agents]]
            name = "same"
            ssh_host = "loadgen-west"
        "#;
        let err = Config::from_toml_str(duplicate).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("duplicate agent name")),
            "got {err}"
        );

        let undersharded = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [scenario]
            type = "sustained"
            tool = "echo"
            concurrent = 1
            [[distributed.agents]]
            name = "east"
            ssh_host = "loadgen-east"
            [[distributed.agents]]
            name = "west"
            ssh_host = "loadgen-west"
        "#;
        let err = Config::from_toml_str(undersharded).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("one slot per agent")),
            "got {err}"
        );
    }

    #[test]
    fn parses_oauth_authorization_code_config() {
        let toml_in = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [server.auth]
            type = "oauth"
            flow = "authorization_code"
            registration = "pre_registered"
            client_id = "mcp-loadtest"
            client_secret_env = "MCP_CLIENT_SECRET"
            token_endpoint_auth_method = "client_secret_basic"
            scopes = ["mcp:read", "mcp:tools"]
            offline_access = true
            max_step_up_retries = 2
            [scenario]
            type = "sustained"
            tool = "echo"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("OAuth config must parse");
        let auth = cfg.server.auth.expect("auth block");
        assert_eq!(auth.flow, crate::config::OAuthFlow::AuthorizationCode);
        assert_eq!(auth.scopes, vec!["mcp:read", "mcp:tools"]);
        assert!(auth.offline_access);
        assert!(matches!(
            auth.registration,
            crate::config::OAuthRegistration::PreRegistered { .. }
        ));
    }

    #[test]
    fn rejects_oauth_static_authorization_and_invalid_client_credentials() {
        let static_authorization = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [server.headers_from_env]
            Authorization = "MCP_AUTHORIZATION"
            [server.auth]
            type = "oauth"
            registration = "dynamic"
            [scenario]
            type = "sustained"
            tool = "echo"
        "#;
        let err = Config::from_toml_str(static_authorization).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("mutually exclusive")),
            "got {err}"
        );

        let client_credentials = r#"
            [server]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            [server.auth]
            type = "oauth"
            flow = "client_credentials"
            registration = "pre_registered"
            client_id = "ci"
            [scenario]
            type = "sustained"
            tool = "echo"
        "#;
        let err = Config::from_toml_str(client_credentials).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("client_secret_env")),
            "got {err}"
        );
    }

    #[test]
    fn parses_otlp_and_history_output_blocks() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "sustained"
            tool = "echo"
            [output]
            formats = ["json", "junit", "prometheus"]
            [output.otlp]
            endpoint = "https://otel.example.test/v1/metrics"
            timeout = "5s"
            max_attempts = 4
            [output.otlp.headers_from_env]
            Authorization = "OTEL_AUTHORIZATION"
            [output.history]
            series = "main-sustained"
            directory = "./history"
            window = 7
            min_samples = 3
            require_history = true
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("output blocks must parse");
        let otlp = cfg.output.otlp.expect("otlp block");
        assert_eq!(otlp.timeout, Duration::from_secs(5));
        assert_eq!(otlp.max_attempts, 4);
        let history = cfg.output.history.expect("history block");
        assert_eq!(history.series, "main-sustained");
        assert_eq!(history.window, 7);
        assert!(history.require_history);
    }

    #[test]
    fn rejects_unsafe_or_impossible_output_settings() {
        let unknown_format = r#"
            [server]
            command = "python"
            [scenario]
            type = "sustained"
            tool = "echo"
            [output]
            formats = ["json", "telepathy"]
        "#;
        let err = Config::from_toml_str(unknown_format).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("unknown format")),
            "got {err}"
        );

        let plaintext_secret = r#"
            [server]
            command = "python"
            [scenario]
            type = "sustained"
            tool = "echo"
            [output.otlp]
            endpoint = "http://127.0.0.1:4318/v1/metrics"
            [output.otlp.headers_from_env]
            Authorization = "OTEL_AUTHORIZATION"
        "#;
        let err = Config::from_toml_str(plaintext_secret).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("requires an HTTPS")),
            "got {err}"
        );

        let impossible_history = r#"
            [server]
            command = "python"
            [scenario]
            type = "sustained"
            tool = "echo"
            [output.history]
            series = "../escape"
            window = 2
            min_samples = 3
        "#;
        let err = Config::from_toml_str(impossible_history).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("output.history.series")),
            "got {err}"
        );
    }

    #[test]
    fn parses_rss_leak_mb_per_sec_threshold() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "soak"
            [thresholds]
            rss_leak_mb_per_sec = 0.5
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("rss_leak threshold must parse");
        assert_eq!(cfg.thresholds.rss_leak_mb_per_sec, Some(0.5));
    }

    #[test]
    fn rss_leak_mb_per_sec_defaults_to_none() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "soak"
            [thresholds]
            memory_growth_mb = 50.0
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("config must parse");
        assert_eq!(cfg.thresholds.rss_leak_mb_per_sec, None);
    }

    #[test]
    fn rejects_negative_rss_leak_mb_per_sec() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "soak"
            [thresholds]
            rss_leak_mb_per_sec = -0.5
        "#;
        let err = Config::from_toml_str(toml_in).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("rss_leak_mb_per_sec")),
            "expected Invalid mentioning rss_leak_mb_per_sec, got: {err}"
        );
    }

    #[test]
    fn rejects_non_finite_process_thresholds() {
        for (field, value) in [
            ("memory_growth_mb", "nan"),
            ("memory_growth_mb", "inf"),
            ("memory_growth_mb", "-inf"),
            ("rss_leak_mb_per_sec", "nan"),
            ("rss_leak_mb_per_sec", "inf"),
            ("rss_leak_mb_per_sec", "-inf"),
        ] {
            let toml_in = format!(
                r#"
                    [server]
                    command = "python"
                    [scenario]
                    type = "soak"
                    [thresholds]
                    {field} = {value}
                "#
            );
            let err = match Config::from_toml_str(&toml_in) {
                Ok(_) => panic!("{field} accepted non-finite value {value}"),
                Err(err) => err,
            };
            assert!(
                matches!(err, ConfigError::Invalid(ref message)
                    if message.contains(field) && message.contains("finite")),
                "{field}={value}: expected a finite-value validation error, got {err}"
            );
        }
    }

    #[test]
    fn accepts_scenario_kind_ramp() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "ramp"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("ramp must parse");
        assert_eq!(cfg.scenario.kind, "ramp");
    }

    #[test]
    fn accepts_scenario_kind_soak() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "soak"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("soak must parse");
        assert_eq!(cfg.scenario.kind, "soak");
    }

    #[test]
    fn accepts_scenario_kind_fuzzer() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "fuzzer"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("fuzzer must parse");
        assert_eq!(cfg.scenario.kind, "fuzzer");
    }

    #[test]
    fn accepts_scenario_kind_race_check() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "race_check"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("race_check must parse");
        assert_eq!(cfg.scenario.kind, "race_check");
    }

    #[test]
    fn accepts_scenario_kind_pattern() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "pattern"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("pattern must parse");
        assert_eq!(cfg.scenario.kind, "pattern");
    }

    #[test]
    fn parses_protocol_version_pin() {
        let toml_in = r#"
            [server]
            command = "python"
            protocol_version = "2025-11-25"
            [scenario]
            type = "sustained"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("pin must parse");
        assert_eq!(
            cfg.server.resolved_protocol_version(),
            ProtocolVersion::V2025_11_25
        );
    }

    #[test]
    fn protocol_version_auto_and_unset_resolve_to_default() {
        let toml_auto = r#"
            [server]
            command = "python"
            protocol_version = "auto"
            [scenario]
            type = "sustained"
        "#;
        let cfg = Config::from_toml_str(toml_auto).expect("auto must parse");
        assert_eq!(
            cfg.server.resolved_protocol_version(),
            ProtocolVersion::DEFAULT_ADVERTISED
        );

        let unset = ServerConfig::stdio("python".into(), Vec::new());
        assert_eq!(
            unset.resolved_protocol_version(),
            ProtocolVersion::DEFAULT_ADVERTISED
        );
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let toml_in = r#"
            [server]
            command = "python"
            protocol_version = "2019-01-01"
            [scenario]
            type = "sustained"
        "#;
        let err = Config::from_toml_str(toml_in).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(ref m) if m.contains("protocol_version")),
            "expected Invalid mentioning protocol_version, got: {err}"
        );
    }

    #[test]
    fn parses_spike_kind() {
        let toml_in = r#"
            [server]
            command = "python"
            [scenario]
            type = "spike"
        "#;
        let cfg = Config::from_toml_str(toml_in).expect("spike must parse");
        assert_eq!(cfg.scenario.kind, "spike");
    }
}
