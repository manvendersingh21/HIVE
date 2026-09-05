//! Finding out which tmux sessions actually exist, locally and on every worker.
//!
//! `hive sessions` used to print "worker delegation is not implemented (Phase 3/4)"
//! while `WorkerPool::active_sessions` had existed since Phase 3. But wiring the command
//! straight to that method would have produced a second, quieter lie: that map is an
//! `Arc<Mutex<HashMap>>` populated by `delegate` **in the calling process**, so a fresh
//! `hive sessions` invocation would truthfully report an empty list every single time,
//! no matter what was running.
//!
//! So the source of truth here is `tmux list-sessions` — on this machine and on each
//! configured worker over SSH. It is the same source `hive-web` uses, for the reason
//! recorded there: no separate registry to drift out of date. What you see is what
//! `tmux ls` would show you if you went and looked.

use std::process::Stdio;

use hive_common::protocol::WorkerInfo;
use tokio::process::Command;

use super::ssh::SshWorker;

/// tmux's `list-sessions` format, one field per `|`. Deliberately identical to
/// `hive-web`'s, so the two surfaces cannot describe the same session differently.
const LIST_FORMAT: &str =
    "#{session_name}|#{session_windows}|#{session_created}|#{session_attached}|#{pane_current_command}|#{window_name}";

/// One live tmux session and where it lives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TmuxSession {
    pub name: String,
    /// The worker's configured name, or `"local"`.
    pub host: String,
    pub windows: u32,
    /// Unix timestamp of creation.
    pub created: i64,
    pub attached: bool,
    /// The active pane's command — usually a login shell.
    pub current_command: String,
    /// The window name, set at creation to the session's kind.
    pub window_name: String,
}

impl TmuxSession {
    /// The command that takes this session over from a local terminal.
    ///
    /// `=name:` rather than a bare name: tmux treats a bare string as a prefix match, and
    /// this project has already shipped one bug from that — a bare session name where a
    /// target-pane was required, which made the watchdog's pause silently fail for a
    /// whole phase (`docs/STATUS.md`, `workers::ssh::pane_target`).
    pub fn attach_command(&self, worker: Option<&WorkerInfo>) -> String {
        let inner = format!("tmux attach -t '={}:'", self.name);
        match worker {
            None => inner,
            Some(w) => format!("ssh -t {} \"{inner}\"", w.ssh_target()),
        }
    }
}

/// Whether a session was created by Hive. Everything else on a worker is somebody's
/// own work and is listed, not filtered — hiding it would make this command lie by
/// omission the moment a session was named unexpectedly.
pub fn is_hive_session(name: &str) -> bool {
    name.starts_with("hive-")
}

/// Sessions on this machine.
pub async fn list_local() -> anyhow::Result<Vec<TmuxSession>> {
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", LIST_FORMAT])
        .stderr(Stdio::null())
        .output()
        .await?;
    // tmux exits non-zero with "no server running" when nothing is up. That is an empty
    // list, not a failure.
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse(&String::from_utf8_lossy(&out.stdout), "local"))
}

/// Sessions on one worker, over its SSH connection.
pub async fn list_on(worker: &WorkerInfo) -> anyhow::Result<Vec<TmuxSession>> {
    let ssh = SshWorker::connect(&worker.ssh_target()).await?;
    // `|| true` so "no server running" comes back as success with no output, matching
    // the local case rather than surfacing as an unreachable worker.
    let stdout = ssh
        .run(&format!(
            "tmux list-sessions -F '{LIST_FORMAT}' 2>/dev/null || true"
        ))
        .await?;
    Ok(parse(&stdout, &worker.name))
}

fn parse(text: &str, host: &str) -> Vec<TmuxSession> {
    text.lines().filter_map(|l| parse_line(l, host)).collect()
}

fn parse_line(line: &str, host: &str) -> Option<TmuxSession> {
    let f: Vec<&str> = line.split('|').collect();
    if f.len() < 6 || f[0].is_empty() {
        return None;
    }
    Some(TmuxSession {
        name: f[0].to_string(),
        host: host.to_string(),
        windows: f[1].parse().unwrap_or(0),
        created: f[2].parse().unwrap_or(0),
        attached: f[3] == "1",
        current_command: f[4].to_string(),
        window_name: f[5].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> WorkerInfo {
        WorkerInfo {
            name: "lawfinder".into(),
            host: "hive-worker-1".into(),
            user: "azureuser".into(),
            port: None,
            tags: vec![],
        }
    }

    #[test]
    fn a_list_line_parses_into_a_session() {
        let s = parse_line("hive-abc|1|1757000000|1|bash|codex", "local").unwrap();
        assert_eq!(s.name, "hive-abc");
        assert_eq!(s.host, "local");
        assert_eq!(s.windows, 1);
        assert_eq!(s.created, 1757000000);
        assert!(s.attached);
        assert_eq!(s.window_name, "codex");
    }

    #[test]
    fn a_truncated_or_empty_line_is_skipped_rather_than_half_parsed() {
        assert!(parse_line("hive-abc|1", "local").is_none());
        assert!(parse_line("", "local").is_none());
        assert!(parse_line("|1|2|0|bash|shell", "local").is_none());
    }

    #[test]
    fn tmux_saying_nothing_is_an_empty_list() {
        assert!(parse("", "local").is_empty());
    }

    #[test]
    fn an_attach_target_is_exact_not_a_prefix() {
        // A bare name is a prefix match in tmux. This project has already shipped one
        // bug from exactly that, and it disabled the watchdog's pause for a phase.
        let s = parse_line("hive-1|1|0|0|bash|shell", "local").unwrap();
        assert!(s.attach_command(None).contains("'=hive-1:'"));
    }

    #[test]
    fn a_remote_attach_goes_through_ssh_with_a_tty() {
        let s = parse_line("hive-1|1|0|0|bash|shell", "lawfinder").unwrap();
        let cmd = s.attach_command(Some(&worker()));
        assert!(cmd.starts_with("ssh -t azureuser@hive-worker-1"), "{cmd}");
        // Without -t there is no tty and tmux refuses to attach.
        assert!(cmd.contains("-t "), "{cmd}");
    }

    #[test]
    fn sessions_this_project_did_not_create_are_recognizable_but_not_hidden() {
        assert!(is_hive_session("hive-34b09a06-03-work"));
        assert!(!is_hive_session("my-editor"));
        // The listing itself keeps both: a filtered list lies by omission.
        let all = parse("hive-a|1|0|0|bash|shell\nmy-editor|2|0|1|nvim|edit", "local");
        assert_eq!(all.len(), 2);
    }
}
