//! Task execution in local tmux sessions.
//!
//! The daemon runs *on* the worker, so unlike `hive-core`'s SSH path these are
//! plain local `tmux` invocations. The session/log/sentinel shape is kept
//! deliberately identical to `hive_core::workers::ssh::spawn_tmux`, so a
//! session started by the daemon and one started by direct SSH delegation look
//! the same to anyone attaching, and to the watchdog.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use hive_common::{TaskAssignment, TaskCommand, TaskState};
use tokio::process::Command;
use tracing::{info, warn};

use crate::registry::Registry;
use crate::reporter::Reporter;

/// Marker the shell appends after each command, carrying its exit code.
/// Matches Phase 3's sentinel so both paths are readable the same way.
const SENTINEL: &str = "__HIVE_DONE__";

/// How often the executor checks whether a command has finished.
const POLL: Duration = Duration::from_millis(500);

/// Where task logs are written.
pub fn log_path_for(task_id: &str) -> String {
    format!("/tmp/hive-task-{task_id}.log")
}

/// Quote a string for safe interpolation into a POSIX shell command.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the shell line for one command: env, working dir, the command itself,
/// then the sentinel carrying its exit code.
///
/// The exit code is captured inside the brace group *before* the pipe, so it is
/// the command's own status and not `tee`'s.
fn shell_line(cmd: &TaskCommand, log_path: &str) -> String {
    let mut prefix = String::new();
    if let Some(dir) = &cmd.working_dir {
        prefix.push_str(&format!("cd {} && ", shq(dir)));
    }
    for (key, value) in stable_env(&cmd.env_vars) {
        prefix.push_str(&format!("export {key}={} ; ", shq(&value)));
    }
    format!(
        "{{ ( {prefix}{} ); echo \"{SENTINEL}$?\"; }} 2>&1 | tee -a {log_path}",
        cmd.command
    )
}

/// Env vars in a deterministic order — `HashMap` iteration order is arbitrary,
/// which would make the generated shell line untestable.
fn stable_env(env: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<_> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

/// Exact-match target for *session*-scoped commands (`has-session`,
/// `kill-session`). The `=` prefix stops tmux prefix-matching, so a request for
/// `hive-1` can never land on `hive-10`.
fn session_target(session: &str) -> String {
    format!("={session}")
}

/// Exact-match target for *pane*-scoped commands (`send-keys`, `capture-pane`,
/// `display-message`).
///
/// These take a target-pane, and a bare `=name` is not valid there — tmux
/// answers "can't find pane". The trailing colon makes it a session-qualified
/// pane reference while keeping the `=` exact-match semantics: verified that
/// `=hive-1:` reaches `hive-1` and not `hive-1-longer`.
fn pane_target(session: &str) -> String {
    format!("={session}:")
}

async fn tmux(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn session_exists(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &session_target(session)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Last `lines` of a session's visible pane.
pub async fn capture_pane(session: &str, lines: u32) -> anyhow::Result<String> {
    tmux(&[
        "capture-pane",
        "-p",
        "-t",
        &pane_target(session),
        "-S",
        &format!("-{lines}"),
    ])
    .await
}

/// Tail of a task's log file — lossless, unlike the pane, which only holds
/// what is currently on screen.
pub async fn tail_log(log_path: &str, lines: usize) -> Option<String> {
    let content = tokio::fs::read_to_string(log_path).await.ok()?;
    let kept: Vec<&str> = content.lines().rev().take(lines).collect();
    Some(kept.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

/// PID of the session's active pane, used for signalling.
async fn pane_pid(session: &str) -> anyhow::Result<i32> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        &pane_target(session),
        "#{pane_pid}",
    ])
    .await?;
    out.trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("could not parse pane pid '{}': {e}", out.trim()))
}

/// The foreground process group of the pane's terminal.
///
/// This is the subtlety that makes pause actually work. Commands arrive by
/// `send-keys`, so the pane's shell runs them as a job — and because the shell
/// has job control, that job gets its **own** process group, distinct from the
/// shell's. Signalling the shell's group therefore stops the shell and leaves
/// the work running, which is exactly the bug this replaced: SIGSTOP returned
/// success while the task kept writing output.
///
/// `tpgid` is the tty's foreground process group, which is the job. Signalling
/// that reaches the command and every child it spawned.
async fn foreground_pgid(session: &str) -> anyhow::Result<i32> {
    let pid = pane_pid(session).await?;
    let out = Command::new("ps")
        .args(["-o", "tpgid=", "-p", &pid.to_string()])
        .output()
        .await?;
    let tpgid: i32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("could not read foreground pgid for pane {pid}: {e}"))?;

    // With no job running, tpgid is the shell's own group — signalling it is
    // still the right thing (there is nothing else to hit), so no special case.
    if tpgid <= 0 {
        anyhow::bail!("pane {pid} has no foreground process group");
    }
    Ok(tpgid)
}

