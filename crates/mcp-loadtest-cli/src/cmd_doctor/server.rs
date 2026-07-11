//! doctor check (b): the MCP server completes the `initialize` handshake.
//!
//! Only runs when `--server` is given (otherwise it is a trivial pass — we
//! have nothing to probe). Spawns the server with its stderr captured to a
//! unique temp file, bounds the handshake with a timeout, and on failure
//! surfaces the tail of the captured stderr so the user sees *why* it died
//! (DESIGN §21.3 / §21.6).

use std::time::Duration;

use mcp_loadtest::config::split_server_command;
use mcp_loadtest::{Session, SpawnOptions};

use super::CheckResult;

const NAME: &str = "server initialize smoke";
const FIX: &str = "the server failed to initialize — see the captured stderr above; \
                    verify the command and that its dependencies are installed";
/// Handshake budget. Slightly above `Session`'s own 10s startup timeout so a
/// server that *just* makes it still counts as a pass rather than a doctor
/// timeout race.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(12);
/// How many trailing stderr lines to echo on failure.
const STDERR_TAIL_LINES: usize = 20;

/// Run the initialize smoke against `server` (a shell-style command string).
/// `None` → nothing to probe → pass.
pub(super) async fn check(server: Option<&str>) -> CheckResult {
    let Some(server) = server else {
        return CheckResult::pass(NAME, "skipped (no --server given)");
    };

    let (command, args) = match split_server_command(server) {
        Ok(parts) => parts,
        Err(e) => {
            return CheckResult::fail(
                NAME,
                format!("could not parse --server `{server}`: {e}"),
                FIX,
            );
        }
    };

    // Unique temp file for the captured stderr; cleaned up before we return.
    let stderr_path = std::env::temp_dir().join(format!(
        "mcp-loadtest-doctor-{}-{}.stderr.log",
        std::process::id(),
        ulid_like()
    ));
    let opts = SpawnOptions::capture_stderr(&stderr_path);

    let outcome =
        tokio::time::timeout(SMOKE_TIMEOUT, Session::spawn_with(&command, args, opts)).await;

    let result = match outcome {
        Ok(Ok(session)) => {
            // Handshake succeeded. Close it cleanly; a shutdown hiccup is not
            // an initialize failure, so it doesn't fail the check.
            let _ = session.shutdown().await;
            CheckResult::pass(NAME, format!("`{server}` completed initialize"))
        }
        Ok(Err(e)) => {
            let tail = read_stderr_tail(&stderr_path).await;
            CheckResult::fail(NAME, format!("initialize failed: {e}{tail}"), FIX)
        }
        Err(_) => {
            let tail = read_stderr_tail(&stderr_path).await;
            CheckResult::fail(
                NAME,
                format!("server did not initialize within {SMOKE_TIMEOUT:?}{tail}"),
                FIX,
            )
        }
    };

    // Best-effort cleanup of the temp capture file.
    let _ = tokio::fs::remove_file(&stderr_path).await;
    result
}

/// Read the last [`STDERR_TAIL_LINES`] lines of the capture file, formatted
/// as an indented block to append to the failure detail. Best-effort: a
/// missing/empty file (server died before writing) yields an empty string.
async fn read_stderr_tail(path: &std::path::Path) -> String {
    let Ok(content) = tokio::fs::read_to_string(path).await else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    let tail = lines[start..]
        .iter()
        .map(|l| format!("\n     | {l}"))
        .collect::<String>();
    format!("\n   captured stderr (last {STDERR_TAIL_LINES} lines):{tail}")
}

/// Tiny monotonic-ish uniqueness suffix for the temp filename (avoids pulling
/// the server-side `ulid` crate into this CLI module just for a temp name).
fn ulid_like() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_server_is_a_skip_pass() {
        let r = check(None).await;
        assert!(r.ok);
        assert!(r.detail.contains("skipped"));
    }

    #[tokio::test]
    async fn missing_server_binary_fails_with_fix() {
        let r = check(Some("no-such-binary-zzzz-doctor")).await;
        assert!(!r.ok, "a missing server binary must fail the smoke");
        assert!(r.fix.is_some());
        assert!(r.detail.contains("initialize failed"));
    }

    #[tokio::test]
    async fn unparseable_server_string_fails() {
        let r = check(Some("   ")).await;
        assert!(!r.ok);
        assert!(r.detail.contains("could not parse"));
    }
}
