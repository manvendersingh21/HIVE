//! [`SessionHost`] over a **local** detached tmux session.
//!
//! This is the local analogue of [`crate::workers::ssh::SshWorker`]'s tmux path: same
//! session shape, same completion sentinel, same log-tailing supervision — but the CLI
//! runs on this machine, so every tmux call is a plain `Command` instead of an SSH
//! channel. Keeping the shape identical is deliberate: a session started here and one
//! started over SSH look the same to anyone who attaches to it, and to the watchdog that
//! reads its log.
//!
//! Three things here are load-bearing and each of them cost real debugging time in this
//! project (`docs/STATUS.md`):
//!
//! * **A bare session name is not a target-pane.** tmux answers `can't find pane` and
//!   exits non-zero, which once made the watchdog's pause a silent no-op — it could
//!   detect a dangerous session perfectly and never actually stop one. See
//!   [`pane_target`].
//! * **`C-c` is a kill, not a pause.** The shell we start has exactly one job; interrupt
//!   it and the session ends and its children orphan, destroying the state an operator
//!   was just told to attach to and inspect. We SIGSTOP the pane's foreground process
//!   group instead. See [`LocalSessionHost::suspend`].
//! * **A negative pgid is overloaded.** `-1` does not mean "group 1", it means every
//!   process the user can signal. That has actually happened here. See [`validate_pgid`].
//!
//! A fourth was found while building this, and it is not recorded anywhere else in the
//! project: **tmux resurrects a stopped pane process.** If the group we SIGSTOP contains
//! the pane's own process, tmux answers with a group-wide SIGCONT and the pause quietly
//! comes undone. [`build_command`] explains the shape that avoids it and how it was
//! measured; it applies to `workers::ssh::pause_session` too, which is noted there.
//!
//! ## What this does not do
//!
//! Supervision only lasts as long as [`SessionHost::wait`] is being polled. The tmux
//! session itself is detached and survives this process, but nothing scans its output
//! once the future is dropped — the same limitation `docs/STATUS.md` records for the SSH
//! path. Recovering supervision of an already-running session after a restart would need
//! the per-session state below to be persisted; today it is in-memory.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::collab::{Result, SessionHandle, SessionHost, SessionOutcome, SessionSpec};
use crate::watchdog::Watchdog;

/// Marker the shell appends when the program exits, carrying its exit code.
///
/// Identical to the SSH path's and to `hive-worker`'s, so one log reader works for all
/// three. Changing it here alone would split them.
const SENTINEL: &str = "__HIVE_DONE__";

/// How long to block on the next log line before re-checking the deadline and whether the
/// session is still alive. A tail that never yields must not stall those checks.
const POLL: Duration = Duration::from_millis(500);

/// Applied when [`SessionHost::wait`] is handed a [`SessionHandle`] this host has no
/// record of — a handle outliving the process that launched it, say.
///
/// A [`SessionHandle`] carries only a name and a log path, so the spec's `timeout_secs`
/// is unrecoverable in that case. Falling back to *no* limit would leave a runaway agent
/// running forever, which is the one outcome supervision exists to prevent, so we fall
/// back to a bound and say so loudly.
const UNKNOWN_HANDLE_TIMEOUT_SECS: u64 = 3600;

/// Refuse to build a command line longer than this.
///
/// The whole shell line is passed to tmux as a *single* argv entry, and Linux caps one
/// argument at `MAX_ARG_STRLEN` (128 KiB). A brief that exceeds it fails inside `execve`
/// with a message that says nothing about the brief being too long, so we refuse first
/// and name the actual cause. If briefs ever get this big the fix is to hand the prompt
/// through a file rather than argv — which is a change to `SessionSpec`, not to this
/// guard.
const MAX_COMMAND_BYTES: usize = 100 * 1024;

// ---------------------------------------------------------------------------
// Shell and tmux plumbing
// ---------------------------------------------------------------------------

/// Quote a string for safe interpolation into a POSIX shell command.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Exact-match target for *session*-scoped commands (`has-session`, `kill-session`).
///
/// The `=` prefix disables tmux prefix-matching, so a request for `hive-1` can never land
/// on `hive-10`.
fn session_target(session: &str) -> String {
    format!("={session}")
}

/// Exact-match target for *pane*-scoped commands (`display-message`, `capture-pane`,
/// `send-keys`).
///
/// A bare `=name` is not valid where tmux wants a target-pane; it answers `can't find
/// pane` and exits non-zero. The trailing colon is what makes this a session-qualified
/// pane reference, and it is the difference between a pause that works and one that
/// silently fails — which is precisely how the watchdog once detected `rm -rf /` in a
/// live session and then left it running.
fn pane_target(session: &str) -> String {
    format!("={session}:")
}

