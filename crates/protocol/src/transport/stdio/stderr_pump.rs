//! Background pump that drains a spawned child's stderr into a per-run file
//! (and optionally mirrors it live to the parent's stderr).
//!
//! Split out of `stdio.rs` so that file stays under the 300-line production
//! convention (stdio.rs was already 189 lines before Feature 2). Declared as a
//! private child module of `stdio` via `#[path = "stderr_pump.rs"]` so that
//! `transport/mod.rs` (owned by another agent) doesn't need a new `pub mod`
//! line — see ADR 0013.
//!
//! Lifecycle (mirrors the `sse`/`ws` reader-task pattern): the task is owned by
//! a `JoinHandle` stored on `StdioTransport`, and a `CancellationToken` lets
//! `shutdown` / `Drop` stop it deterministically. All I/O here is best-effort:
//! a write failure (closed pipe during teardown) must not poison shutdown, so
//! errors are swallowed. The file is **flushed before every exit path**
//! (EOF / cancel / read error) — otherwise the last buffered line is lost.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStderr;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Spawn the stderr pump. Reads `stderr` line-by-line, appending each line
/// (newline-terminated) to `file`; when `tee` is true the same line is also
/// written to the parent process's stderr.
///
/// The returned `JoinHandle` is stored on `StdioTransport` (mirrors the
/// `ws`/`sse` reader task): `shutdown` cancels + awaits it so the final
/// `flush` runs; `Drop` cancels + `abort`s it as a backstop.
pub(super) fn spawn_stderr_pump(
    stderr: ChildStderr,
    mut file: tokio::fs::File,
    tee: bool,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // Cancelled (shutdown / Drop): flush what we have so the
                    // captured file isn't missing its tail, then stop.
                    let _ = file.flush().await;
                    break;
                }
                r = lines.next_line() => match r {
                    Ok(Some(line)) => {
                        // Best-effort: a closed file mid-teardown must not
                        // panic or wedge the pump.
                        let _ = file.write_all(line.as_bytes()).await;
                        let _ = file.write_all(b"\n").await;
                        if tee {
                            let mut err = tokio::io::stderr();
                            let _ = err.write_all(line.as_bytes()).await;
                            let _ = err.write_all(b"\n").await;
                        }
                    }
                    // EOF: child closed stderr (usually because it exited).
                    // Flush before breaking or the last line is lost.
                    Ok(None) => {
                        let _ = file.flush().await;
                        break;
                    }
                    // Read error (e.g. pipe broke): same flush-then-exit.
                    Err(_) => {
                        let _ = file.flush().await;
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Unique temp path; we only need a *path* a child could write — here we
    /// drive the pump's file half directly via a real `ChildStderr` from a
    /// short-lived child process so the EOF→flush path is exercised end-to-end.
    fn temp_log(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mcp-loadtest-pump-{tag}-{}-{nanos}.log",
            std::process::id()
        ))
    }

    /// Spawn a child whose only job is to print two lines to stderr and exit.
    /// Asserts the pump captured both lines and flushed them on EOF. Uses the
    /// platform shell so the test doesn't depend on Python being present.
    #[tokio::test]
    async fn captures_child_stderr_and_flushes_on_eof() {
        let log = temp_log("eof");
        let file = tokio::fs::File::create(&log)
            .await
            .expect("create temp log");

        #[cfg(windows)]
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "echo line-one 1>&2 & echo line-two 1>&2"])
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn child");

        #[cfg(not(windows))]
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "echo line-one 1>&2; echo line-two 1>&2"])
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn child");

        let stderr = child.stderr.take().expect("child stderr piped");
        let cancel = CancellationToken::new();
        let handle = spawn_stderr_pump(stderr, file, false, cancel.clone());

        // Child exits quickly → stderr hits EOF → pump flushes & returns.
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("pump did not finish after child EOF")
            .expect("pump task panicked");

        let contents = tokio::fs::read_to_string(&log)
            .await
            .expect("read captured log");
        let _ = tokio::fs::remove_file(&log).await;

        assert!(
            contents.contains("line-one") && contents.contains("line-two"),
            "captured log should contain both stderr lines, got: {contents:?}"
        );
    }

    /// Python interpreter, overridable via `MCP_LOADTEST_PYTHON` (repo
    /// convention — mirrors `tests/helpers::python()`). Used for a child whose
    /// stderr/lifetime we can shape precisely.
    fn python() -> String {
        std::env::var("MCP_LOADTEST_PYTHON").unwrap_or_else(|_| "python".to_string())
    }

    /// Cancelling the token must stop the pump **promptly** even while the
    /// source is still alive (the shutdown / Drop path), with the captured
    /// file left intact.
    ///
    /// The child writes one stderr line, flushes, then sleeps 30s without
    /// emitting more — so the pump stays genuinely blocked in `next_line()`
    /// (not at EOF) the whole time. Rather than a fixed sleep before
    /// cancelling (racy: on a slow/loaded CI runner the child + interpreter
    /// may not have emitted the line inside a fixed window, leaving the file
    /// empty at cancel — the original macOS/Windows CI flake), we **poll the
    /// file until the line is observed**. `tokio::fs::File` is unbuffered, so
    /// the line is visible the instant the pump's read arm writes it. Only
    /// then do we cancel and assert the `JoinHandle` resolves far inside the
    /// child's 30s lifetime (a broken cancel would block ~30s). The child is
    /// reaped explicitly (`start_kill` + bounded `wait`) so the runtime never
    /// blocks on its sleep at teardown (that original bug made this ~59s).
    #[tokio::test]
    async fn cancel_stops_pump_while_child_alive() {
        let log = temp_log("cancel");
        let file = tokio::fs::File::create(&log)
            .await
            .expect("create temp log");

        // Stdlib-only: emit one stderr line + flush, then stay alive (no more
        // output) for 30s. Cross-platform via the resolved interpreter.
        let mut child = tokio::process::Command::new(python())
            .args([
                "-c",
                "import sys,time; sys.stderr.write('early\\n'); \
                 sys.stderr.flush(); time.sleep(30)",
            ])
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn child");

        let stderr = child.stderr.take().expect("child stderr piped");
        let cancel = CancellationToken::new();
        let handle = spawn_stderr_pump(stderr, file, false, cancel.clone());

        // Deterministically wait until the pump has actually read+written the
        // line (child still sleeping → no EOF; this proves the alive-source
        // read path). Generous bound tolerates slow-CI process startup;
        // normally completes in well under 100ms.
        let observed = {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if tokio::fs::read_to_string(&log)
                    .await
                    .map(|c| c.contains("early"))
                    .unwrap_or(false)
                {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        assert!(
            observed,
            "pump never wrote the child's stderr line to the file \
             (real pump bug, not a timing flake)"
        );

        // Source is still alive (child sleeping 30s). Cancelling must stop the
        // pump promptly: a broken cancel would leave the handle pending until
        // the child's 30s sleep ends, so 5s is unambiguous yet CI-tolerant.
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("cancel must stop the pump within 5s (source still alive)")
            .expect("pump task panicked");

        // The captured line survived the cancel/flush exit path.
        let contents = tokio::fs::read_to_string(&log)
            .await
            .expect("read captured log");
        assert!(
            contents.contains("early"),
            "cancel path must preserve the captured line, got: {contents:?}"
        );

        // Reap the still-sleeping child now (don't leave it for the runtime to
        // block on at drop — that was the original 59s bug). `start_kill` is
        // async-signal-only; the bounded `wait` actually collects it.
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        let _ = tokio::fs::remove_file(&log).await;
    }
}
