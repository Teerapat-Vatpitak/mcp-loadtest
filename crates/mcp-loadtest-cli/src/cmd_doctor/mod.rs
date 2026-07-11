//! `mcp-loadtest doctor` — diagnose common setup problems (DESIGN §21.6).
//!
//! Runs four best-effort checks and prints a ✅/❌ checklist with a one-line
//! fix per ❌, then exits non-zero if **any** check failed — exactly the
//! shape an LLM agent can chain into a fix-it loop. All checks are
//! independent and *all* run (no early return) so one ❌ doesn't mask the
//! others.
//!
//! Split across submodules to stay under the 300-line production cap and to
//! keep each check independently unit-testable:
//! - `python` — Python interpreter on PATH
//! - `server` — MCP server `initialize` smoke (only with `--server`)
//! - `runs` — stale `runs/` accumulation
//! - `toolchain` — Windows MSVC-vs-GNU mismatch
//!
//! See [ADR 0014](../../../docs/adr/0014-error-hints-explain-doctor.md).

use std::path::PathBuf;

use anyhow::Result;

mod python;
mod runs;
mod server;
mod toolchain;

/// Outcome of one diagnostic check.
///
/// `ok == false` renders as `❌ <name> — <detail>` followed by an indented
/// `fix:` line; `ok == true` renders as `✅ <name>` (with `detail` appended
/// when it carries useful context, e.g. the resolved Python version).
pub struct CheckResult {
    /// Stable short name shown on the checklist line.
    pub name: &'static str,
    /// Whether the check passed.
    pub ok: bool,
    /// Human-readable detail (the failure reason, or context on success).
    pub detail: String,
    /// One-line remediation, shown only when `ok == false`.
    pub fix: Option<&'static str>,
}

impl CheckResult {
    /// A passing check with optional context `detail`.
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A failing check with a `detail` reason and a one-line `fix`.
    fn fail(name: &'static str, detail: impl Into<String>, fix: &'static str) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
            fix: Some(fix),
        }
    }
}

/// Run all four diagnostics and print the checklist.
///
/// Returns `Err` (non-zero exit) if any check failed. The error is a **bare**
/// `anyhow::anyhow!` message — deliberately *not* a typed library error — so
/// the top-level `hints::print_with_hint` does not append a spurious `Hint:`
/// to the doctor summary (the per-❌ `fix:` lines are the actionable advice).
///
/// `server`, when `Some`, is a shell-style server command (`"python -m foo"`)
/// for the `initialize` smoke; omitted entirely when `None`. `runs_dir` is
/// the directory scanned for stale-run accumulation.
pub async fn run_doctor(server: Option<String>, runs_dir: PathBuf) -> Result<()> {
    let results = vec![
        python::check().await,
        server::check(server.as_deref()).await,
        runs::check(&runs_dir).await,
        toolchain::check().await,
    ];

    let mut failures = 0usize;
    for r in &results {
        if r.ok {
            if r.detail.is_empty() {
                println!("✅ {}", r.name);
            } else {
                println!("✅ {} — {}", r.name, r.detail);
            }
        } else {
            failures += 1;
            println!("❌ {} — {}", r.name, r.detail);
            if let Some(fix) = r.fix {
                println!("   fix: {fix}");
            }
        }
    }

    if failures == 0 {
        println!("\nAll checks passed — environment looks good.");
        Ok(())
    } else {
        // Bare message (no typed source) → print_with_hint adds no Hint:.
        Err(anyhow::anyhow!("doctor: {failures} check(s) failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passes_when_python_present_and_no_server() {
        // On this dev machine Python is on PATH and there is no --server, so
        // with a fresh (empty) runs dir all checks should pass.
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = run_doctor(None, tmp.path().to_path_buf()).await;
        assert!(res.is_ok(), "doctor should pass on a clean env: {res:?}");
    }

    #[tokio::test]
    async fn fails_and_reports_when_server_binary_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = run_doctor(
            Some("no-such-binary-zzzz-doctor-test".to_string()),
            tmp.path().to_path_buf(),
        )
        .await;
        let err = res.expect_err("a missing server binary must fail doctor");
        // Bare anyhow message — must look like the doctor summary, NOT a
        // typed library error. (The "no spurious Hint:" guarantee is proven
        // directly in `hints::tests::bare_anyhow_message_yields_no_hint`,
        // which exercises the same bare-`anyhow!` shape this returns.)
        assert!(err.to_string().contains("doctor:"));
        assert!(
            err.downcast_ref::<mcp_loadtest::RunError>().is_none(),
            "doctor summary must not be a typed RunError"
        );
    }

    #[test]
    fn check_result_constructors() {
        let p = CheckResult::pass("x", "ctx");
        assert!(p.ok && p.fix.is_none() && p.detail == "ctx");
        let f = CheckResult::fail("y", "boom", "do the thing");
        assert!(!f.ok && f.fix == Some("do the thing"));
    }
}
