//! tmux session discovery and lifecycle.
//!
//! Everything here shells out to `tmux` on the machine hosting this server.
//! That keeps the web layer honest: the sessions it lists are the same ones
//! you would see from `tmux ls` over SSH, with no separate registry to drift.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A live tmux session as reported by `tmux list-sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
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
