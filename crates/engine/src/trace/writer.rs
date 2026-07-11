//! Append side of trace recording: [`TraceWriter`] + [`TracingTransport`]
//! (ADR 0021 §5).
//!
//! # Blocking-I/O tradeoff
//!
//! [`TraceWriter`] uses `std::fs` behind a `std::sync::Mutex<BufWriter<_>>`,
//! flushed once per frame, and is called from async transport methods. This
//! deliberately bends the house "no blocking I/O in async paths" rule: each
//! write is one line appended to a local file (a page-cache write,
//! microseconds), the lock is held for exactly one line, and the fully-async
//! alternative — a channel plus dedicated writer task — can silently lose
//! tail frames when the process aborts, which for a debugging artifact is
//! worse than the micro-stall. Recording failures never fail the run: the
//! writer warns once (via `tracing`) and drops subsequent failing frames.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;

use mcp_loadtest_protocol::transport::{Transport, TransportError};

use super::TraceError;
use super::format::{self, Direction, TraceFrame, TraceHeader};

/// Serialized `mcp-trace/1` JSONL writer. Shared via `Arc` between the run
/// orchestrator and every [`TracingTransport`] (including session-factory
/// respawns), so all frames of a run land in one file in append order.
pub struct TraceWriter {
    /// One line per frame; flushed per frame (see module docs).
    file: Mutex<BufWriter<std::fs::File>>,
    /// Reference point for every frame's `elapsed_ms`.
    start: Instant,
    /// Apply the default-on secret redaction to client→server frames.
    redact: bool,
    /// Warn-once latch for write failures.
    failed: AtomicBool,
    /// Where the trace is being written (diagnostics / `Report::trace_path`).
    path: PathBuf,
}

impl TraceWriter {
    /// Create (truncating) `path`, write the header line, and return a
    /// writer whose `elapsed_ms` values are measured from `start`.
    ///
    /// `redact` applies the default-**on** secret redaction (ADR 0021 §3) to
    /// client→server frames. Pass `false` only where the opt-out has been
    /// explicitly decided — CLI exposure of `--no-redact` is an open decision
    /// point, so `Run` always passes `true` today.
    pub fn create(
        path: &Path,
        header: &TraceHeader,
        start: Instant,
        redact: bool,
    ) -> Result<Self, TraceError> {
        let file = std::fs::File::create(path)?;
        let mut buf = BufWriter::new(file);
        let line = serde_json::to_string(header)?;
        buf.write_all(line.as_bytes())?;
        buf.write_all(b"\n")?;
        buf.flush()?;
        Ok(Self {
            file: Mutex::new(buf),
            start,
            redact,
            failed: AtomicBool::new(false),
            path: path.to_path_buf(),
        })
    }

    /// Where the trace is being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one frame. Best-effort by design: a failure warns once and
    /// never propagates — losing trace frames must not fail the run that is
    /// being recorded.
    pub fn record(&self, dir: Direction, method: Option<&str>, body: &str) {
        let body = if self.redact && dir == Direction::ClientToServer {
            format::redact_body(body)
        } else {
            std::borrow::Cow::Borrowed(body)
        };
        let frame = TraceFrame {
            dir,
            elapsed_ms: u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX),
            method: method.map(str::to_owned),
            body: body.into_owned(),
        };
        let line = match serde_json::to_string(&frame) {
            Ok(line) => line,
            Err(err) => {
                self.warn_once(&err.to_string());
                return;
            }
        };
        // A poisoned lock means another thread panicked mid-write; the file
        // stays line-consistent (whole lines only), so keep recording.
        let mut guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let result = guard
            .write_all(line.as_bytes())
            .and_then(|()| guard.write_all(b"\n"))
            .and_then(|()| guard.flush());
        if let Err(err) = result {
            drop(guard);
            self.warn_once(&err.to_string());
        }
    }

    fn warn_once(&self, err: &str) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "trace recording failed; further frames may be dropped"
            );
        }
    }
}

/// [`Transport`] decorator that records every request / notification body
/// and every response through a shared [`TraceWriter`], then delegates to
/// the wrapped transport. Constructed inside `Run`'s session spawn path when
/// [`crate::Run::with_trace`] is set (ADR 0021).
///
/// Failed requests record no server→client frame — there was no response.
pub struct TracingTransport<T> {
    inner: T,
    writer: Arc<TraceWriter>,
}

impl<T: Transport> TracingTransport<T> {
    /// Wrap `inner`, recording every frame through `writer`.
    pub fn new(inner: T, writer: Arc<TraceWriter>) -> Self {
        Self { inner, writer }
    }
}

#[async_trait]
impl<T: Transport> Transport for TracingTransport<T> {
    async fn request(&mut self, body: &str) -> Result<String, TransportError> {
        let method = format::method_of(body);
        self.writer
            .record(Direction::ClientToServer, method.as_deref(), body);
        let response = self.inner.request(body).await?;
        self.writer
            .record(Direction::ServerToClient, method.as_deref(), &response);
        Ok(response)
    }

