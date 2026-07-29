//! Safe OpenSSH worker launcher.
//!
//! No job field is interpolated into the remote command. Inventory data only
//! selects an SSH destination and standard OpenSSH file/port options; the
//! remote command is fixed to `mcp-loadtest __distributed-agent --stdio`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::channel::NdjsonChannel;

/// One SSH inventory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshAgentSpec {
    /// Portable artifact/inventory name.
    pub name: String,
    /// OpenSSH host alias or `user@alias`.
    pub destination: String,
    /// Optional SSH port.
    pub port: Option<u16>,
    /// Optional private-key path passed as one `-i` argument.
    pub identity_file: Option<PathBuf>,
    /// Optional known-hosts file. Strict checking remains enabled.
    pub known_hosts_file: Option<PathBuf>,
    /// SSH connection timeout.
    pub connect_timeout: Duration,
}

/// Fully constructed local process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshCommand {
    /// Local OpenSSH executable.
    pub program: PathBuf,
    /// Argument vector; never interpreted by a local shell.
    pub args: Vec<OsString>,
}

/// OpenSSH launcher configuration.
#[derive(Debug, Clone)]
pub struct SshLauncher {
    program: PathBuf,
}

impl Default for SshLauncher {
    fn default() -> Self {
        Self {
            program: PathBuf::from("ssh"),
        }
    }
}

impl SshLauncher {
    /// Use the `ssh` executable resolved by the operating system.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use an explicit local OpenSSH executable.
    ///
    /// This is a caller-owned local setting, never a value accepted from a
    /// remote job frame.
    pub fn with_program(program: PathBuf) -> Self {
        Self { program }
    }

    /// Validate inventory data and construct the shell-free local argv.
    pub fn build_command(&self, spec: &SshAgentSpec) -> Result<SshCommand, SshLaunchError> {
        validate_spec(spec)?;

        let timeout_secs = spec.connect_timeout.as_secs().max(1);
        let mut args = vec![
            OsString::from("-T"),
            OsString::from("-o"),
            OsString::from("BatchMode=yes"),
            OsString::from("-o"),
            OsString::from("StrictHostKeyChecking=yes"),
            OsString::from("-o"),
            OsString::from(format!("ConnectTimeout={timeout_secs}")),
        ];
        if let Some(port) = spec.port {
            args.push(OsString::from("-p"));
            args.push(OsString::from(port.to_string()));
        }
        if let Some(path) = &spec.identity_file {
            args.push(OsString::from("-i"));
            args.push(path.as_os_str().to_owned());
        }
        if let Some(path) = &spec.known_hosts_file {
            args.push(OsString::from("-o"));
            let mut value = OsString::from("UserKnownHostsFile=");
            value.push(path.as_os_str());
            args.push(value);
        }

        // `--` terminates local ssh options, so a destination can never be
        // reinterpreted as one. Everything after the destination is a fixed
        // remote command; no config/job value reaches the remote shell.
        args.extend([
            OsString::from("--"),
            OsString::from(&spec.destination),
            OsString::from("mcp-loadtest"),
            OsString::from("__distributed-agent"),
            OsString::from("--stdio"),
        ]);

        Ok(SshCommand {
            program: self.program.clone(),
            args,
        })
    }