/// Build the shell line that runs the worker's CLI under tmux.
///
/// Shape, and why each part is the way it is:
///
/// ```text
/// { ( cd <cwd> && <program> <args…> <prompt> ); echo "__HIVE_DONE__$?"; } 2>&1 | tee <log>
/// ```
///
/// * The exit code is read **inside** the brace group, before the pipe, so it is the
///   program's own status and not `tee`'s. This is why a plain pipe is safe here; the
///   naive `<program> | tee <log>; echo $?` is what loses the code.
/// * `tee` (not `>`) because the contract is that output reaches *both* the live pane, so
///   an operator can attach and watch, and the log, which is the supervision and evidence
///   channel.
/// * `2>&1` on the group, so a program that reports trouble on stderr — which is most of
///   them — is scanned by Tier-1 rather than being invisible to it.
/// * The `cd` is explicit even though [`LocalSessionHost::launch`] also passes `-c` to
///   tmux, because we start a *login* shell (so `~/.local/bin` tools resolve) and a login
///   shell runs profile scripts that are entitled to `cd` wherever they like.
///
/// A failing `cd` short-circuits the `&&` and its non-zero status becomes the sentinel's
/// code, so "the workspace was not there" is visible in the log rather than being
/// reported as the program failing.
///
/// ## Why `set -m`, which is not decoration
///
/// Without it the whole pipeline runs in the pane process's own process group, and a
/// SIGSTOP aimed at that group **does not stick**: tmux's `server_child_stopped` reaps its
/// pane child with `WUNTRACED`, sees it stopped, and answers with `killpg(pane_pid,
/// SIGCONT)` — reviving the entire group, our target included. Measured on tmux 3.7c: the
/// stop was accepted (`killpg` returned 0) and every process was back in `S` a moment
/// later, so a "pause" reported success while the agent kept working. That is the same
/// class of failure as the `can't find pane` bug, and just as invisible.
///
/// `set -m` turns on job control in this non-interactive shell, which puts the pipeline in
/// a process group of its own and makes *that* group the terminal's foreground group. The
/// pane's own process is then no longer in the group we stop, tmux is never told anything
/// stopped, and the SIGSTOP holds. Verified: pane leader stays `Ss` while the job's three
/// processes sit at `T+` and the session stays attachable.
///
/// (This is also why `hive-worker`'s executor can pause and the SSH path's
/// `bash -c '<command>'` cannot: the worker daemon sends its command to an *interactive*
/// shell, which gives the job its own group for the same reason.)
fn build_command(spec: &SessionSpec) -> Result<String> {
    let mut program = shq(&spec.program);
    for arg in &spec.args {
        program.push(' ');
        program.push_str(&shq(arg));
    }
    // An empty prompt is omitted rather than passed as an empty argument: some CLIs treat
    // an empty positional as "read from stdin", and stdin here is the tmux pane.
    if !spec.prompt.is_empty() {
        program.push(' ');
        program.push_str(&shq(&spec.prompt));
    }

    let line = format!(
        "set -m; {{ ( cd {} && {program} ); echo \"{SENTINEL}$?\"; }} 2>&1 | tee {}",
        shq(&spec.cwd.to_string_lossy()),
        shq(&spec.log.to_string_lossy())
    );

    if line.len() > MAX_COMMAND_BYTES {
        // Deliberately does not name the program: an error can be surfaced to a peer, and
        // §3 forbids a peer learning which vendor runs a role.
        anyhow::bail!(
            "command line for session '{}' is {} bytes, over the {MAX_COMMAND_BYTES}-byte limit \
             (the brief is too long to pass through argv)",
            spec.name,
            line.len()
        );
    }
    Ok(line)
}

/// Run a tmux command and return its stdout.
async fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("could not run tmux: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn session_exists(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &session_target(session)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// PID of the session's active pane — the shell, not the job.
async fn pane_pid(session: &str) -> Result<i32> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        &pane_target(session),
        "#{pane_pid}",
    ])
    .await?;
    // Not [`parse_pgid`]: this pid is only ever handed to `ps`, never signalled, so the
    // "never our own group" rule would reject a perfectly good pid for the wrong reason.
    parse_id(out.trim())
        .map_err(|e| anyhow::anyhow!("pane pid for session '{session}' is unusable: {e}"))
}

/// The tty of the session's pane, without the `/dev/` prefix `ps -t` does not want.
async fn pane_tty(session: &str) -> Result<String> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        &pane_target(session),
        "#{pane_tty}",
    ])
    .await?;
    let tty = out.trim().trim_start_matches("/dev/").to_string();
    if tty.is_empty() {
        anyhow::bail!("session '{session}' reported no pane tty");
    }
    Ok(tty)
}

