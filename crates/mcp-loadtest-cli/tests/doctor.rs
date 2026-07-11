//! Integration tests for Feature 3 (DESIGN §21): `doctor`, `--explain`, and
//! actionable error hints.
//!
//! The CLI crate has no `assert_cmd` dev-dependency (and this crate `forbid`s
//! `unsafe`, so `std::env::set_var` — `unsafe` on edition 2024 — can't be
//! used to inject args either), so these drive the **library entrypoints**
//! directly: `cmd_doctor::run_doctor`, `explain::maybe_handle_explain`, and
//! the `hints::ErrorHint` impls. The one behaviour that genuinely needs a
//! spawned binary (`run --explain` exiting 0 with no `--config`, proving the
//! pre-clap scan beats clap's required-arg enforcement) is verified by a
//! manual binary smoke and recorded in ADR 0014 / the Feature 3 report.

use std::time::Duration;

use mcp_loadtest::{SessionError, TransportError};
use mcp_loadtest_cli::cmd_doctor;
use mcp_loadtest_cli::explain;
use mcp_loadtest_cli::hints::ErrorHint;
use tempfile::tempdir;

// ---- hints ---------------------------------------------------------------

#[test]
fn startup_timeout_has_hint() {
    let h = SessionError::StartupTimeout(Duration::from_secs(10))
        .hint()
        .expect("startup timeout should carry an actionable hint");
    assert!(h.contains("initialize"));
    assert!(h.contains("server-stderr.log") || h.contains("--tee-stderr"));
}

#[test]
fn transport_closed_has_hint() {
    let h = TransportError::Closed
        .hint()
        .expect("a closed transport should carry a hint");
    assert!(h.contains("--capture-stderr"));
}

#[test]
fn blocked_host_other_maps_to_ssrf_hint() {
    // Mirrors F1's stable rejection message shape.
    let e = TransportError::Other(
        "blocked host `169.254.169.254`: link-local address (SSRF guard, ADR 0012)".into(),
    );
    let h = e
        .hint()
        .expect("a blocked-host Other(_) must map to the SSRF hint");
    assert!(
        h.contains("allowed_hosts"),
        "SSRF hint should point at `[server].allowed_hosts`, got: {h}"
    );
}

#[test]
fn toml_error_has_hint() {
    // The CLI crate doesn't depend on `toml` directly; route a real
    // `ConfigError::Toml` through the library's own parser instead.
    let e =
        mcp_loadtest::Config::from_toml_str("a = = b").expect_err("broken TOML must fail to parse");
    assert!(
        matches!(e, mcp_loadtest::ConfigError::Toml(_)),
        "expected ConfigError::Toml, got {e:?}"
    );
    let h = e.hint().expect("a TOML parse error should carry a hint");
    assert!(h.contains("example-config"));
}

// ---- --explain -----------------------------------------------------------

#[test]
fn maybe_handle_explain_is_false_without_the_flag() {
    // The test harness's own argv has no `--explain`, so the scan must be
    // inert (return false, print nothing) and let normal parsing proceed.
    assert!(
        !explain::maybe_handle_explain(),
        "no --explain in argv → scan must be a no-op"
    );
}

// (The "prints text + returns true when --explain is present" half and the
// per-subcommand text landmarks — including the verbatim DESIGN §21.4
// `deadlock-probe` block — are covered by the in-crate unit tests in
// `src/explain.rs` against the pure `explanation_for`, which needs no argv
// manipulation.)

// ---- doctor --------------------------------------------------------------

#[tokio::test]
async fn doctor_passes_on_clean_env() {
    // Python is on PATH in dev/CI; no --server; fresh empty runs dir.
    let tmp = tempdir().expect("tempdir");
    let res = cmd_doctor::run_doctor(None, tmp.path().to_path_buf()).await;
    assert!(res.is_ok(), "doctor should pass on a clean env: {res:?}");
}

#[tokio::test]
async fn doctor_fails_on_missing_server_with_no_spurious_hint() {
    let tmp = tempdir().expect("tempdir");
    let res = cmd_doctor::run_doctor(
        Some("no-such-binary-zzzz-doctor-itest".to_string()),
        tmp.path().to_path_buf(),
    )
    .await;
    let err = res.expect_err("a missing --server binary must fail doctor (non-zero exit)");

    // Summary is a *bare* anyhow message, not a typed library error — so the
    // top-level `print_with_hint` adds NO spurious `Hint:` to the checklist.
    assert!(
        err.to_string().contains("doctor:"),
        "expected the doctor summary message, got: {err}"
    );
    assert!(
        err.downcast_ref::<mcp_loadtest::RunError>().is_none(),
        "doctor summary must not be a typed RunError"
    );
    assert!(
        err.downcast_ref::<SessionError>().is_none(),
        "doctor summary must not be a typed SessionError"
    );
}

#[tokio::test]
async fn doctor_flags_stale_runs_accumulation() {
    let tmp = tempdir().expect("tempdir");
    let runs = tmp.path().join("runs");
    tokio::fs::create_dir(&runs).await.expect("mkdir runs");
    // > 50 fake run dirs → the stale-runs check must fail, which fails the
    // whole doctor run.
    for i in 0..60 {
        tokio::fs::create_dir(runs.join(format!("run-{i:03}")))
            .await
            .expect("mkdir fake run dir");
    }
    let res = cmd_doctor::run_doctor(None, runs).await;
    assert!(
        res.is_err(),
        "61 run dirs should trip the stale-runs check and fail doctor"
    );
}