    /// Spawn OpenSSH with piped control stdin/stdout and captured stderr.
    pub fn launch(&self, spec: &SshAgentSpec) -> Result<SshAgentProcess, SshLaunchError> {
        let invocation = self.build_command(spec)?;
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(SshLaunchError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or(SshLaunchError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(SshLaunchError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(SshLaunchError::MissingPipe("stderr"))?;
        Ok(SshAgentProcess {
            child,
            channel: NdjsonChannel::new(stdout, stdin),
            stderr: Some(stderr),
        })
    }
}

/// Running OpenSSH child and its control channel.
pub struct SshAgentProcess {
    child: Child,
    channel: NdjsonChannel<ChildStdout, ChildStdin>,
    stderr: Option<ChildStderr>,
}

impl SshAgentProcess {
    /// Mutable protocol channel over the child's stdout/stdin.
    pub fn channel_mut(&mut self) -> &mut NdjsonChannel<ChildStdout, ChildStdin> {
        &mut self.channel
    }

    /// Take the captured stderr stream for a bounded redacting pump.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// Operating-system child id, when available.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Wait for the SSH child to exit.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        self.child.wait().await
    }

    /// Terminate the SSH child.
    pub async fn kill(&mut self) -> Result<(), std::io::Error> {
        self.child.kill().await
    }
}

/// SSH inventory validation or process-launch failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SshLaunchError {
    /// Inventory name is not a portable token.
    #[error("invalid SSH agent name `{0}`")]
    InvalidName(String),
    /// Destination could be parsed as options or remote shell text.
    #[error("invalid SSH destination `{0}`")]
    InvalidDestination(String),
    /// Port zero is never valid.
    #[error("SSH port must be greater than zero")]
    InvalidPort,
    /// Connection timeout must be positive.
    #[error("SSH connect timeout must be greater than zero")]
    InvalidTimeout,
    /// A configured path was empty.
    #[error("SSH {0} path must not be empty")]
    EmptyPath(&'static str),
    /// OpenSSH could not be started.
    #[error("starting OpenSSH: {0}")]
    Spawn(std::io::Error),
    /// A requested stdio pipe was unexpectedly absent.
    #[error("OpenSSH child did not expose piped {0}")]
    MissingPipe(&'static str),
}

fn validate_spec(spec: &SshAgentSpec) -> Result<(), SshLaunchError> {
    if !portable_token(&spec.name, 64, false) {
        return Err(SshLaunchError::InvalidName(spec.name.clone()));
    }
    if !portable_token(&spec.destination, 255, true)
        || spec.destination.starts_with('-')
        || spec.destination.matches('@').count() > 1
    {
        return Err(SshLaunchError::InvalidDestination(spec.destination.clone()));
    }
    if spec.port == Some(0) {
        return Err(SshLaunchError::InvalidPort);
    }
    if spec.connect_timeout.is_zero() {
        return Err(SshLaunchError::InvalidTimeout);
    }
    for (label, path) in [
        ("identity-file", spec.identity_file.as_ref()),
        ("known-hosts", spec.known_hosts_file.as_ref()),
    ] {
        if path.is_some_and(|path| path.as_os_str().is_empty()) {
            return Err(SshLaunchError::EmptyPath(label));
        }
    }
    Ok(())
}

fn portable_token(value: &str, max_len: usize, allow_at: bool) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-')
                || (allow_at && byte == b'@')
        })
        && !matches!(value, "." | "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SshAgentSpec {
        SshAgentSpec {
            name: "east-1".to_owned(),
            destination: "loadtest@east.example".to_owned(),
            port: Some(2222),
            identity_file: Some(PathBuf::from("keys/agent")),
            known_hosts_file: Some(PathBuf::from("known_hosts")),
            connect_timeout: Duration::from_secs(20),
        }
    }

    #[test]
    fn builds_strict_shell_free_fixed_command() {
        let command = SshLauncher::new().build_command(&spec()).unwrap();
        let args: Vec<String> = command
            .args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-o", "StrictHostKeyChecking=yes"])
        );
        assert_eq!(
            &args[args.len() - 5..],
            &[
                "--",
                "loadtest@east.example",
                "mcp-loadtest",
                "__distributed-agent",
                "--stdio"
            ]
        );
    }

    #[test]
    fn rejects_destination_injection() {
        for destination in [
            "-oProxyCommand=evil",
            "host;evil",
            "host evil",
            "user@host@other",
            "",
        ] {
            let mut input = spec();
            input.destination = destination.to_owned();
            assert!(
                matches!(
                    SshLauncher::new().build_command(&input),
                    Err(SshLaunchError::InvalidDestination(_))
                ),
                "{destination:?} must be rejected"
            );
        }
    }

    #[test]
    fn paths_are_single_argv_values() {
        let mut input = spec();
        input.identity_file = Some(PathBuf::from("a key with spaces"));
        let command = SshLauncher::new().build_command(&input).unwrap();
        assert!(command.args.iter().any(|arg| arg == "a key with spaces"));
    }
}
