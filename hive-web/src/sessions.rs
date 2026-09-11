//! tmux session discovery and lifecycle.
//!
//! Discover local and configured remote tmux servers, without a separate registry.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A live tmux session as reported by `tmux list-sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    #[serde(default = "local_host")]
    pub host: String,
    pub windows: u32,
    /// Unix timestamp the session was created.
    pub created: i64,
    pub attached: bool,
    /// Title of the active window — usually the running program.
    pub current_command: String,
    /// tmux window name. Set at creation to the session kind, so the dashboard
    /// can say "claude" even though `pane_current_command` reports the login
    /// shell that claude runs under.
    pub window_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<hive_core::delegation::store::Run>,
}

pub fn local_host() -> String {
    "local".into()
}

/// Shell quoting is needed because SSH sends one command string to the remote shell.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn ssh_args(worker: &hive_common::protocol::WorkerInfo) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        // Dashboard polling and terminal tabs share one authenticated transport.
        // Reconnecting every four seconds can hit a bastion's SSH login limits.
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=60".into(),
        "-o".into(),
        "ControlPath=~/.ssh/hive-web-%C".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "ServerAliveInterval=5".into(),
        "-o".into(),
        "ServerAliveCountMax=2".into(),
    ];
    if let Some(port) = worker.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    args.push(worker.ssh_target());
    args
}

pub fn remote_command(command: &str) -> String {
    format!("{}; {command}", hive_core::workers::ssh::REMOTE_PATH)
}

async fn remote(
    worker: &hive_common::protocol::WorkerInfo,
    command: &str,
) -> anyhow::Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_args(worker))
        .arg(remote_command(command))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output()).await??;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn list_on(worker: &hive_common::protocol::WorkerInfo) -> anyhow::Result<Vec<Session>> {
    // Only an absent tmux server is an empty list. Missing tmux and SSH errors
    // must be surfaced so an unreachable worker does not look empty.
    let out = remote(worker, &format!(
        "out=$(tmux list-sessions -F {} 2>&1); rc=$?; if [ \"$rc\" -eq 0 ]; then printf '%s\\n' \"$out\"; else case \"$out\" in *'no server running'*|*'error connecting to '*'/default (No such file or directory)'*) ;; *) printf '%s\\n' \"$out\" >&2; exit \"$rc\";; esac; fi",
        quote(LIST_FORMAT))).await?;
    Ok(out
        .lines()
        .filter_map(parse_line)
        .map(|mut s| {
            s.host = worker.name.clone();
            s
        })
        .collect())
}

pub async fn exists_on(
    worker: &hive_common::protocol::WorkerInfo,
    name: &str,
) -> anyhow::Result<()> {
    remote(
        worker,
        &format!("tmux has-session -t {}", quote(&format!("={name}"))),
    )
    .await?;
    Ok(())
}

pub async fn create_on(
    worker: &hive_common::protocol::WorkerInfo,
    name: &str,
    kind: Kind,
    dir: Option<&str>,
) -> anyhow::Result<()> {
    let mut command = format!(
        "tmux new-session -d -s {} -n {}",
        quote(name),
        quote(kind.label())
    );
    if let Some(dir) = dir {
        command.push_str(&format!(" -c {}", quote(dir)));
    }
    match kind.launch_command() {
        Some(program) => command.push_str(&format!(
            " bash -lc {}",
            quote(&format!("{program}; exec bash -l"))
        )),
        None => command.push_str(" bash -l"),
    }
    remote(worker, &command).await?;
    Ok(())
}

pub async fn kill_on(worker: &hive_common::protocol::WorkerInfo, name: &str) -> anyhow::Result<()> {
    remote(
        worker,
        &format!("tmux kill-session -t {}", quote(&format!("={name}"))),
    )
    .await?;
    Ok(())
}

/// What to launch in a newly created session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Shell,
    Claude,
    Codex,
}

