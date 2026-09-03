//! SSH transport to a worker machine — connection pooling, tmux session
//! creation, and remote log tailing.
//!
//! Connection pooling and keepalives ride on OpenSSH's own `ControlMaster`
//! multiplexing (via [`openssh::SessionBuilder`]), configured both here
//! (`server_alive_interval`) and, for the master itself, in the worker's
//! `~/.ssh/config` `Host` entry (`ControlPersist`, `ControlPath`). Every
//! command issued through the same [`SshWorker`] rides the same
//! already-authenticated master connection instead of re-authenticating.

use std::sync::Arc;
use std::time::Duration;

use openssh::{KnownHosts, Session, SessionBuilder, Stdio};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};

/// A pooled SSH connection to one worker, with tmux helpers layered on top.
pub struct SshWorker {
    session: Arc<Session>,
}

impl SshWorker {
    /// Connect to `ssh_target` (`user@host`, where `host` is expected to
    /// resolve via the local `~/.ssh/config`, keeping real hostnames/IPs
    /// out of any committed config).
    pub async fn connect(ssh_target: &str) -> anyhow::Result<Self> {
        let session = SessionBuilder::default()
            .known_hosts_check(KnownHosts::Strict)
            .server_alive_interval(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(10))
            .connect(ssh_target)
            .await
            .map_err(|e| anyhow::anyhow!("SSH connect to '{ssh_target}' failed: {e}"))?;
        Ok(Self {
            session: Arc::new(session),
        })
    }

    /// Cheap reachability probe, used for worker health checks.
    pub async fn ping(&self) -> anyhow::Result<()> {
        let status = self
            .session
            .command("true")
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("ping failed: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("ping command exited non-zero")
        }
    }

    /// Run a command and return its stdout. For short, synchronous queries
    /// (machine probes, capability checks) — not for long-running work, which
    /// belongs in a supervised tmux session via [`SshWorker::spawn_tmux`].
    pub async fn run(&self, command: &str) -> anyhow::Result<String> {
        let output = self
            .session
            .command("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("remote command failed: {e}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "remote command exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Create a detached tmux session on the worker running `command`,
    /// piping its combined stdout/stderr into `log_path` on the remote
    /// filesystem so it can be tailed losslessly (rather than polling
    /// `tmux capture-pane`, which can miss short-lived output between
    /// polls). A sentinel line is appended so callers can detect
    /// completion and capture the exit code from the log stream itself.
    pub async fn spawn_tmux(
        &self,
        session_name: &str,
        command: &str,
        log_path: &str,
    ) -> anyhow::Result<()> {
        // A single synchronous pipe into one `tee`, not `> >(tee ...)`
        // process substitution: process substitution runs its reader as a
        // background job that the shell does NOT wait for before moving on
        // to the next `;`-separated statement, so the sentinel line below
        // can be written (or the file re-truncated) before the command's
        // own output is flushed — a real race that dropped output in
        // testing. Capturing `$?` inside the brace group, before the pipe,
        // keeps the original command's exit code rather than tee's.
        let inner = format!("{{ ( {command} ); echo \"__HIVE_DONE__$?\"; }} 2>&1 | tee {log_path}");
        let status = self
            .session
            .command("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                session_name,
                "bash",
                "-c",
                inner.as_str(),
            ])
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("failed to start tmux session '{session_name}': {e}"))?;
        if !status.success() {
            anyhow::bail!("tmux new-session failed for '{session_name}'");
        }
        Ok(())
    }

    /// Send keys to an existing tmux session — e.g. `&["C-c"]` to interrupt
    /// a runaway process without killing the session outright, preserving
    /// state for a human to inspect on reattach.
    pub async fn send_keys(&self, session_name: &str, keys: &[&str]) -> anyhow::Result<()> {
        let mut cmd = self.session.command("tmux");
        cmd.args(["send-keys", "-t", session_name]);
        cmd.args(keys.iter().copied());
        let status = cmd
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("tmux send-keys failed for '{session_name}': {e}"))?;
        if !status.success() {
            anyhow::bail!("tmux send-keys returned non-zero for '{session_name}'");
        }
        Ok(())
    }

    /// Capture the last `lines` of a tmux pane (fallback / point-in-time
    /// snapshot — the primary supervision path uses [`SshWorker::tail`]).
    pub async fn capture_pane(&self, session_name: &str, lines: u32) -> anyhow::Result<String> {
        let output = self
            .session
            .command("tmux")
            .args([
                "capture-pane",
                "-p",
                "-t",
                session_name,
                "-S",
                format!("-{lines}").as_str(),
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("tmux capture-pane failed for '{session_name}': {e}"))?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Start tailing `log_path` on the remote host from the beginning,
    /// returning a line stream that reads over a dedicated SSH channel on
    /// the same pooled connection.
    pub async fn tail(&self, log_path: &str) -> anyhow::Result<LogTail> {
        LogTail::start(self.session.clone(), log_path).await
    }
}

/// A live line-by-line stream of a remote log file (`tail -f` over SSH).
pub struct LogTail {
    // Held to keep the remote process (and its SSH channel) alive for as
    // long as the tail is in use; dropped to terminate it.
    _child: openssh::Child<Arc<Session>>,
    lines: Lines<BufReader<openssh::ChildStdout>>,
}

impl LogTail {
    async fn start(session: Arc<Session>, log_path: &str) -> anyhow::Result<Self> {
        let mut cmd = session.arc_command("tail");
        cmd.args(["-f", "-n", "+1", log_path]);
        cmd.stdout(Stdio::piped());
        let mut child = cmd
            .spawn()
            .await
            .map_err(|e| anyhow::anyhow!("failed to start `tail -f {log_path}`: {e}"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| anyhow::anyhow!("tail process has no stdout"))?;
        Ok(Self {
            _child: child,
            lines: BufReader::new(stdout).lines(),
        })
    }

    /// Read the next line, or `Ok(None)` if the remote tail process ended.
    pub async fn next_line(&mut self) -> anyhow::Result<Option<String>> {
        Ok(self.lines.next_line().await?)
    }
}