/// The foreground process group of the pane's terminal.
///
/// This is the subtlety that makes pause actually do something, and it only works because
/// [`build_command`] turns on job control: the pipeline then has a process group of its
/// own and holds the terminal, so `tpgid` is the *job*, not the pane's shell. Signalling
/// it reaches the program and every child it spawned, and — critically — leaves the pane's
/// own process alone, which is what stops tmux from undoing the stop. Signalling the pane
/// pid's group instead would be the bug `build_command` documents.
async fn foreground_pgid(session: &str) -> Result<i32> {
    let pid = pane_pid(session).await?;
    let out = Command::new("ps")
        .args(["-o", "tpgid=", "-p", &pid.to_string()])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("could not run ps: {e}"))?;
    parse_pgid(String::from_utf8_lossy(&out.stdout).trim())
        .map_err(|e| anyhow::anyhow!("no usable foreground pgid for pane {pid}: {e}"))
}

/// Parse a pgid the way the shell guard in `workers::ssh::pause_session` does: non-empty
/// and **all digits**.
///
/// `str::parse::<i32>` alone is not that check — it happily accepts `-1` and `+1`, and
/// `-1` is the value this whole guard exists to keep away from `kill`. Rejecting anything
/// non-numeric also catches the `ps` failure modes where the field is missing entirely and
/// the "pgid" is really a header or an error string.
fn parse_pgid(raw: &str) -> Result<i32> {
    validate_pgid(parse_id(raw)?)
}

/// The lexical half of [`parse_pgid`]: non-empty, all digits, fits in an `i32`.
fn parse_id(raw: &str) -> Result<i32> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("empty process id");
    }
    if !raw.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("process id '{raw}' is not all digits");
    }
    raw.parse()
        .map_err(|e| anyhow::anyhow!("process id '{raw}' does not fit: {e}"))
}

/// Reject process-group ids that must never be signalled.
///
/// From a real incident, not a hypothetical: signalling used to shell out to
/// `kill -STOP -<pgid>`, where a negative argument is overloaded — `-1` there does not
/// mean "process group 1", it means **every process the user can signal**. One bad pgid
/// stopped a host's web server, editor server, user systemd, tmux server, and an
/// unrelated multi-day data pipeline.
///
/// So: never negative, never 0 (our own group — the master would freeze itself), never 1
/// (init, and the source of the `-1` broadcast), and never our own group even when it is
/// a plausible-looking number.
fn validate_pgid(pgid: i32) -> Result<i32> {
    if pgid <= 1 {
        anyhow::bail!(
            "refusing to signal process group {pgid}: 0 is our own group, 1 is init, and \
             negatives broadcast to every process the user can signal"
        );
    }
    // SAFETY: getpgrp takes no arguments and cannot fail.
    let own = unsafe { libc::getpgrp() };
    if pgid == own {
        anyhow::bail!("refusing to signal process group {pgid}: it is the master's own group");
    }
    Ok(pgid)
}

/// Signals this module is allowed to send. Pause and resume; never a kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
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