impl Kind {
    /// The command tmux should start the session with.
    ///
    /// `claude` and `codex` live in `~/.local/bin`, which is only on `PATH`
    /// for login shells — so we go through `bash -lc` rather than exec'ing
    /// the bare name, which would fail under a non-interactive server process.
    /// Short label used as the tmux window name.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Shell => "shell",
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    fn launch_command(self) -> Option<&'static str> {
        match self {
            Kind::Shell => None,
            Kind::Claude => Some("claude"),
            Kind::Codex => Some("codex --skip-git-repo-check"),
        }
    }
}

/// tmux's `list-sessions` format string, one field per `|`.
const LIST_FORMAT: &str = "#{session_name}|#{session_windows}|#{session_created}|#{session_attached}|#{pane_current_command}|#{window_name}";

pub async fn list() -> anyhow::Result<Vec<Session>> {
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", LIST_FORMAT])
        .stderr(Stdio::null())
        .output()
        .await?;

    // tmux exits non-zero with "no server running" when nothing is up. That is
    // an empty list, not an error.
    if !out.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_line)
        .collect())
}

fn parse_line(line: &str) -> Option<Session> {
    let mut parts = line.split('|');
    let name = parts.next()?.to_string();
    if name.is_empty() {
        return None;
    }
    Some(Session {
        windows: parts.next()?.parse().unwrap_or(0),
        created: parts.next()?.parse().unwrap_or(0),
        attached: parts.next()? != "0",
        current_command: parts.next().unwrap_or("").to_string(),
        window_name: parts.next().unwrap_or("").to_string(),
        name,
        host: local_host(),
        run: None,
    })
}

pub async fn exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &format!("={name}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a detached session named `name` running `kind`.
pub async fn create(name: &str, kind: Kind, working_dir: Option<&str>) -> anyhow::Result<()> {
    if exists(name).await {
        anyhow::bail!("session '{name}' already exists");
    }

    let mut cmd = Command::new("tmux");
    cmd.args(["new-session", "-d", "-s", name, "-n", kind.label()]);
    if let Some(dir) = working_dir {
        cmd.args(["-c", dir]);
    }
    // Login shell so ~/.local/bin (claude, codex) is on PATH. Without `-l`
    // the tools resolve only for interactive logins, not for what we spawn.
    match kind.launch_command() {
        Some(program) => cmd.args(["bash", "-lc", &format!("{program}; exec bash -l")]),
        None => cmd.args(["bash", "-l"]),
    };

    let out = cmd.output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub async fn kill(name: &str) -> anyhow::Result<()> {
    let out = Command::new("tmux")
        .args(["kill-session", "-t", &format!("={name}")])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux kill-session failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Session names go into `tmux -t` arguments, so keep them boring.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_arguments_do_not_execute_shell_metacharacters() {
        let value = "space ' quote; $(exit 17) `exit 18`";
        let output = Command::new("sh")
            .args(["-c", &format!("printf '%s' {}", quote(value))])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
    }

    #[test]
    fn parses_a_list_sessions_line() {
        let s = parse_line("hive-1|3|1735689600|1|bash|claude").expect("should parse");
        assert_eq!(s.name, "hive-1");
        assert_eq!(s.windows, 3);
        assert_eq!(s.created, 1735689600);
        assert!(s.attached);
        assert_eq!(s.current_command, "bash");
        assert_eq!(s.window_name, "claude");
    }

    #[test]
    fn detached_session_reports_not_attached() {
        let s = parse_line("build|1|1735689600|0|bash|shell").expect("should parse");
        assert!(!s.attached);
    }

    #[test]
    fn rejects_names_that_would_confuse_tmux() {
        assert!(valid_name("hive-worker_1"));
        assert!(!valid_name(""));
        assert!(!valid_name("has space"));
        assert!(!valid_name("semi;colon"));
        assert!(!valid_name("dollar$sign"));
        assert!(!valid_name(&"x".repeat(65)));
    }
}