/// Signals this module is allowed to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Stop,
    Cont,
}

impl Signal {
    fn as_libc(self) -> i32 {
        match self {
            Signal::Stop => libc::SIGSTOP,
            Signal::Cont => libc::SIGCONT,
        }
    }
}

/// Reject process-group ids that must never be signalled.
///
/// This guard exists because of a real incident, not a hypothetical. Signalling
/// used to shell out to `kill -STOP -<pgid>`, and a negative argument there is
/// overloaded: `-1` does not mean "process group 1", it means **every process
/// the user can signal**. One bad pgid stopped the host's web server, editor
/// server, user systemd, tmux server, and an unrelated multi-day data pipeline.
///
/// So: never 0 (the caller's own group — the daemon would freeze itself), never
/// 1 (init, and the source of the `-1` broadcast), never negative.
fn validate_pgid(pgid: i32) -> anyhow::Result<i32> {
    if pgid <= 1 {
        anyhow::bail!(
            "refusing to signal process group {pgid}: 0 is our own group, \
             1 is init, and negatives broadcast to every process"
        );
    }
    let own = unsafe { libc::getpgrp() };
    if pgid == own {
        anyhow::bail!("refusing to signal process group {pgid}: it is this daemon's own group");
    }
    Ok(pgid)
}

/// Send `signal` to one process group, via `killpg` rather than the `kill`
/// binary — no argument parsing to misread, and no shell in the path.
fn signal_group(pgid: i32, signal: Signal) -> anyhow::Result<()> {
    let pgid = validate_pgid(pgid)?;
    // SAFETY: killpg takes a validated positive pgid and a signal constant.
    let rc = unsafe { libc::killpg(pgid, signal.as_libc()) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("killpg({pgid}, {signal:?}) failed: {err}");
    }
    Ok(())
}

/// Process groups on this pane's tty that are currently stopped.
///
/// Used to resume when the pgid recorded at pause time is unavailable — after
/// a daemon restart, say. `stat` beginning with `T` is a stopped process.
async fn stopped_pgids(session: &str) -> Vec<i32> {
    let Ok(tty) = tmux(&["display-message", "-p", "-t", &pane_target(session), "#{pane_tty}"]).await
    else {
        return Vec::new();
    };
    let tty = tty.trim().trim_start_matches("/dev/");
    let Ok(out) = Command::new("ps")
        .args(["-t", tty, "-o", "pgid=,stat="])
        .output()
        .await
    else {
        return Vec::new();
    };

    let mut pgids: Vec<i32> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pgid: i32 = parts.next()?.parse().ok()?;
            parts.next()?.starts_with('T').then_some(pgid)
        })
        .collect();
    pgids.sort_unstable();
    pgids.dedup();
    pgids
}

/// Suspend a task's processes (SIGSTOP), returning the group that was stopped.
///
/// A real stop, not `send-keys C-c`: the work freezes mid-flight and can be
/// genuinely resumed, which is what a `resume` endpoint has to mean. An
/// interrupt would end the command and make resuming impossible.
///
/// The caller must keep the returned pgid: once a job is stopped, bash reclaims
/// the terminal, so the tty's foreground group is then the *shell*, and asking
/// for it again at resume time sends SIGCONT to the wrong process.
pub async fn pause(session: &str) -> anyhow::Result<i32> {
    let pgid = foreground_pgid(session).await?;
    // Belt and braces: the group must live on this session's own tty, so a
    // misread pgid cannot reach processes belonging to anything else.
    if !pgid_is_on_session_tty(session, pgid).await {
        anyhow::bail!(
            "refusing to pause process group {pgid}: it is not on session '{session}' tty"
        );
    }
    signal_group(pgid, Signal::Stop)?;
    Ok(pgid)
}

/// Whether `pgid` names a process group running on this session's terminal.
async fn pgid_is_on_session_tty(session: &str, pgid: i32) -> bool {
    let Ok(tty) = tmux(&["display-message", "-p", "-t", &pane_target(session), "#{pane_tty}"]).await
    else {
        return false;
    };
    let tty = tty.trim().trim_start_matches("/dev/");
    let Ok(out) = Command::new("ps").args(["-t", tty, "-o", "pgid="]).output().await else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .any(|p| p == pgid)
}

