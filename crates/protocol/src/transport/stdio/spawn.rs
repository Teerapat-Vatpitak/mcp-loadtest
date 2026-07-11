//! Process spawn + stderr-mode wiring for [`StdioTransport`].
//!
//! Split out of `stdio/mod.rs` so the wire path (request/notify/raw_send +
//! Drop) and the spawn path each stay under the 300-line production
//! convention. See ADR 0013.

use std::ffi::OsStr;
use std::process::Stdio;

use tokio::io::BufReader;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::StdioTransport;
use super::stderr_pump::spawn_stderr_pump;
use crate::transport::TransportError;
use crate::transport::spawn_options::{SpawnOptions, StderrMode};

impl StdioTransport {
    /// Spawn `command` with `args` using the default [`SpawnOptions`] (stderr
    /// inherits the parent's, so test runners still see the server's panics).
    ///
    /// Thin async delegate to [`Self::spawn_with`]. Was synchronous pre-0.1;
    /// it is now `async` so the stderr-capture pump (a `tokio` task + async
    /// file open) can be set up. The only callers are `Session::spawn` /
    /// `Session::spawn_with`, which are already async, so the 2-arg
    /// `Session::spawn(cmd, args)` public API is unchanged. See ADR 0013.
    pub async fn spawn<I, S>(command: &str, args: I) -> Result<Self, TransportError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::spawn_with(command, args, &SpawnOptions::default()).await
    }

    /// Spawn `command` with `args`, applying `opts` to the child's stderr.
    ///
    /// Footgun this addresses: when `mcp-loadtest` runs as a child of an
    /// MCP-aware agent (Claude Code, Cursor, ...), an inherited server stderr
    /// blends into the agent's view — noisy and confusing. `opts.stderr`
    /// controls the disposition:
    /// - [`StderrMode::Inherit`] (default) — pass through to the parent; no
    ///   pump task, zero overhead.
    /// - [`StderrMode::CaptureToFile`] — redirect to a per-run file (quiet).
    /// - [`StderrMode::TeeToFile`] — file **and** live mirror to the parent.
    ///
    /// For the capture/tee modes a background pump task drains the piped
    /// stderr; it is cancellation-aware and joined on shutdown so the file is
    /// flushed (see the `stderr_pump` submodule).
    ///
    /// `kill_on_drop(true)` ensures a panicking parent doesn't leave zombie
    /// children behind.
    pub async fn spawn_with<I, S>(
        command: &str,
        args: I,
        opts: &SpawnOptions,
    ) -> Result<Self, TransportError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let capture_path = match &opts.stderr {
            StderrMode::Inherit => None,
            StderrMode::CaptureToFile(p) | StderrMode::TeeToFile(p) => Some(p.clone()),
        };
        let stderr_cfg = if capture_path.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        };

        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr_cfg)
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Other("child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Other("child has no stdout".into()))?;

        let pump_cancel = CancellationToken::new();
        let stderr_pump = match capture_path {
            None => None,
            Some(path) => {
                let child_stderr = child
                    .stderr
                    .take()
                    .ok_or_else(|| TransportError::Other("child has no stderr".into()))?;
                let file = tokio::fs::File::create(&path)
                    .await
                    .map_err(TransportError::Io)?;
                let tee = matches!(opts.stderr, StderrMode::TeeToFile(_));
                Some(spawn_stderr_pump(
                    child_stderr,
                    file,
                    tee,
                    pump_cancel.clone(),
                ))
            }
        };

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            line_buf: String::new(),
            stderr_pump,
            pump_cancel,
        })
    }
}
