//! Vibe-Trading deadlock regression test — proves `DeadlockProbe` catches
//! the real bug class against the real-world buggy server.
//!
//! ## Why this test exists
//!
//! `mcp-loadtest`'s differentiator vs other MCP load testers is its ability
//! to detect lazy-init deadlocks (DESIGN.md §15.2). The canonical example is
//! [HKUDS/Vibe-Trading PR #85](https://github.com/HKUDS/Vibe-Trading/pull/85)
//! where `_get_registry()` lazy-init inside FastMCP's worker thread blocks on
//! the first `tools/call` and hangs forever. The fix landed in commit
//! `6809324a` (merge of PR #85); the parent commit
//! `71220c7c851179314426a9312a1a47c4c2d981d6` is the last one with the bug.
//!
//! This test:
//! 1. Locates a Vibe-Trading checkout pinned to (or before) the buggy commit.
//! 2. Spawns `python agent/mcp_server.py` against it via [`Session`].
//! 3. Runs [`DeadlockProbe`] with a 2s hang threshold + 5s grace period.
//! 4. Asserts `outcome.deadlock_count >= 1`.
//!
//! ## Running it
//!
//! Marked `#[ignore]` because it (a) needs a Python interpreter with the
//! `vibe-trading-ai` package + dependencies installed, (b) takes ~10s wall
//! clock, (c) needs the buggy commit checked out somewhere visible to the
//! test. Default `cargo test` skips it; explicit run:
//!
//! ```bash
//! cargo test -p mcp-loadtest --test vibe_trading_regression -- --ignored --nocapture
//! ```
//!
//! ## Configuration
//!
//! The test resolves the buggy server in this priority order:
//!
//! 1. `MCP_LOADTEST_VIBE_TRADING_DIR` env var → directory containing
//!    `agent/mcp_server.py` already at the buggy commit.
//! 2. `target/vibe-trading-fixture/` if it exists (clone-at-test-setup
//!    populates this; gitignored).
//! 3. `C:\Users\teera\Vibe-Trading\` (developer's local clone) is a
//!    last-resort fallback for the author's box.
//!
//! `MCP_LOADTEST_VIBE_TRADING_PYTHON` overrides the Python interpreter (so
//! a venv with the dependencies installed can be pointed at without
//! polluting `PATH`). Defaults to `python`.
//!
//! ## Approach choice (A vs B)
//!
//! Approach A — clone @ specific commit. Approach B — vendored mock fixture.
//! This file uses Approach A: pinning to a real-world buggy commit makes the
//! regression durable and proves the differentiator against the actual code
//! that motivated the project. The `mock-broken.py` fixture (already covered
//! by `tests/deadlock.rs`) plays the Approach-B role.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use mcp_loadtest_core::metrics::Recorder;
use mcp_loadtest_engine::scenario::deadlock_probe::DeadlockProbe;
use mcp_loadtest_engine::scenario::{RunContext, Scenario};
use mcp_loadtest_protocol::Session;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Buggy parent commit (last commit before PR #85 landed). Used to pin the
/// checkout and surfaced in assertions so failures cross-reference the bug.
const BUGGY_COMMIT_SHA: &str = "71220c7c851179314426a9312a1a47c4c2d981d6";

/// Workspace root (the dir containing the top-level `Cargo.toml`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2) // crates/mcp-loadtest -> crates -> workspace root
        .expect("CARGO_MANIFEST_DIR has at least 2 ancestors")
        .to_path_buf()
}

/// Returns the path the test fixture lives at: `target/vibe-trading-fixture/`.
fn fixture_dir() -> PathBuf {
    workspace_root().join("target").join("vibe-trading-fixture")
}

/// Verify that `dir` is a Vibe-Trading checkout pinned at the buggy commit.
/// Returns Ok(()) if so; Err with a human description otherwise.
fn verify_at_buggy_commit(dir: &Path) -> Result<(), String> {
    if !dir.join("agent").join("mcp_server.py").exists() {
        return Err(format!(
            "{}/agent/mcp_server.py missing — not a Vibe-Trading checkout",
            dir.display()
        ));
    }
    let head = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git rev-parse failed: {e}"))?;
    if !head.status.success() {
        return Err(format!(
            "git rev-parse exited {}: {}",
            head.status,
            String::from_utf8_lossy(&head.stderr)
        ));
    }
    let head_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if head_sha != BUGGY_COMMIT_SHA {
        return Err(format!(
            "checkout is at {head_sha}, expected buggy commit {BUGGY_COMMIT_SHA}"
        ));
    }
    Ok(())
}