/// Resume suspended processes (SIGCONT).
///
/// Prefers the pgid recorded at pause time; falls back to whatever is stopped
/// on the pane's tty, so a task paused before a daemon restart can still be
/// resumed.
pub async fn resume(session: &str, paused_pgid: Option<i32>) -> anyhow::Result<()> {
    if let Some(pgid) = paused_pgid {
        return signal_group(pgid, Signal::Cont);
    }
    let stopped = stopped_pgids(session).await;
    if stopped.is_empty() {
        anyhow::bail!("nothing is stopped in session '{session}'");
    }
    for pgid in stopped {
        signal_group(pgid, Signal::Cont)?;
    }
    Ok(())
}

/// Kill the session outright.
pub async fn kill(session: &str, paused_pgid: Option<i32>) -> anyhow::Result<()> {
    // Continue first: a stopped process never acts on a terminating signal
    // until it runs again, so killing a paused session without this can leave
    // orphaned stopped processes behind after the session is gone.
    let _ = resume(session, paused_pgid).await;
    tmux(&["kill-session", "-t", &session_target(session)]).await?;
    Ok(())
}

/// Read the exit code from the last sentinel in a log, if the command finished.
fn sentinel_exit_code(log: &str) -> Option<i32> {
    log.lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(SENTINEL))
        .and_then(|c| c.trim().parse().ok())
}

/// How many sentinels the log holds — i.e. how many commands have finished.
fn sentinel_count(log: &str) -> usize {
    log.lines()
        .filter(|l| l.trim().starts_with(SENTINEL))
        .count()
}

/// Run a whole assignment: create the session, then feed it each command.
///
/// Spawned as a background task by `POST /task` so the handler can answer
/// immediately — the master gets `Running` and polls, rather than holding a
/// connection open for the length of the work.
pub async fn run_task(
    task: TaskAssignment,
    registry: Registry,
    reporter: Reporter,
    worker_name: String,
) {
    let session = task.tmux_session_name.clone();
    let log_path = log_path_for(&task.task_id);
    let _ = tokio::fs::remove_file(&log_path).await;

    if let Err(e) = start_session(&session, &log_path).await {
        warn!(task = %task.task_id, error = %e, "could not start tmux session");
        registry.fail(&task.task_id, e.to_string()).await;
        reporter.report(&registry, &task.task_id, &worker_name).await;
        return;
    }

    registry.set_state(&task.task_id, TaskState::Running).await;
    reporter.report(&registry, &task.task_id, &worker_name).await;
    info!(task = %task.task_id, session = %session, "task running");

    let mut last_exit = 0;
    for (index, cmd) in task.commands.iter().enumerate() {
        let expected = index + 1;
        let line = shell_line(cmd, &log_path);

        if let Err(e) = send_line(&session, &line).await {
            warn!(task = %task.task_id, error = %e, "send-keys failed");
            registry.fail(&task.task_id, e.to_string()).await;
            reporter.report(&registry, &task.task_id, &worker_name).await;
            return;
        }

        // A command marked `wait_for_completion: false` is fire-and-forget:
        // move to the next one without waiting for its sentinel.
        if !cmd.wait_for_completion {
            continue;
        }

        match wait_for_sentinel(&log_path, expected, cmd.timeout_secs, &session).await {
            Ok(code) => {
                last_exit = code;
                // A failing command stops the sequence — later commands
                // usually assume the earlier ones worked.
                if code != 0 {
                    warn!(task = %task.task_id, code, "command failed, stopping sequence");
                    break;
                }
            }
            Err(e) => {
                warn!(task = %task.task_id, error = %e, "command did not complete");
                let _ = kill(&session, None).await;
                registry.fail(&task.task_id, e.to_string()).await;
                reporter.report(&registry, &task.task_id, &worker_name).await;
                return;
            }
        }
    }

    registry.finish(&task.task_id, last_exit).await;
    reporter.report(&registry, &task.task_id, &worker_name).await;
    info!(task = %task.task_id, exit = last_exit, "task finished");
}

/// Start a detached session holding an idle shell, ready for `send-keys`.
async fn start_session(session: &str, log_path: &str) -> anyhow::Result<()> {
    if session_exists(session).await {
        anyhow::bail!("tmux session '{session}' already exists");
    }
    tokio::fs::write(log_path, "").await?;
    // A login shell, so ~/.local/bin tools (claude, codex) resolve — the same
    // reason hive-web launches sessions with `bash -l`.
    tmux(&["new-session", "-d", "-s", session, "bash", "-l"]).await?;
    Ok(())
}

async fn send_line(session: &str, line: &str) -> anyhow::Result<()> {
    tmux(&["send-keys", "-t", &pane_target(session), line, "Enter"]).await?;
    Ok(())
}

