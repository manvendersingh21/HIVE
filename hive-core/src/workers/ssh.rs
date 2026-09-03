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

/// What suspending a session actually achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseOutcome {
    /// The session was live and is now stopped.
    Suspended,
    /// The session had already finished; there was nothing to stop.
    AlreadyEnded,
}

/// Exact-match target for tmux commands that take a *pane*, such as
/// `send-keys` and `capture-pane`.
///
/// Two separate things are going on and both matter:
///
/// * A bare session name is not a valid target-pane. tmux answers
///   `can't find pane: <name>` and exits non-zero. This silently broke the
///   watchdog's pause: Tier-1 would correctly detect `rm -rf /` in a live
///   session, then fail to send `C-c`, so a session flagged as dangerous kept
///   running. It was mistaken for a benign race in Phase 3 ("the command
///   already exited"), which is why it survived this long.
/// * The `=` prefix stops tmux prefix-matching, so `hive-1` can never resolve
///   to `hive-10`.
fn pane_target(session_name: &str) -> String {
    format!("={session_name}:")
}

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

    /// Suspend a session's foreground job with SIGSTOP.
    ///
    /// Replaces `send-keys C-c`, which was never a pause. A session created by
    /// [`SshWorker::spawn_tmux`] runs `bash -c <command>` — its only job — so
    /// interrupting the command leaves the shell with nothing to do, the
    /// session exits, and long-running children can orphan. Verified: a C-c'd
    /// session vanished and left a stray `sleep 300` behind. That is a kill
    /// with extra steps, and it destroys exactly the state a human was meant
    /// to attach to and review.
    ///
    /// SIGSTOP freezes the work in place instead: the session stays attachable,
    /// the process tree is intact, and it can be resumed.
    ///
    /// The remote snippet is written defensively on purpose. Passing a negative
    /// pgid to `kill` is how a process group is addressed, but `-1` there means
    /// *every process the user can signal* — that mistake once stopped a
    /// worker's unrelated services. So the pgid is required to be non-empty,
    /// all digits, and greater than 1 before it is used.
    pub async fn pause_session(&self, session_name: &str) -> anyhow::Result<PauseOutcome> {
        let script = format!(
            r#"set -e
pid=$(tmux display-message -p -t '={session_name}:' '#{{pane_pid}}' 2>/dev/null)
case "$pid" in ''|*[!0-9]*) echo "no pane pid" >&2; exit 1;; esac
pgid=$(ps -o tpgid= -p "$pid" | tr -d ' ')
case "$pgid" in ''|*[!0-9]*) echo "no foreground pgid" >&2; exit 1;; esac
if [ "$pgid" -le 1 ]; then echo "refusing to signal pgid $pgid" >&2; exit 1; fi
kill -STOP -"$pgid"
echo "stopped $pgid""#
        );

        // Distinguish "already finished" from "failed to stop a live session".
        // Reporting a completed session as still-running-and-dangerous sends an
        // operator to attach to something that no longer exists, and buries the
        // cases where a genuinely live session escaped the pause.
        if !self.session_exists(session_name).await {
            return Ok(PauseOutcome::AlreadyEnded);
        }

        let out = self.run(&script).await?;
        tracing::debug!(session = session_name, result = out.trim(), "session suspended");
        Ok(PauseOutcome::Suspended)
    }

    /// Whether a tmux session is still alive on the worker.
    pub async fn session_exists(&self, session_name: &str) -> bool {
        self.session
            .command("tmux")
            .args(["has-session", "-t", &format!("={session_name}")])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Resume a session suspended by [`SshWorker::pause_session`].
    ///
    /// Finds the stopped process group on the pane's tty rather than trusting a
    /// remembered id: once a job stops, the shell reclaims the terminal, so the
    /// tty's *foreground* group is no longer the stopped work.
    pub async fn resume_session(&self, session_name: &str) -> anyhow::Result<()> {
        let script = format!(
            r#"set -e
tty=$(tmux display-message -p -t '={session_name}:' '#{{pane_tty}}' 2>/dev/null | sed 's|^/dev/||')
[ -n "$tty" ] || {{ echo "no tty" >&2; exit 1; }}
resumed=0
for pgid in $(ps -t "$tty" -o pgid=,stat= | awk '$2 ~ /^T/ {{print $1}}' | sort -u); do
  case "$pgid" in ''|*[!0-9]*) continue;; esac
  if [ "$pgid" -gt 1 ]; then kill -CONT -"$pgid" && resumed=$((resumed+1)); fi
done
[ "$resumed" -gt 0 ] || {{ echo "nothing stopped" >&2; exit 1; }}
echo "resumed $resumed""#
        );
        let out = self.run(&script).await?;
        tracing::debug!(session = session_name, result = out.trim(), "session resumed");
        Ok(())
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
        cmd.args(["send-keys", "-t", &pane_target(session_name)]);
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
                pane_target(session_name).as_str(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_target_is_exact_and_valid_for_pane_scoped_commands() {
        // The trailing colon is what makes this a target-pane rather than a
        // session name; without it tmux rejects the command outright.
        assert_eq!(pane_target("hive-1"), "=hive-1:");
        // The `=` keeps `hive-1` from matching `hive-10`.
        assert!(pane_target("hive-1").starts_with('='));
    }
}
