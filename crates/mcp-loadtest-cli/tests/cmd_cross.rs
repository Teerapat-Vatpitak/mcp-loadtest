//! Integration test for the `cross` subcommand.
//!
//! Drives two `mock-normal.py` instances through `cmd_cross::run` and asserts
//! the rendered Markdown table contains the expected rows. We can't import
//! the lib crate's `tests/helpers` (it's another crate's integration-only
//! module), so the fixture path is built from `CARGO_MANIFEST_DIR` of the CLI
//! crate.

use std::path::PathBuf;
use std::time::Duration;

use mcp_loadtest_cli::cmd_cross::{self, CrossArgs, CrossScenario};
use tempfile::tempdir;

fn mock_normal_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("engine")
        .join("tests")
        .join("fixtures")
        .join("mock-normal.py")
}

fn python() -> String {
    std::env::var("MCP_LOADTEST_PYTHON").unwrap_or_else(|_| "python".to_string())
}

/// End-to-end: two `mock-normal.py` servers, 1s of sustained echo each. The
/// rendered table should mention both server commands plus a `p99` row and a
/// `Grade` row.
#[tokio::test]
async fn cross_two_mock_normals_produces_table() {
    let mock = mock_normal_path();
    let mock_str = mock.to_string_lossy().to_string();
    assert!(
        mock.exists(),
        "fixture missing at {mock_str}; check CARGO_MANIFEST_DIR resolution"
    );

    let py = python();
    // Same fixture twice — we just want to exercise the multi-server path.
    let server_a = format!("{py} {mock_str}");
    let server_b = format!("{py} {mock_str}");

    let dir = tempdir().expect("tempdir");

    let args = CrossArgs {
        servers: vec![server_a.clone(), server_b.clone()],
        tool: "echo".to_string(),
        args: "{}".to_string(),
        duration: Duration::from_secs(1),
        scenario: CrossScenario::Sustained,
        output_dir: dir.path().to_path_buf(),
        redact_server_identity: false,
    };

    let outcome = cmd_cross::run(args).await.expect("cross run");
    outcome.gate().expect("both reports should pass");
    let rendered = outcome.rendered;

    assert!(
        rendered.contains(&server_a),
        "expected server A command in output:\n{rendered}",
    );
    assert!(
        rendered.contains(&server_b),
        "expected server B command in output:\n{rendered}",
    );
    assert!(
        rendered.contains("p99 latency"),
        "expected a `p99 latency` row in output:\n{rendered}",
    );
    assert!(
        rendered.contains("Grade"),
        "expected a `Grade` row in output:\n{rendered}",
    );
    assert!(
        rendered.contains("Cross-server comparison"),
        "expected the markdown header in output:\n{rendered}",
    );
}

#[tokio::test]
async fn cross_empty_server_list_errors() {
    let args = CrossArgs {
        servers: vec![],
        tool: "echo".to_string(),
        args: "{}".to_string(),
        duration: Duration::from_secs(1),
        scenario: CrossScenario::Sustained,
        output_dir: PathBuf::from("./runs"),
        redact_server_identity: false,
    };

    let result = cmd_cross::run(args).await;
    assert!(
        result.is_err(),
        "empty server list should error, got {result:?}"
    );
}

#[tokio::test]
async fn cross_records_failure_per_server() {
    // A bogus command — should fail to spawn but the cross run should
    // gracefully record the failure and still produce output.
    let dir = tempdir().expect("tempdir");
    let args = CrossArgs {
        servers: vec!["this-binary-definitely-does-not-exist-xyz".to_string()],
        tool: "echo".to_string(),
        args: "{}".to_string(),
        duration: Duration::from_secs(1),
        scenario: CrossScenario::Sustained,
        output_dir: dir.path().to_path_buf(),
        redact_server_identity: false,
    };

    let outcome = cmd_cross::run(args).await.expect("cross run");
    assert_eq!(outcome.failed_servers, 1);
    assert!(
        outcome.gate().is_err(),
        "per-server failures must make the CLI exit non-zero"
    );
    let rendered = outcome.rendered;
    assert!(
        rendered.contains("FAILED"),
        "expected FAILED status for missing binary:\n{rendered}",
    );
    assert!(
        rendered.contains("## Errors"),
        "expected an errors section listing the failure:\n{rendered}",
    );
}

#[tokio::test]
async fn action_mode_cross_failure_redacts_command_and_parsed_argv() {
    let dir = tempdir().expect("tempdir");
    let sentinel = "ACTION_SERVER_SECRET_7F3B";
    let server = format!("no-such-cross-binary --token {sentinel}");
    let args = CrossArgs {
        servers: vec![server.clone()],
        tool: "echo".to_string(),
        args: "{}".to_string(),
        duration: Duration::from_millis(10),
        scenario: CrossScenario::Sustained,
        output_dir: dir.path().to_path_buf(),
        redact_server_identity: true,
    };

    let outcome = cmd_cross::run(args).await.expect("redacted cross run");
    assert_eq!(outcome.failed_servers, 1);
    assert!(outcome.gate().is_err());
    assert!(
        !outcome.rendered.contains(&server) && !outcome.rendered.contains(sentinel),
        "redacted cross output leaked server identity:\n{}",
        outcome.rendered
    );
    assert!(outcome.rendered.contains("server 1"));
    assert!(outcome.rendered.contains("identity redacted"));
}
