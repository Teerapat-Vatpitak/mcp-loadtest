//! Integration tests for the `run` subcommand's public surface.
//!
//! The `cmd_run` refactor split the module into private submodules
//! (`builder` / `params` / `patterns`). This guards the two public items
//! `main.rs` depends on across the bin↔lib boundary: the re-exported
//! [`cmd_run::parse_dur_str`] helper (shared with `deadlock-probe` / `cross`)
//! and the [`cmd_run::run_from_config`] entry point. The Action-only
//! [`cmd_run::run_from_config_with_output`] path is exercised end-to-end in
//! `run_strict.rs`.

use std::path::Path;
use std::time::Duration;

use mcp_loadtest_cli::cmd_run;
use tempfile::tempdir;

#[test]
fn parse_dur_str_reexport_is_reachable_and_parses() {
    assert_eq!(
        cmd_run::parse_dur_str("250ms").expect("250ms parses"),
        Duration::from_millis(250)
    );
    assert_eq!(
        cmd_run::parse_dur_str("2s").expect("2s parses"),
        Duration::from_secs(2)
    );
    assert!(
        cmd_run::parse_dur_str("not-a-duration").is_err(),
        "garbage duration should error"
    );
}

#[test]
fn run_from_config_errors_on_missing_file() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");

    let result = rt.block_on(cmd_run::run_from_config(
        Path::new("definitely/does/not/exist.toml"),
        false,
        false,
        None,
    ));

    assert!(
        result.is_err(),
        "a missing config path must surface an error, got: {result:?}"
    );
}

#[test]
fn action_mode_config_error_does_not_echo_server_identity() {
    let tmp = tempdir().expect("tempdir");
    let config = tmp.path().join("invalid.toml");
    let sentinel = "ACTION_SERVER_SECRET_7F3B";
    std::fs::write(
        &config,
        format!("[server]\ncommand = \"{sentinel}\"\nthis is not toml\n"),
    )
    .expect("write invalid config");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");

    let error = rt
        .block_on(cmd_run::run_from_config_with_output(
            &config, false, false, None, None, true,
        ))
        .expect_err("invalid Action config must fail");
    let diagnostic = format!("{error:#}");
    assert!(!diagnostic.contains(sentinel), "leaked: {diagnostic}");
    assert!(diagnostic.contains("identity redacted"), "{diagnostic}");
}
