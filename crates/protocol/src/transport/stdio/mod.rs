//! Stdio transport — newline-delimited JSON-RPC over a child process's
//! stdin/stdout.
//!
//! Pre-M4 this lived inline in `crate::Session`; refactored here so HTTP and
//! SSE can share the same `Session` orchestration.

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Transport, TransportError};

// The spawn path (process spawn + stderr-mode wiring) lives in `spawn.rs`, the
// stderr pump in `stderr_pump.rs`, and the bounded line reader (L-2 OOM guard)
// in `reader.rs` — each a child module so this file stays under the 300-line
// production convention. See ADR 0013.
mod reader;
mod shutdown;
mod spawn;
mod stderr_pump;
use reader::read_bounded_line;

/// Default time budget applied around individual `request` calls when the
/// caller doesn't wrap them. Kept loose; finer control lives in the
/// scenario layer.
const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard cap on a single response line from the child. JSON-RPC has no spec
/// limit, but real MCP messages are < 1 MB; 16 MB leaves slack for pathological
/// servers while preventing a malicious server-under-test from OOM-ing the
/// load tester by emitting one unbounded line. On overflow we surface a
/// transport error rather than truncating silently.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Upper bound on notification frames skipped while waiting for one response
/// (see `request`). Generous — real servers emit a handful of notifications
/// per response at most — but finite, so a server that streams notifications
/// forever surfaces a transport error instead of wedging the read.
const MAX_SKIPPED_NOTIFICATIONS: usize = 1024;

/// True when `line` is a JSON-RPC *notification* frame: a JSON object carrying
/// a `method` and no top-level `id`. Responses always echo the request `id`;
/// notifications never do. Anything that isn't this exact shape — a response
/// (has `id`), a malformed object with neither, or invalid JSON — is left for
/// the session layer, so its `IdMismatch` / malformed handling is unchanged.
///
/// Fast path: a notification must mention `"method"`, and responses almost
/// never do, so a substring miss short-circuits before any parse.
fn is_notification_frame(line: &str) -> bool {
    if !line.contains("\"method\"") {
        return false;
    }
    #[derive(serde::Deserialize)]
    struct FrameProbe {
        #[serde(default)]
        id: Option<serde_json::Value>,
        #[serde(default)]
        method: Option<serde_json::Value>,
    }
    matches!(
        serde_json::from_str::<FrameProbe>(line),
        Ok(p) if p.id.is_none() && p.method.is_some()
    )
}

/// Stdio transport — owns the spawned child plus pipe handles.
///
/// `child` / `stdin` are `Option` only so `shutdown` can take them by value
/// for the EOF-then-`wait` teardown: the type implements [`Drop`] (stderr-pump
/// backstop) and a `Drop` type can't be destructured field-by-field. They are
/// `Some` for the whole live lifetime — the hot `request`/`notify` paths
/// `expect` them, which is infallible until `shutdown` consumes the transport.
pub struct StdioTransport {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    line_buf: String,
    /// Background task draining the child's stderr into a file (and optionally
    /// teeing to the parent's stderr). `None` when stderr is inherited (the
    /// default — no pump, zero overhead). Owned here so `shutdown`/`Drop` can
    /// stop it deterministically (mirrors the `ws`/`sse` reader task). The
    /// nested I/O result makes capture/read failures gate clean shutdown.
    stderr_pump: Option<JoinHandle<std::io::Result<()>>>,
    /// Cancels [`Self::stderr_pump`]. Always present so the field is uniform;
    /// a fresh token in the inherit case is simply never tripped.
    pump_cancel: CancellationToken,
}

impl StdioTransport {
    /// Sum of the internal graceful-exit, forced-reap, and stderr-pump phase
    /// budgets. Callers applying an outer timeout must add scheduling margin
    /// and keep their deadline strictly greater than this value.
    pub const SHUTDOWN_BUDGET: Duration = shutdown::SHUTDOWN_BUDGET;