/// Block until the log holds `expected` sentinels, then return the last code.
async fn wait_for_sentinel(
    log_path: &str,
    expected: usize,
    timeout_secs: Option<u64>,
    session: &str,
) -> anyhow::Result<i32> {
    let deadline = timeout_secs.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));

    loop {
        let log = tokio::fs::read_to_string(log_path).await.unwrap_or_default();
        if sentinel_count(&log) >= expected {
            return sentinel_exit_code(&log)
                .ok_or_else(|| anyhow::anyhow!("sentinel present but unparseable"));
        }
        // If the session is gone the command can never finish — someone killed
        // it, or the shell exited. Waiting for the deadline would just stall.
        if !session_exists(session).await {
            anyhow::bail!("tmux session '{session}' disappeared before the command completed");
        }
        if let Some(deadline) = deadline {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out after {}s", timeout_secs.unwrap_or(0));
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_line_applies_working_dir_and_sorted_env() {
        let cmd = TaskCommand::new("echo hi")
            .with_dir("/tmp/work")
            .with_env("ZED", "9")
            .with_env("ALPHA", "1");
        let line = shell_line(&cmd, "/tmp/x.log");
        assert!(line.contains("cd '/tmp/work' &&"));
        let alpha = line.find("ALPHA").expect("ALPHA present");
        let zed = line.find("ZED").expect("ZED present");
        assert!(alpha < zed, "env must be emitted in a deterministic order");
        assert!(line.contains("echo hi"));
        assert!(line.contains("tee -a /tmp/x.log"));
    }

    #[test]
    fn shell_line_captures_the_commands_own_exit_code() {
        let line = shell_line(&TaskCommand::new("false"), "/tmp/x.log");
        // `$?` must be read inside the brace group, before the pipe into tee,
        // or it reports tee's status instead of the command's.
        let sentinel_at = line.find(SENTINEL).expect("sentinel present");
        let pipe_at = line.find("| tee").expect("pipe present");
        assert!(sentinel_at < pipe_at, "sentinel must precede the pipe");
    }

    #[test]
    fn shell_quoting_survives_embedded_quotes() {
        let cmd = TaskCommand::new("echo x").with_dir("/tmp/it's here");
        let line = shell_line(&cmd, "/tmp/x.log");
        assert!(line.contains(r"'/tmp/it'\''s here'"));
    }

    #[test]
    fn refuses_the_process_group_ids_that_caused_the_incident() {
        // -1 as a `kill` argument means "every process the user can signal".
        // Reaching that broadcast is what stopped a host's web server, editor
        // server, tmux server and an unrelated data pipeline.
        assert!(validate_pgid(-1).is_err(), "negative pgid must be refused");
        assert!(validate_pgid(0).is_err(), "0 is the caller's own group");
        assert!(validate_pgid(1).is_err(), "1 is init");
        // The daemon must never be able to freeze itself.
        let own = unsafe { libc::getpgrp() };
        assert!(validate_pgid(own).is_err(), "own process group must be refused");
    }

    #[test]
    fn accepts_a_plausible_foreign_process_group() {
        let own = unsafe { libc::getpgrp() };
        let candidate = if own == 424242 { 424243 } else { 424242 };
        assert_eq!(validate_pgid(candidate).unwrap(), candidate);
    }

    #[test]
    fn pane_and_session_targets_differ_but_both_pin_exactly() {
        // A bare `=name` is rejected by pane-scoped commands ("can't find
        // pane"); the trailing colon is what makes it a valid pane reference.
        assert_eq!(session_target("hive-1"), "=hive-1");
        assert_eq!(pane_target("hive-1"), "=hive-1:");
        // Both keep the `=`, which is what stops `hive-1` matching `hive-10`.
        assert!(session_target("hive-1").starts_with('='));
        assert!(pane_target("hive-1").starts_with('='));
    }

    #[test]
    fn reads_the_last_sentinel_exit_code() {
        let log = "output\n__HIVE_DONE__0\nmore\n__HIVE_DONE__42\n";
        assert_eq!(sentinel_exit_code(log), Some(42));
        assert_eq!(sentinel_count(log), 2);
    }

    #[test]
    fn no_sentinel_means_still_running() {
        assert_eq!(sentinel_exit_code("just output\n"), None);
        assert_eq!(sentinel_count("just output\n"), 0);
    }

    #[test]
    fn sentinel_ignores_output_that_merely_mentions_it() {
        // A command echoing the marker mid-line must not be mistaken for the
        // shell's own completion marker, which always starts its line.
        let log = "see __HIVE_DONE__0 in docs\n";
        assert_eq!(sentinel_count(log), 0);
    }
}