    async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
        let method = format::method_of(body);
        self.writer
            .record(Direction::ClientToServer, method.as_deref(), body);
        self.inner.notify(body).await
    }

    async fn raw_send(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        // Delegated so a wrapped transport's override (stdio) still fires.
        // Deliberately NOT recorded: raw fuzzer payloads may be invalid
        // UTF-8 / non-JSON, and a lossy conversion would poison replay
        // (first-class raw frames are an additive mcp-trace/1 extension —
        // ADR 0021 §2).
        self.inner.raw_send(bytes).await
    }

    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn set_protocol_version(&mut self, version: &str) {
        self.inner.set_protocol_version(version);
    }

    async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
        let this = *self;
        Box::new(this.inner).shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::super::format::parse_trace;
    use super::*;

    /// Unique scratch file under the OS temp dir, removed on drop.
    struct ScratchFile(PathBuf);

    impl ScratchFile {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            Self(std::env::temp_dir().join(format!(
                "mcp-trace-writer-{tag}-{}-{nanos}.jsonl",
                std::process::id()
            )))
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn header() -> TraceHeader {
        TraceHeader::new("01TEST", "python mock.py", "2026-07-07T00:00:00Z")
    }

    #[test]
    fn writer_emits_parseable_header_and_frames() {
        let scratch = ScratchFile::new("basic");
        let w = TraceWriter::create(&scratch.0, &header(), Instant::now(), true).unwrap();
        w.record(
            Direction::ClientToServer,
            Some("tools/call"),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"hi"}}}"#,
        );
        w.record(
            Direction::ServerToClient,
            Some("tools/call"),
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#,
        );

        let text = std::fs::read_to_string(&scratch.0).unwrap();
        let (h, frames) = parse_trace(&text).unwrap();
        assert_eq!(h.run_id, "01TEST");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].dir, Direction::ClientToServer);
        assert_eq!(frames[0].method.as_deref(), Some("tools/call"));
        assert_eq!(frames[1].dir, Direction::ServerToClient);
        assert_eq!(w.path(), scratch.0.as_path());
    }

    #[test]
    fn writer_redacts_c2s_frames_only_when_enabled() {
        let scratch = ScratchFile::new("redact");
        let w = TraceWriter::create(&scratch.0, &header(), Instant::now(), true).unwrap();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"api_key":"sekrit"}}}"#;
        // Response-side redaction is out of scope for v1 (ADR 0021 §3) — an
        // echo server can leak the secret into an s2c frame.
        let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"echoed_api_key":"sekrit"}}"#;
        w.record(Direction::ClientToServer, Some("tools/call"), req);
        w.record(Direction::ServerToClient, Some("tools/call"), resp);

        let text = std::fs::read_to_string(&scratch.0).unwrap();
        let (_, frames) = parse_trace(&text).unwrap();
        assert!(!frames[0].body.contains("sekrit"), "c2s must be redacted");
        assert!(frames[0].body.contains(format::REDACTED));
        assert!(frames[1].body.contains("sekrit"), "s2c is recorded raw");
    }

    #[test]
    fn writer_with_redaction_off_records_raw() {
        let scratch = ScratchFile::new("noredact");
        let w = TraceWriter::create(&scratch.0, &header(), Instant::now(), false).unwrap();
        let req = r#"{"method":"tools/call","params":{"arguments":{"password":"p4ss"}}}"#;
        w.record(Direction::ClientToServer, Some("tools/call"), req);
        let text = std::fs::read_to_string(&scratch.0).unwrap();
        assert!(text.contains("p4ss"));
    }

    /// Canned transport: fixed response, records delegated calls.
    struct Canned {
        response: String,
        notified: Arc<StdMutex<Vec<String>>>,
        versions: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl Transport for Canned {
        async fn request(&mut self, _body: &str) -> Result<String, TransportError> {
            Ok(self.response.clone())
        }
        async fn notify(&mut self, body: &str) -> Result<(), TransportError> {
            self.notified.lock().unwrap().push(body.to_owned());
            Ok(())
        }
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }
        fn set_protocol_version(&mut self, version: &str) {
            self.versions.lock().unwrap().push(version.to_owned());
        }
        async fn shutdown(self: Box<Self>) -> Result<(), TransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn tracing_transport_records_and_delegates() {
        let scratch = ScratchFile::new("decorator");
        let writer =
            Arc::new(TraceWriter::create(&scratch.0, &header(), Instant::now(), true).unwrap());
        let notified = Arc::new(StdMutex::new(Vec::new()));
        let versions = Arc::new(StdMutex::new(Vec::new()));
        let inner = Canned {
            response: r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_owned(),
            notified: notified.clone(),
            versions: versions.clone(),
        };
        let mut t = TracingTransport::new(inner, writer);

        assert_eq!(t.pid(), Some(4242), "pid must delegate");
        t.set_protocol_version("2025-03-26");
        assert_eq!(
            versions.lock().unwrap().as_slice(),
            ["2025-03-26"],
            "set_protocol_version must delegate"
        );

        let resp = t
            .request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#)
            .await
            .unwrap();
        assert!(resp.contains(r#""ok":true"#));
        t.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#)
            .await
            .unwrap();
        assert_eq!(notified.lock().unwrap().len(), 1, "notify must delegate");

        Box::new(t).shutdown().await.unwrap();

        let text = std::fs::read_to_string(&scratch.0).unwrap();
        let (_, frames) = parse_trace(&text).unwrap();
        assert_eq!(frames.len(), 3, "c2s request + s2c response + c2s notify");
        assert_eq!(frames[0].dir, Direction::ClientToServer);
        assert_eq!(frames[0].method.as_deref(), Some("tools/list"));
        assert_eq!(frames[1].dir, Direction::ServerToClient);
        assert_eq!(frames[1].method.as_deref(), Some("tools/list"));
        assert_eq!(frames[2].dir, Direction::ClientToServer);
        assert_eq!(
            frames[2].method.as_deref(),
            Some("notifications/initialized")
        );
    }
}