/// Send `signal` to one process group via `killpg` rather than the `kill` binary — no
/// argument parsing to misread, and no shell in the path.
fn signal_group(pgid: i32, signal: Signal) -> Result<()> {
    let pgid = validate_pgid(pgid)?;
    // SAFETY: killpg takes a validated positive pgid and a signal constant.
    let rc = unsafe { libc::killpg(pgid, signal.as_libc()) };
    if rc != 0 {
        anyhow::bail!(
            "killpg({pgid}, {signal:?}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Whether `pgid` names a process group running on this session's terminal.
///
/// Belt and braces on top of [`validate_pgid`]: a misread pgid that happens to be a live,
/// positive, foreign group would still be signalled without this. Requiring it to live on
/// the session's own tty means a bad read can only ever reach this session's own work.
async fn pgid_is_on_session_tty(session: &str, pgid: i32) -> bool {
    let Ok(tty) = pane_tty(session).await else {
        return false;
    };
    let Ok(out) = Command::new("ps")
        .args(["-t", &tty, "-o", "pgid="])
        .output()
        .await
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .any(|p| p == pgid)
}

/// Process groups on this pane's tty that are currently stopped (`ps` `stat` starting
/// with `T`).
///
/// The resume fallback for when no pgid was recorded: once a job stops, the shell
/// reclaims the terminal, so the tty's *foreground* group is then the shell and asking for
/// it again would SIGCONT the wrong process.
async fn stopped_pgids(session: &str) -> Vec<i32> {
    let Ok(tty) = pane_tty(session).await else {
        return Vec::new();
    };
    let Ok(out) = Command::new("ps")
        .args(["-t", &tty, "-o", "pgid=,stat="])
        .output()
        .await
    else {
        return Vec::new();
    };
    let mut pgids: Vec<i32> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pgid = parse_pgid(parts.next()?).ok()?;
            parts.next()?.starts_with('T').then_some(pgid)
        })
        .collect();
    pgids.sort_unstable();
    pgids.dedup();
    pgids
}

/// The exit code carried by a sentinel line, if this line is one.
///
/// Anchored at the start of the trimmed line so a program that merely *prints* the
/// sentinel mid-sentence — an agent echoing this very source file, say — cannot end its
/// own supervision early.
fn sentinel_exit_code(line: &str) -> Option<i32> {
    line.trim().strip_prefix(SENTINEL)?.trim().parse().ok()
}

/// The last sentinel in a whole log, for the case where the session ended while we were
/// between reads.
fn last_sentinel_exit_code(log: &str) -> Option<i32> {
    log.lines().rev().find_map(sentinel_exit_code)
}

/// When a session started at `started` with this budget must be given up on. `0` means no
/// limit: a zero-second budget is never what anyone means, and reading it literally would
/// time out every session built from a defaulted spec before it drew a breath.
fn deadline_from(started: Instant, timeout_secs: u64) -> Option<Instant> {
    (timeout_secs > 0).then(|| started + Duration::from_secs(timeout_secs))
}

// ---------------------------------------------------------------------------
// Log tailing
// ---------------------------------------------------------------------------

/// A live line-by-line stream of a local log file (`tail -f`).
///
/// Tailing, not polling `capture-pane`: the pane holds only what is currently on screen,
/// and a poll interval is exactly long enough to miss a short burst — which for Tier-1
/// means missing the one line that mattered.
struct LogTail {
    // Held so the tail process (and its pipe) live as long as the stream. `kill_on_drop`
    // is what stops `tail -f` from outliving the wait it was started for; without it every
    // supervised session would leak one process.
    _child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl LogTail {
    async fn start(log: &Path) -> Result<Self> {
        let mut child = Command::new("tail")
            .args(["-f", "-n", "+1"])
            .arg(log)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("could not tail '{}': {e}", log.display()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("tail process has no stdout"))?;
        Ok(Self {
            _child: child,
            lines: BufReader::new(stdout).lines(),
        })
    }

    /// The next line, or `Ok(None)` if the tail process ended.
    async fn next_line(&mut self) -> Result<Option<String>> {
        Ok(self.lines.next_line().await?)
    }
}

// ---------------------------------------------------------------------------
// LocalSessionHost
// ---------------------------------------------------------------------------

/// What suspending a session actually achieved.
///
/// The distinction matters more than it looks: reporting a finished session as
/// still-running-and-suspended sends an operator to attach to something that no longer
/// exists, and buries the case where a genuinely live session escaped the pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suspended {
    /// The session was live and its foreground group is now stopped.
    Stopped(i32),
    /// The session had already finished; there was nothing to stop.
    AlreadyEnded,
}

/// What this host remembers about one session it launched.
///
/// It exists because [`SessionHandle`] carries only a name and a log path, so neither the
/// timeout nor the pgid stopped at pause time can be recovered from the handle later.
struct SessionRecord {
    started: Instant,
    timeout_secs: u64,
    /// Recorded at pause so [`SessionHost::resume`] signals the group that was actually
    /// stopped rather than whatever holds the terminal now.
    paused_pgid: Option<i32>,
}

/// Launches a stock agentic CLI in a local detached tmux session, and supervises it.
pub struct LocalSessionHost {
    watchdog: Watchdog,
    sessions: Mutex<HashMap<String, SessionRecord>>,
}

impl LocalSessionHost {
    /// A host with the built-in Tier-1 rule set.
    pub fn new() -> Self {
        Self::with_watchdog(Watchdog::new())
    }

    /// A host supervised by a specific watchdog — a config-built one with extra rules, or
    /// a narrowed rule set for a test.
    pub fn with_watchdog(watchdog: Watchdog) -> Self {
        Self {
            watchdog,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Suspend the session's foreground process group with SIGSTOP.
    ///
    /// Not `send-keys C-c`. The shell we start has exactly one job, so an interrupt leaves
    /// it with nothing to do: the session exits and long-running children orphan —
    /// verified in this project, where a C-c'd session vanished and left a stray
    /// `sleep 300` behind. That is a kill with extra steps, and it destroys the state a
    /// human was just asked to attach to. SIGSTOP freezes the work in place: the session
    /// stays attachable, the process tree is intact, and it can be resumed.
    async fn suspend(&self, name: &str) -> Result<Suspended> {
        if !session_exists(name).await {
            return Ok(Suspended::AlreadyEnded);
        }
        let pgid = foreground_pgid(name).await?;
        if !pgid_is_on_session_tty(name, pgid).await {
            anyhow::bail!("refusing to pause process group {pgid}: not on session '{name}' tty");
        }
        signal_group(pgid, Signal::Stop)?;
        if let Some(record) = self.sessions.lock().await.get_mut(name) {
            record.paused_pgid = Some(pgid);
        }
        Ok(Suspended::Stopped(pgid))
    }

    /// The deadline for a session, and whether we actually knew its budget.
    async fn deadline_for(&self, name: &str) -> Option<Instant> {
        match self.sessions.lock().await.get(name) {
            Some(record) => deadline_from(record.started, record.timeout_secs),
            None => {
                tracing::warn!(
                    session = name,
                    fallback_secs = UNKNOWN_HANDLE_TIMEOUT_SECS,
                    "no launch record for this session; supervising with the fallback timeout \
                     because the handle does not carry the spec's"
                );
                deadline_from(Instant::now(), UNKNOWN_HANDLE_TIMEOUT_SECS)
            }
        }
    }

    /// Decide the outcome from the log alone, for when the session ended between reads.
    ///
    /// A session that is gone with no sentinel is an error, not an `Exited { code: 0 }`:
    /// someone killed it, or tmux failed to start the shell at all, and reporting that as
    /// a clean exit would let a run proceed to verification on output that was never
    /// produced.
    async fn outcome_from_log(&self, handle: &SessionHandle) -> Result<SessionOutcome> {
        let log = tokio::fs::read_to_string(&handle.log).await.unwrap_or_default();
        match last_sentinel_exit_code(&log) {
            Some(code) => Ok(SessionOutcome::Exited { code }),
            None => anyhow::bail!(
                "session '{}' ended without a completion sentinel; its log is at {}",
                handle.name,
                handle.log.display()
            ),
        }
    }
}

impl Default for LocalSessionHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionHost for LocalSessionHost {
    async fn launch(&self, spec: &SessionSpec) -> Result<SessionHandle> {
        if session_exists(&spec.name).await {
            anyhow::bail!("tmux session '{}' already exists", spec.name);
        }
        if !spec.cwd.is_dir() {
            anyhow::bail!(
                "workspace '{}' for session '{}' is not a directory",
                spec.cwd.display(),
                spec.name
            );
        }

        let command = build_command(spec)?;

        // Create the log before tmux does, for two reasons: `tail -f` in `wait` needs the
        // file to exist, and a missing parent directory would otherwise make `tee` fail
        // inside the pane — leaving a session that runs with no log at all, which reads
        // from the outside exactly like a session that produced no output.
        if let Some(parent) = spec.log.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                anyhow::anyhow!("could not create log directory '{}': {e}", parent.display())
            })?;
        }
        tokio::fs::write(&spec.log, "")
            .await
            .map_err(|e| anyhow::anyhow!("could not create log '{}': {e}", spec.log.display()))?;

        // `bash -l` so tools installed under `~/.local/bin` resolve — the same reason
        // hive-web and the worker daemon start login shells.
        tmux(&[
            "new-session",
            "-d",
            "-s",
            &spec.name,
            "-c",
            &spec.cwd.to_string_lossy(),
            "bash",
            "-lc",
            &command,
        ])
        .await?;

        // tmux exits 0 for a session that dies immediately, so confirm rather than assume.
        // A program that is not installed dies here, and its `command not found` is in the
        // log — which is where the diagnosis belongs anyway, since naming the program in a
        // returned error would put a vendor name somewhere it can travel (§3).
        if !session_exists(&spec.name).await {
            anyhow::bail!(
                "session '{}' did not survive launch; see {}",
                spec.name,
                spec.log.display()
            );
        }

        self.sessions.lock().await.insert(
            spec.name.clone(),
            SessionRecord {
                started: Instant::now(),
                timeout_secs: spec.timeout_secs,
                paused_pgid: None,
            },
        );

        // The program is named at debug level in this process's own log only. It must not
        // reach the bus, a brief, or a contract (§3), so it appears in neither the handle
        // nor any error above.
        tracing::debug!(
            session = %spec.name,
            program = %spec.program,
            cwd = %spec.cwd.display(),
            "local session launched"
        );

        Ok(SessionHandle {
            name: spec.name.clone(),
            log: spec.log.clone(),
        })
    }

    async fn wait(&self, handle: &SessionHandle) -> Result<SessionOutcome> {
        let deadline = self.deadline_for(&handle.name).await;
        let mut tail = LogTail::start(&handle.log).await?;

        loop {
            // Bounded so a session that produces no output for a while still has its
            // deadline checked, and so a session that dies without a sentinel is noticed
            // instead of leaving us blocked on a tail that will never yield again.
            match tokio::time::timeout(POLL, tail.next_line()).await {
                Ok(Ok(Some(line))) => {
                    if let Some(code) = sentinel_exit_code(&line) {
                        return Ok(SessionOutcome::Exited { code });
                    }

                    // Tier 1 on every line, as it arrives.
                    if let Some(analysis) = self.watchdog.scan_line(&line) {
                        let reason = analysis.reason.clone();
                        tracing::warn!(
                            session = %handle.name,
                            severity = ?analysis.severity,
                            "Tier-1 rule matched; suspending"
                        );
                        match self.suspend(&handle.name).await {
                            Ok(Suspended::Stopped(pgid)) => {
                                tracing::warn!(session = %handle.name, pgid, "session suspended");
                                return Ok(SessionOutcome::Paused { reason });
                            }
                            // The work is already over; there is nothing to attach to.
                            // Say what actually happened rather than claiming a pause.
                            Ok(Suspended::AlreadyEnded) => {
                                tracing::warn!(
                                    session = %handle.name,
                                    "Tier-1 matched but the session had already ended"
                                );
                                return self.outcome_from_log(handle).await;
                            }
                            // Never downgrade this to a warning and carry on. A detection
                            // that cannot stop the session is the exact failure mode that
                            // let a flagged session keep running for a whole phase here.
                            Err(e) => anyhow::bail!(
                                "Tier-1 rule matched in session '{}' but it could not be \
                                 suspended, so it may still be running: {e} ({reason})",
                                handle.name
                            ),
                        }
                    }

                    if past(deadline) {
                        return self.time_out(handle).await;
                    }
                }
                // The tail process died; fall back to the file itself.
                Ok(Ok(None)) => return self.outcome_from_log(handle).await,
                Ok(Err(e)) => {
                    return Err(e.context(format!("tailing session '{}'", handle.name)))
                }
                Err(_elapsed) => {
                    if past(deadline) {
                        return self.time_out(handle).await;
                    }
                    // Check liveness only after the tail has gone quiet, and then still
                    // read the log: the session can exit with its sentinel already written
                    // but not yet delivered to us.
                    if !session_exists(&handle.name).await {
                        return self.outcome_from_log(handle).await;
                    }
                }
            }
        }
    }

    async fn pause(&self, handle: &SessionHandle, reason: &str) -> Result<()> {
        match self.suspend(&handle.name).await? {
            Suspended::Stopped(pgid) => {
                tracing::warn!(session = %handle.name, pgid, reason, "session suspended");
            }
            // Not an error: the caller asked for a state that already holds — the session
            // is not running. Failing here would make every pause racing a normal exit
            // look like a supervision failure.
            Suspended::AlreadyEnded => {
                tracing::info!(
                    session = %handle.name,
                    reason,
                    "pause requested for a session that had already ended"
                );
            }
        }
        Ok(())
    }

    async fn resume(&self, handle: &SessionHandle) -> Result<()> {
        let recorded = self
            .sessions
            .lock()
            .await
            .get(&handle.name)
            .and_then(|r| r.paused_pgid);

        if let Some(pgid) = recorded {
            signal_group(pgid, Signal::Cont)?;
            if let Some(record) = self.sessions.lock().await.get_mut(&handle.name) {
                record.paused_pgid = None;
            }
            return Ok(());
        }

        // No record — paused by something else, or by a previous run of this process.
        // Resume whatever is stopped on the pane's own tty.
        let stopped = stopped_pgids(&handle.name).await;
        if stopped.is_empty() {
            anyhow::bail!("nothing is stopped in session '{}'", handle.name);
        }
        for pgid in stopped {
            signal_group(pgid, Signal::Cont)?;
        }
        Ok(())
    }
}