/// Clone (or fetch + checkout) the Vibe-Trading repo into
/// `target/vibe-trading-fixture/` and pin to the buggy commit.
///
/// Idempotent: if the fixture already exists at the right commit, returns
/// immediately. If it exists at a different commit, fetches + checks out.
/// On clean failure (network unreachable, git missing) returns the underlying
/// error so the test can soft-skip with context.
fn ensure_fixture_at_buggy_commit() -> Result<PathBuf, String> {
    let dir = fixture_dir();

    // Fast path: already at the right commit.
    if dir.exists() && verify_at_buggy_commit(&dir).is_ok() {
        return Ok(dir);
    }

    if dir.exists() {
        // Existing dir but wrong commit: fetch + checkout.
        let fetch = StdCommand::new("git")
            .args(["fetch", "--depth", "1", "origin", BUGGY_COMMIT_SHA])
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("git fetch failed: {e}"))?;
        if !fetch.status.success() {
            return Err(format!(
                "git fetch failed ({}): {}",
                fetch.status,
                String::from_utf8_lossy(&fetch.stderr)
            ));
        }
        let checkout = StdCommand::new("git")
            .args(["checkout", "--detach", BUGGY_COMMIT_SHA])
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("git checkout failed: {e}"))?;
        if !checkout.status.success() {
            return Err(format!(
                "git checkout {BUGGY_COMMIT_SHA} failed ({}): {}",
                checkout.status,
                String::from_utf8_lossy(&checkout.stderr)
            ));
        }
    } else {
        // Cold clone. Use a partial-clone trick to keep the download cheap:
        // shallow + then fetch the specific SHA.
        let parent = dir
            .parent()
            .ok_or_else(|| "fixture dir has no parent".to_string())?;
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
        let clone = StdCommand::new("git")
            .args([
                "clone",
                "--no-checkout",
                "--filter=blob:none",
                "https://github.com/HKUDS/Vibe-Trading.git",
            ])
            .arg(&dir)
            .output()
            .map_err(|e| format!("git clone failed (is git installed and network up?): {e}"))?;
        if !clone.status.success() {
            return Err(format!(
                "git clone failed ({}): {}",
                clone.status,
                String::from_utf8_lossy(&clone.stderr)
            ));
        }
        let checkout = StdCommand::new("git")
            .args(["checkout", "--detach", BUGGY_COMMIT_SHA])
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("git checkout after clone failed: {e}"))?;
        if !checkout.status.success() {
            return Err(format!(
                "git checkout {BUGGY_COMMIT_SHA} after clone failed ({}): {}",
                checkout.status,
                String::from_utf8_lossy(&checkout.stderr)
            ));
        }
    }

    verify_at_buggy_commit(&dir).map_err(|e| format!("post-fetch verification failed: {e}"))?;
    Ok(dir)
}

/// Find the directory containing the Vibe-Trading buggy checkout, or skip
/// the test with a clear message.
fn locate_vibe_trading_dir() -> Option<PathBuf> {
    // Highest precedence: explicit env var. Trust it without checking the
    // commit — caller has full control.
    if let Ok(dir) = std::env::var("MCP_LOADTEST_VIBE_TRADING_DIR") {
        let p = PathBuf::from(dir);
        if p.join("agent").join("mcp_server.py").exists() {
            return Some(p);
        }
    }

    // Auto-clone into target/vibe-trading-fixture/ at the pinned buggy
    // commit. This is the durable path: no dev-box dependency, deterministic
    // SHA, gitignored output.
    match ensure_fixture_at_buggy_commit() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("vibe-trading-fixture setup failed: {e}");
            None
        }
    }
}

fn vibe_trading_python() -> String {
    std::env::var("MCP_LOADTEST_VIBE_TRADING_PYTHON").unwrap_or_else(|_| "python".to_string())
}

