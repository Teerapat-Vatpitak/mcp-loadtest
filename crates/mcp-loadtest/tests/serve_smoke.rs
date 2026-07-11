//! Smoke test for `mcp-loadtest serve --mcp`.
//!
//! Spawns the CLI binary as a subprocess, pipes `initialize` + `tools/list`
//! into its stdin, reads two response lines from stdout, and asserts the
//! shape matches the MCP spec. Mirrors `happy_path.rs` but with
//! mcp-loadtest's own binary as the server.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Locate the built CLI binary. We rely on `cargo test` having already
/// produced it via the workspace dependency graph (the CLI crate depends on
/// the lib, so test-builds include both). Failing that, we fall back to a
/// best-effort `cargo build` once per test process.
fn cli_binary_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is populated when the test target is in the same
    // crate as the bin. Cross-crate it isn't, so we search both common debug
    // dirs.
    let exe_name = if cfg!(windows) {
        "mcp-loadtest.exe"
    } else {
        "mcp-loadtest"
    };
    let candidates = [
        // workspace target dir from a `cargo test --workspace` invocation
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("debug")
            .join(exe_name),
        // release fallback
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("release")
            .join(exe_name),
    ];
    for c in &candidates {
        if c.is_file() {
            return c.clone();
        }
    }
    // Last resort: best-effort build. Quiet so we don't pollute test output
    // unless something goes very wrong.
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "mcp-loadtest-cli", "--bin", "mcp-loadtest"])
        .current_dir(&workspace_root)
        .status()
        .expect("cargo build for serve_smoke fallback");
    assert!(status.success(), "fallback cargo build failed");
    candidates[0].clone()
}

#[tokio::test]
async fn serve_mcp_responds_to_initialize_and_tools_list() {
    let bin = cli_binary_path();
    let mut child = Command::new(&bin)
        .args(["serve", "--mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mcp-loadtest serve --mcp");

    let mut stdin = child.stdin.take().expect("child has no stdin");
    let stdout = child.stdout.take().expect("child has no stdout");
    let mut reader = BufReader::new(stdout);

    // Send initialize + tools/list.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    stdin.write_all(init.as_bytes()).await.unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.write_all(list.as_bytes()).await.unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.flush().await.unwrap();

    // Read two response lines with a generous outer timeout — the server
    // shouldn't take more than a few ms to answer, but cold spawn on
    // Windows can add 1s+.
    let mut line1 = String::new();
    let n1 = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line1))
        .await
        .expect("first response timed out")
        .expect("read first response");
    assert!(n1 > 0, "got EOF before first response");

    let mut line2 = String::new();
    let n2 = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line2))
        .await
        .expect("second response timed out")
        .expect("read second response");
    assert!(n2 > 0, "got EOF before second response");

    let resp1: serde_json::Value =
        serde_json::from_str(line1.trim()).expect("first response is JSON");
    let resp2: serde_json::Value =
        serde_json::from_str(line2.trim()).expect("second response is JSON");

    // Assertions: initialize shape.
    assert_eq!(resp1["jsonrpc"], "2.0");
    assert_eq!(resp1["id"], 1);
    assert!(
        resp1["result"]["protocolVersion"].is_string(),
        "initialize response must carry result.protocolVersion: {resp1}"
    );

    // Assertions: tools/list shape.
    assert_eq!(resp2["jsonrpc"], "2.0");
    assert_eq!(resp2["id"], 2);
    let tools = resp2["result"]["tools"]
        .as_array()
        .expect("tools/list result.tools must be an array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        names.contains(&"deadlock_probe"),
        "tools must include deadlock_probe; got {names:?}"
    );
    assert!(
        names.contains(&"sustained_load"),
        "tools must include sustained_load; got {names:?}"
    );

    // Clean shutdown by dropping stdin → EOF for the child loop.
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
