//! Adversarial checks for the composite Action's private redaction mode.
//!
//! These spawn the real CLI because the contract covers both stdout and the
//! top-level error diagnostic written to stderr.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

const SENTINEL: &str = "ACTION_SERVER_SECRET_7F3B";
const ARGS_SENTINEL: &str = "ACTION_ARG_SECRET_91C4";

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mcp-loadtest"))
        .args(args)
        .output()
        .expect("spawn mcp-loadtest")
}

fn assert_redacted(output: &Output, server: &str) {
    assert!(!output.status.success(), "adversarial command should fail");
    for (stream, bytes) in [
        ("stdout", output.stdout.as_slice()),
        ("stderr", output.stderr.as_slice()),
    ] {
        let rendered = String::from_utf8_lossy(bytes);
        assert!(
            !rendered.contains(SENTINEL) && !rendered.contains(server),
            "{stream} leaked Action server identity:\n{rendered}"
        );
    }
}

fn assert_no_sentinel(output: &Output, sentinel: &str) {
    assert!(!output.status.success(), "adversarial command should fail");
    for (stream, bytes) in [
        ("stdout", output.stdout.as_slice()),
        ("stderr", output.stderr.as_slice()),
    ] {
        let rendered = String::from_utf8_lossy(bytes);
        assert!(
            !rendered.contains(sentinel),
            "{stream} leaked Action argument value:\n{rendered}"
        );
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn action_mode_redacts_deadlock_cross_and_doctor_process_output() {
    let temp = tempdir().expect("tempdir");
    let output_dir = path_text(temp.path());
    let server = format!("no-such-action-binary --token {SENTINEL}");

    let deadlock = run_cli(&[
        "deadlock-probe",
        "--server",
        &server,
        "--concurrent",
        "1",
        "--hang-threshold",
        "1ms",
        "--grace-period",
        "1ms",
        "--output-dir",
        &output_dir,
        "--action-redact-server-identity",
    ]);
    assert_redacted(&deadlock, &server);

    let cross = run_cli(&[
        "cross",
        "--server",
        &server,
        "--duration",
        "1ms",
        "--output-dir",
        &output_dir,
        "--action-redact-server-identity",
    ]);
    assert_redacted(&cross, &server);

    let doctor = run_cli(&[
        "doctor",
        "--server",
        &server,
        "--runs-dir",
        &output_dir,
        "--action-redact-server-identity",
    ]);
    assert_redacted(&doctor, &server);
}

#[test]
fn action_mode_redacts_malformed_tool_args_from_parse_errors() {
    let temp = tempdir().expect("tempdir");
    let output_dir = path_text(temp.path());

    let deadlock = run_cli(&[
        "deadlock-probe",
        "--server",
        "safe-unused-command",
        "--args",
        ARGS_SENTINEL,
        "--output-dir",
        &output_dir,
        "--action-redact-server-identity",
    ]);
    assert_no_sentinel(&deadlock, ARGS_SENTINEL);

    let cross = run_cli(&[
        "cross",
        "--server",
        "safe-unused-command",
        "--args",
        ARGS_SENTINEL,
        "--duration",
        "1ms",
        "--output-dir",
        &output_dir,
        "--action-redact-server-identity",
    ]);
    assert_no_sentinel(&cross, ARGS_SENTINEL);
}