    /// Borrow the live stdin. `None` is unreachable while the transport is in
    /// use (only `shutdown` takes it, consuming `self`); we still map it to a
    /// typed error instead of `expect()` to honour the no-panic-in-lib rule.
    fn stdin_mut(&mut self) -> Result<&mut ChildStdin, TransportError> {
        self.stdin.as_mut().ok_or(TransportError::Closed)
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        // Write framed line.
        let stdin = self.stdin_mut()?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        // Read the response line, with a wide default timeout so a wedged
        // server still surfaces eventually rather than hanging tests forever.
        //
        // A JSON-RPC server may interleave *notifications* (frames with a
        // `method` and no `id` — e.g. `notifications/tools/list_changed` or
        // progress updates) with responses at any time, including before the
        // `initialize` result. This transport is single-flight, so the next
        // line after a request isn't guaranteed to be its response: skip any
        // leading notification frames until the response arrives, rather than
        // mis-reading a notification and desyncing the stream for every
        // subsequent call. One `deadline` bounds the whole wait; a skip cap
        // stops a notification flood from wedging the read.
        //
        // Bound each read at `MAX_LINE_BYTES` (16 MB) via `read_bounded_line`
        // (`BufReader::read_line` has no byte cap) so a malicious or buggy
        // server-under-test can't OOM the load tester with one unbounded line.
        let deadline = tokio::time::Instant::now() + DEFAULT_RECV_TIMEOUT;
        for _ in 0..MAX_SKIPPED_NOTIFICATIONS {
            self.line_buf.clear();
            let read_fut = read_bounded_line(&mut self.stdout, &mut self.line_buf);
            let n = tokio::time::timeout_at(deadline, read_fut)
                .await
                .map_err(|_| TransportError::Timeout(DEFAULT_RECV_TIMEOUT))??;
            if n == 0 {
                return Err(TransportError::Closed);
            }
            let line = self.line_buf.trim_end();
            if is_notification_frame(line) {
                continue;
            }
            return Ok(line.to_string());
        }
        Err(TransportError::Other(format!(
            "server sent {MAX_SKIPPED_NOTIFICATIONS}+ consecutive notifications with no response"
        )))
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        let stdin = self.stdin_mut()?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn raw_send(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        // Write the caller's bytes *verbatim* — invalid UTF-8, truncated JSON
        // and other framing violations are the whole point (this is the
        // fuzzer's raw escape hatch), so we do no validation or escaping. A
        // single trailing newline delimits them as one (malformed) frame for a
        // line-framed peer. Unlike `request` we read nothing back: the wire may
        // now be desynced, so the caller treats the session as poisoned.
        let stdin = self.stdin_mut()?;
        stdin.write_all(bytes).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
        let mut this = self;
        shutdown::run(&mut this).await
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // Backstop when the caller skipped `shutdown`: cancel the pump and
        // abort the task. We can't `.await` in `Drop`, so the final flush is
        // best-effort via the pump's own cancel arm; `abort` guarantees no
        // orphaned task. `kill_on_drop(true)` requests child termination;
        // graceful `shutdown` is the path that also waits for exit/reap.
        // Idempotent: after `shutdown` the pump is already `None` and the
        // token already cancelled, so this is a cheap no-op there.
        self.pump_cancel.cancel();
        if let Some(handle) = self.stderr_pump.take() {
            handle.abort();
        }
    }
}

// `read_bounded_line` (the L-2 OOM guard) + its `push_utf8_lossy` helper live
// in `reader.rs`, a child module declared above, so this file stays under the
// 300-line production convention. See ADR 0013.

// Pure classifier tests — no process spawn, so they run on every platform
// (unlike the unix-gated loopback below).
#[cfg(test)]
mod notification_tests {
    use super::is_notification_frame;

    #[test]
    fn notification_without_id_is_skipped() {
        assert!(is_notification_frame(
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#
        ));
        assert!(is_notification_frame(
            r#"{"method":"notifications/progress","params":{"progress":1},"jsonrpc":"2.0"}"#
        ));
    }

    #[test]
    fn response_with_id_is_not_a_notification() {
        // Ok result, error result, and a result whose payload text happens to
        // contain the word "method" — all carry an `id`, so none are skipped.
        assert!(!is_notification_frame(
            r#"{"jsonrpc":"2.0","id":3,"result":{}}"#
        ));
        assert!(!is_notification_frame(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found"}}"#
        ));
        assert!(!is_notification_frame(
            r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"the method ran"}]}}"#
        ));
    }

    #[test]
    fn malformed_or_idless_response_is_left_for_the_session() {
        // Invalid JSON and an object with neither id nor method must NOT be
        // swallowed here — the session surfaces them as malformed.
        assert!(!is_notification_frame("not json at all"));
        assert!(!is_notification_frame(r#"{"jsonrpc":"2.0","result":{}}"#));
    }
}

// The only test in here is the unix-gated loopback, so gate the whole module
// — otherwise its `use` lines are unused imports on Windows under
// `-D warnings`.
#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use tokio::io::AsyncReadExt;

    // Loopback: `cat` copies stdin → stdout byte-for-byte (it uses raw
    // read/write syscalls on the pipe, no libc stdio buffering), so we can
    // read back exactly what `raw_send` wrote and assert the bytes are
    // verbatim plus a single trailing newline. Unix-only — `cat` isn't
    // guaranteed on Windows and the newline-framing contract is
    // platform-independent, so one platform is enough coverage.
    #[tokio::test]
    async fn raw_send_writes_verbatim_bytes_then_single_newline() {
        let no_args: [&OsStr; 0] = [];
        let mut t = StdioTransport::spawn("cat", no_args)
            .await
            .expect("spawn cat");

        // Deliberately neither valid UTF-8 nor valid JSON: raw_send must not
        // touch, escape, or reframe it.
        let payload: &[u8] = b"{\"broken\": \x00\xff not-json";
        t.raw_send(payload).await.expect("raw_send");

        let mut buf = vec![0u8; payload.len() + 1];
        tokio::time::timeout(Duration::from_secs(5), t.stdout.read_exact(&mut buf))
            .await
            .expect("cat echo timed out")
            .expect("read echoed bytes");

        let mut expected = payload.to_vec();
        expected.push(b'\n');
        assert_eq!(
            buf, expected,
            "raw_send must write bytes verbatim + exactly one newline"
        );
    }
}