impl LocalSessionHost {
    /// Give up on a session that outran its budget.
    ///
    /// Suspends rather than kills, for the same reason a Tier-1 hit does: the outcome is
    /// `TimedOut` either way, but leaving the work frozen and attachable is what lets
    /// someone find out *why* it overran, and leaving it running is how a runaway agent
    /// outlives the supervision that was meant to bound it.
    async fn time_out(&self, handle: &SessionHandle) -> Result<SessionOutcome> {
        match self.suspend(&handle.name).await {
            Ok(Suspended::Stopped(pgid)) => {
                tracing::warn!(session = %handle.name, pgid, "timed out; session suspended");
            }
            Ok(Suspended::AlreadyEnded) => {}
            Err(e) => {
                tracing::error!(
                    session = %handle.name,
                    error = %e,
                    "timed out and could not be suspended; it may still be running"
                );
            }
        }
        Ok(SessionOutcome::TimedOut)
    }
}

fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec() -> SessionSpec {
        SessionSpec {
            name: "hive-collab-1".to_string(),
            program: "claude".to_string(),
            args: vec!["-p".to_string(), "--output-format".to_string(), "text".to_string()],
            prompt: "Implement the frozen interface.".to_string(),
            cwd: PathBuf::from("/tmp/work"),
            log: PathBuf::from("/tmp/hive/agent.log"),
            timeout_secs: 900,
        }
    }

    #[test]
    fn pane_and_session_targets_differ_but_both_pin_exactly() {
        // A bare `=name` is rejected by pane-scoped commands ("can't find pane"); the
        // trailing colon is what makes it a valid pane reference. Getting this wrong is
        // how the watchdog once detected a dangerous session and never paused one.
        assert_eq!(session_target("hive-1"), "=hive-1");
        assert_eq!(pane_target("hive-1"), "=hive-1:");
        // Both keep the `=`, which is what stops `hive-1` matching `hive-10`.
        assert!(session_target("hive-1").starts_with('='));
        assert!(pane_target("hive-1").starts_with('='));
    }

    #[test]
    fn refuses_the_process_group_ids_that_caused_the_incident() {
        // `-1` as a kill argument means "every process the user can signal". Reaching that
        // broadcast is what stopped a host's web server, tmux server and a data pipeline.
        assert!(parse_pgid("-1").is_err(), "negative pgid must be refused");
        assert!(parse_pgid("0").is_err(), "0 is the caller's own group");
        assert!(parse_pgid("1").is_err(), "1 is init");
        assert!(parse_pgid("").is_err(), "empty pgid must be refused");
        assert!(parse_pgid("   ").is_err(), "whitespace-only pgid must be refused");
        assert!(parse_pgid("12a").is_err(), "a non-numeric pgid must be refused");
        // `+1` parses as 1 for `str::parse`, which is why the all-digits check exists
        // rather than relying on the parse alone.
        assert!(parse_pgid("+1").is_err(), "signed pgid must be refused");
    }

    #[test]
    fn refuses_our_own_process_group_even_though_it_looks_valid() {
        // SAFETY: getpgrp takes no arguments and cannot fail.
        let own = unsafe { libc::getpgrp() };
        assert!(validate_pgid(own).is_err(), "the master must not freeze itself");
        let foreign = if own == 424_242 { 424_243 } else { 424_242 };
        assert_eq!(validate_pgid(foreign).unwrap(), foreign);
        assert_eq!(parse_pgid(&format!("  {foreign}  ")).unwrap(), foreign);
    }

    #[test]
    fn command_captures_the_programs_own_exit_code_not_tees() {
        let line = build_command(&spec()).unwrap();
        let sentinel_at = line.find(SENTINEL).expect("sentinel present");
        let pipe_at = line.find("| tee").expect("tee present");
        // `$?` must be read inside the brace group, before the pipe, or the sentinel
        // reports tee's status instead of the program's.
        assert!(sentinel_at < pipe_at, "sentinel must precede the pipe");
        assert!(line.contains(&format!("{SENTINEL}$?")));
    }

    #[test]
    fn command_enables_job_control_so_the_pause_can_stick() {
        let line = build_command(&spec()).unwrap();
        // Without `set -m` the pipeline shares the pane process's group, and tmux answers
        // a stop of its own pane child with a group-wide SIGCONT — the pause is accepted
        // and then silently undone. Measured on tmux 3.7c.
        assert!(line.starts_with("set -m; "), "got: {line}");
    }

    #[test]
    fn command_runs_in_the_workspace_with_args_then_prompt() {
        let line = build_command(&spec()).unwrap();
        assert!(line.contains("cd '/tmp/work' &&"), "got: {line}");
        let args_at = line.find("'--output-format'").expect("args present");
        let prompt_at = line
            .find("'Implement the frozen interface.'")
            .expect("prompt present");
        assert!(args_at < prompt_at, "the prompt is the final argument");
        // Both streams reach the pane and the log; stderr-only output must be scannable.
        assert!(line.contains("2>&1 | tee '/tmp/hive/agent.log'"), "got: {line}");
    }

    #[test]
    fn command_quoting_survives_embedded_quotes() {
        let mut s = spec();
        s.prompt = "don't break".to_string();
        s.cwd = PathBuf::from("/tmp/it's here");
        let line = build_command(&s).unwrap();
        assert!(line.contains(r"'don'\''t break'"), "got: {line}");
        assert!(line.contains(r"'/tmp/it'\''s here'"), "got: {line}");
    }

    #[test]
    fn command_omits_an_empty_prompt_rather_than_passing_an_empty_argument() {
        let mut s = spec();
        s.prompt = String::new();
        let line = build_command(&s).unwrap();
        assert!(!line.contains("'' "), "got: {line}");
        assert!(line.contains("'claude' '-p'"), "got: {line}");
    }

    #[test]
    fn refuses_a_brief_too_long_for_argv() {
        let mut s = spec();
        s.prompt = "x".repeat(MAX_COMMAND_BYTES + 1);
        let err = build_command(&s).unwrap_err().to_string();
        assert!(err.contains("too long to pass through argv"), "got: {err}");
        // The refusal must not name the vendor's tool: an error can be surfaced to a peer
        // and §3 forbids a peer learning which CLI runs a role.
        assert!(!err.contains("claude"), "error leaked the program name: {err}");
    }

    #[test]
    fn sentinel_is_read_only_at_the_start_of_a_line() {
        assert_eq!(sentinel_exit_code("__HIVE_DONE__0"), Some(0));
        assert_eq!(sentinel_exit_code("  __HIVE_DONE__42  "), Some(42));
        // An agent that prints the sentinel mid-sentence — quoting this file, say — must
        // not be able to end its own supervision.
        assert_eq!(sentinel_exit_code("echo __HIVE_DONE__0"), None);
        assert_eq!(sentinel_exit_code("__HIVE_DONE__not-a-code"), None);
        assert_eq!(sentinel_exit_code("ordinary output"), None);
    }

    #[test]
    fn the_last_sentinel_wins_when_reading_a_whole_log() {
        let log = "starting\n__HIVE_DONE__0\nmore\n__HIVE_DONE__42\n";
        assert_eq!(last_sentinel_exit_code(log), Some(42));
        assert_eq!(last_sentinel_exit_code("no sentinel here\n"), None);
    }

    #[test]
    fn a_zero_timeout_means_no_limit_rather_than_instant_expiry() {
        let now = Instant::now();
        assert!(deadline_from(now, 0).is_none());
        assert!(deadline_from(now, 30).unwrap() > now);
        assert!(!past(None));
        assert!(past(Some(now)));
    }

    #[test]
    fn the_handle_carries_nothing_that_identifies_the_vendor() {
        // The handle is the part of a session that other components hold, so §3's rule
        // shows up here: name and log path only, never the program.
        let s = spec();
        let handle = SessionHandle {
            name: s.name.clone(),
            log: s.log.clone(),
        };
        assert!(!handle.name.contains(&s.program));
        assert!(!handle.log.to_string_lossy().contains(&s.program));
    }

    // ---- Live tests -------------------------------------------------------
    //
    // These need a real tmux on this machine and actually start sessions, so they are
    // ignored by default. Run them with:
    //
    //     cargo test -p hive-core --lib collab::session -- --ignored --test-threads=1
    //
    // `--test-threads=1` because they create tmux sessions with fixed names.

    fn live_spec(name: &str, program: &str, args: &[&str], prompt: &str) -> SessionSpec {
        let dir = std::env::temp_dir().join("hive-collab-live");
        std::fs::create_dir_all(&dir).unwrap();
        SessionSpec {
            name: name.to_string(),
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            prompt: prompt.to_string(),
            cwd: dir.clone(),
            log: dir.join(format!("{name}.log")),
            timeout_secs: 30,
        }
    }

    async fn cleanup(name: &str) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &session_target(name)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    #[tokio::test]
    #[ignore = "needs a live tmux; see the module note above"]
    async fn live_exit_code_survives_the_tee() {
        let host = LocalSessionHost::new();
        let spec = live_spec("hive-live-exit", "sh", &["-c"], "echo hello; exit 7");
        cleanup(&spec.name).await;

        let handle = host.launch(&spec).await.unwrap();
        let outcome = host.wait(&handle).await.unwrap();
        assert_eq!(outcome, SessionOutcome::Exited { code: 7 });

        let log = std::fs::read_to_string(&handle.log).unwrap();
        assert!(log.contains("hello"), "output must reach the log: {log}");
        cleanup(&spec.name).await;
    }

    #[tokio::test]
    #[ignore = "needs a live tmux; see the module note above"]
    async fn live_tier1_match_suspends_instead_of_killing() {
        let host = LocalSessionHost::new();
        // Prints the dangerous string and then keeps running, so there is something left
        // to suspend. If pause were `C-c` the session would be gone by the assertion.
        let spec = live_spec(
            "hive-live-tier1",
            "sh",
            &["-c"],
            "echo 'about to run rm -rf /'; sleep 60",
        );
        cleanup(&spec.name).await;

        let handle = host.launch(&spec).await.unwrap();
        let outcome = host.wait(&handle).await.unwrap();
        assert!(
            matches!(outcome, SessionOutcome::Paused { .. }),
            "expected Paused, got {outcome:?}"
        );
        // The whole point: the session and its process tree are still there to attach to.
        assert!(session_exists(&spec.name).await, "pause must not end the session");
        // And genuinely stopped, not merely still alive — a pause that returns success
        // while the work keeps running is the failure this replaced.
        assert!(
            !stopped_pgids(&spec.name).await.is_empty(),
            "the foreground group must actually be in state T"
        );

        host.resume(&handle).await.unwrap();
        assert!(
            stopped_pgids(&spec.name).await.is_empty(),
            "resume must leave nothing stopped"
        );
        cleanup(&spec.name).await;
    }

    #[tokio::test]
    #[ignore = "needs a live tmux; see the module note above"]
    async fn live_timeout_is_enforced() {
        let host = LocalSessionHost::new();
        let mut spec = live_spec("hive-live-timeout", "sh", &["-c"], "sleep 120");
        spec.timeout_secs = 2;
        cleanup(&spec.name).await;

        let handle = host.launch(&spec).await.unwrap();
        let outcome = host.wait(&handle).await.unwrap();
        assert_eq!(outcome, SessionOutcome::TimedOut);
        cleanup(&spec.name).await;
    }
}
