//! Integration tests for `mcp_loadtest_core::config` parsing + validation.
//!
//! Covers DESIGN.md §7 schema. Negative cases assert `ConfigError::Invalid`
//! is returned (not a panic, not `Toml`), which is the contract callers can
//! pattern-match on.

use std::io::Write;
use std::time::Duration;

use mcp_loadtest_core::config::{self, Config, ConfigError};

#[test]
fn parses_minimal_valid() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "sustained"
    "#;
    let cfg = Config::from_toml_str(toml_in).expect("minimal config must parse");
    assert_eq!(cfg.server.command.as_deref(), Some("python"));
    assert!(cfg.server.args.is_empty(), "args defaults to empty vec");
    assert_eq!(cfg.server.transport, "stdio", "transport defaults to stdio");
    assert_eq!(
        cfg.server.startup_timeout,
        Duration::from_secs(10),
        "startup_timeout defaults to 10s"
    );
    assert_eq!(cfg.scenario.kind, "sustained");
    // thresholds + output use Default impls
    assert!(cfg.thresholds.p99_latency.is_none());
    assert_eq!(cfg.output.report_dir.to_string_lossy(), "./runs");
}

#[test]
fn parses_full_example() {
    // The contract: example_config() must round-trip.
    let s = config::example_config();
    let cfg = Config::from_toml_str(&s).expect("example_config must parse");

    // Spot-check a few non-default fields to make sure example_config is
    // actually exercising every section.
    assert_eq!(cfg.server.command.as_deref(), Some("python"));
    assert_eq!(cfg.server.args, vec!["-m", "my_mcp"]);
    assert_eq!(
        cfg.server.env.get("LOG_LEVEL").map(String::as_str),
        Some("warn")
    );
    assert_eq!(cfg.scenario.kind, "sustained");
    assert_eq!(cfg.thresholds.p99_latency, Some(Duration::from_millis(500)));
    assert_eq!(cfg.thresholds.error_rate, Some(0.01));
    assert_eq!(
        cfg.output.formats,
        vec!["terminal".to_string(), "markdown".into(), "json".into()]
    );
}

#[test]
fn humantime_durations_parse() {
    let toml_in = r#"
        [server]
        command = "python"
        startup_timeout = "5s"

        [scenario]
        type = "sustained"

        [thresholds]
        p99_latency = "100ms"
        hang_timeout = "2s"
    "#;
    let cfg = Config::from_toml_str(toml_in).expect("humantime durations must parse");
    assert_eq!(cfg.server.startup_timeout, Duration::from_secs(5));
    assert_eq!(cfg.thresholds.p99_latency, Some(Duration::from_millis(100)));
    assert_eq!(cfg.thresholds.hang_timeout, Some(Duration::from_secs(2)));
}

#[test]
fn rejects_unknown_scenario() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "bogus"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("unknown scenario must be rejected");
    match err {
        ConfigError::Invalid(msg) => {
            assert!(
                msg.contains("scenario.type") && msg.contains("bogus"),
                "error message should mention the offending field + value, got: {msg}"
            );
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn rejects_invalid_error_rate() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "sustained"

        [thresholds]
        error_rate = 1.5
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("error_rate > 1.0 must be rejected");
    match err {
        ConfigError::Invalid(msg) => {
            assert!(
                msg.contains("error_rate"),
                "error message should mention error_rate, got: {msg}"
            );
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn rejects_negative_error_rate() {
    let toml_in = r#"
        [server]
        command = "python"

        [scenario]
        type = "sustained"

        [thresholds]
        error_rate = -0.1
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("error_rate < 0 must be rejected");
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn from_file_reads_disk() {
    // Use std::env::temp_dir() + UUID-ish suffix to avoid pulling in tempfile
    // for one test (per Agent E task notes).
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        // nanos gives us enough entropy for parallel test runs; we don't
        // need cryptographic uniqueness here.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let path = std::env::temp_dir().join(format!("mcp-loadtest-config-{suffix}.toml"));

    let body = r#"
        [server]
        command = "python"
        args = ["-m", "demo"]

        [scenario]
        type = "deadlock_probe"
    "#;

    {
        let mut f = std::fs::File::create(&path).expect("create tmp file");
        f.write_all(body.as_bytes()).expect("write tmp file");
    }

    let cfg = Config::from_file(&path).expect("from_file must read + parse");
    assert_eq!(cfg.server.command.as_deref(), Some("python"));
    assert_eq!(cfg.server.args, vec!["-m", "demo"]);
    assert_eq!(cfg.scenario.kind, "deadlock_probe");

    // Best-effort cleanup; Windows tempdir doesn't auto-purge.
    let _ = std::fs::remove_file(&path);
}

#[test]
fn from_file_missing_path_is_io_error() {
    let path = std::env::temp_dir().join("mcp-loadtest-does-not-exist.toml");
    // Belt and suspenders: make sure it really doesn't exist.
    let _ = std::fs::remove_file(&path);
    let err = Config::from_file(&path).expect_err("missing file must error");
    assert!(
        matches!(err, ConfigError::Io(_)),
        "expected ConfigError::Io, got {err:?}"
    );
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
    let err = Config::from_toml_str(toml_in).expect_err("unknown transport must be rejected");
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn syntactic_toml_error_is_toml_variant() {
    // Unbalanced brackets: triggers the parser, not the validator.
    let toml_in = r#"
        [server
        command = "python"
    "#;
    let err = Config::from_toml_str(toml_in).expect_err("malformed TOML must error");
    assert!(
        matches!(err, ConfigError::Toml(_)),
        "expected ConfigError::Toml for syntax errors, got {err:?}"
    );
}