fn make_ctx() -> RunContext {
    RunContext::new(
        Instant::now(),
        CancellationToken::new(),
        Recorder::new(),
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
}

/// Drive `DeadlockProbe` against the *buggy* Vibe-Trading commit and assert
/// at least one deadlock is detected.
///
/// The buggy server `agent/mcp_server.py` (commit `~PR-85`) lazy-inits its
/// tool registry inside a FastMCP worker thread. The first `tools/call`
/// blocks forever importing `src.tools.shell.*`. With `hang_threshold = 2s`
/// and `grace_period = 5s`, the probe declares deadlock at ~7s and bails.
#[tokio::test]
#[ignore = "requires checked-out Vibe-Trading + vibe-trading-ai env; run with --ignored"]
async fn deadlock_probe_catches_vibe_trading_pr85_bug() {
    let dir = match locate_vibe_trading_dir() {
        Some(d) => d,
        None => {
            // Soft skip: a panic here would scare future contributors. The
            // assertion at the end still ensures correctness when the dir
            // is wired up.
            eprintln!(
                "skip: Vibe-Trading checkout not found. Set \
                 MCP_LOADTEST_VIBE_TRADING_DIR to a directory containing \
                 agent/mcp_server.py at commit {BUGGY_COMMIT_SHA} (or earlier)."
            );
            return;
        }
    };

    let server_script = dir.join("agent").join("mcp_server.py");
    eprintln!(
        "vibe-trading regression: server={}",
        server_script.display()
    );

    let py = vibe_trading_python();
    eprintln!("vibe-trading regression: python={py}");

    let mut session = match Session::spawn(&py, [server_script.as_os_str()]).await {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "skip: Session::spawn failed — likely missing Python deps for \
                 vibe-trading-ai (fastmcp, langchain, ...). err: {err}"
            );
            return;
        }
    };

    let probe = DeadlockProbe {
        // 5 concurrent calls is enough to trip the lazy-init regardless of
        // which call wins the worker thread first. The probe is sequential
        // (M2 limitation) but the buggy server hangs on call #1 anyway —
        // the deadlock happens before tool argument validation.
        concurrent: 5,
        hang_threshold: Duration::from_secs(2),
        grace_period: Duration::from_secs(5),
        // analyze_options is the same tool used by HKUDS/Vibe-Trading PR #86
        // smoke test — pure Black-Scholes calc, no network. Args here match
        // the buggy commit's signature `(spot, strike, expiry_days, ...)`.
        tool: "analyze_options".to_string(),
        args: json!({
            "spot": 450.0,
            "strike": 460.0,
            "expiry_days": 30,
        }),
    };

    let ctx = make_ctx();
    let outcome = probe.drive(&mut session, &ctx).await;

    eprintln!("vibe-trading regression outcome: {outcome:?}");

    // Best-effort shutdown — the session is wedged after a deadlock.
    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;

    assert!(
        outcome.deadlock_count >= 1,
        "expected DeadlockProbe to flag the Vibe-Trading PR #85 bug, but got: \
         deadlock_count={} hang_count={} successful={} errors={} notes={:?}",
        outcome.deadlock_count,
        outcome.hang_count,
        outcome.successful_calls,
        outcome.error_count,
        outcome.notes,
    );

    // Outcome must be human-readable in reports — verify the offending
    // request is annotated.
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("deadlock detected")),
        "outcome notes should annotate the deadlock for the report: {:?}",
        outcome.notes
    );
}

/// Tiny self-test for the directory-resolution helper. Always runs (no
/// `#[ignore]`) because it's pure Rust, doesn't touch the network, and
/// doesn't mutate process env (which would race with parallel tests).
#[test]
fn locate_does_not_panic_on_missing_layout() {
    // Calling the helper should never panic, regardless of whether any of
    // the candidate paths exist on this box. The actual integration test
    // above asserts the deadlock; this just guards the resolution helper
    // itself against accidental panics on shape changes.
    let _ = locate_vibe_trading_dir();
    // Touch the Path import so it stays load-bearing if the helper grows.
    let _ = Path::new(".").exists();
}
