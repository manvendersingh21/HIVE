//! The central session supervisor — one actor that owns every supervised
//! session, and the point at which an incident becomes durable.
//!
//! # What this replaces
//!
//! Phase 3 supervised a delegated session with a bare `tokio::spawn` inside
//! `WorkerPool::delegate`. That had two consequences worth naming, because
//! both were measured rather than theorised:
//!
//! * **The watcher was anonymous.** Nothing held a handle to it. There was no
//!   way to ask what was being supervised, no way to address one supervision,
//!   and no way to stop one. `docs/STATUS.md` records `hive task` exiting a
//!   few hundred milliseconds after delegating — so anything delegated through
//!   the CLI was unwatched from that moment, and nothing in the process could
//!   even report that.
//! * **An incident left no trace.** A Tier-1 hit produced a `tracing::warn!`
//!   and an in-memory `TaskState`. Restart the master and every incident it
//!   had ever raised was gone.
//!
//! Here supervision is a [`SessionSupervisor`] actor holding a registry keyed
//! by tmux session name, with one child `SessionWatch` actor per session, and
//! an incident is written to [`IncidentStore`] at the moment it is raised.
//!
//! # One session's failure must not end the others
//!
//! This is the reason the supervisor overrides
//! [`Actor::handle_supervisor_evt`] rather than taking ractor's default. The
//! default implementation is:
//!
//! ```ignore
//! SupervisionEvent::ActorTerminated(..) | SupervisionEvent::ActorFailed(..) => {
//!     myself.stop(None);
//! }
//! ```
//!
//! — i.e. *any* child exiting stops the supervisor. Every session's watcher is
//! a child, and a watcher exits on the entirely routine event of its session
//! finishing. Taking the default would mean the first session to complete tore
//! down supervision of every other session still running, silently. The
//! override below treats a child's exit as news about that one session:
//! the registry entry is marked unwatched, the reason is logged, and the other
//! children keep running. ractor converts a panic inside a child's `handle`
//! into `ActorFailed` (`catch_unwind` around the child's processing loop), so a
//! panicking tail arrives here as a message rather than as a lost task.
//!
//! # Seams
//!
//! [`SessionTap`], [`IncidentLog`] and [`AlertSink`] exist so the supervision
//! loop can be tested without SSH, tmux, a network, or a database. The old
//! `supervise` took a live `SshWorker` and was therefore only exercisable by an
//! `#[ignore]`d test against a real worker — which is how the pause bug
//! recorded in `workers::ssh::pane_target` survived a whole phase.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hive_common::config::{DatabaseConfig, HiveConfig};
use hive_common::protocol::Incident;
use hive_common::{SafetyAnalysis, SessionInfo, TaskState};
use ractor::{
    Actor, ActorCell, ActorId, ActorProcessingErr, ActorRef, RpcReplyPort, SupervisionEvent,
};
use tracing::{error, info, warn};

use super::incidents::{new_incident, IncidentStore};
use super::notifier::{Alert, Notifier};
use super::Watchdog;
use crate::llm::LlmRouter;
use crate::workers::ssh::PauseOutcome;

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// Everything supervision needs from a running session: its output, and the
/// ability to freeze it.
///
/// Deliberately only two operations. Anything else a reviewer wants (attach,
/// resume, kill) belongs to [`super::review`], which is a human's path, not
/// the watchdog's.
#[async_trait]
pub trait SessionTap: Send + 'static {
    /// The next line of output, or `None` when the stream ends. `None` without
    /// a `__HIVE_DONE__` sentinel means the session or its transport died.
    async fn next_line(&mut self) -> anyhow::Result<Option<String>>;

    /// SIGSTOP the session's foreground process group. **Not** a kill and not
    /// an interrupt — see [`crate::workers::ssh::SshWorker::pause_session`] for
    /// the three separate times this project got that wrong.
    async fn pause(&mut self) -> anyhow::Result<PauseOutcome>;
}

/// Where a raised incident is written.
///
/// [`IncidentStore`] is the real implementation. The trait exists for one
/// specific test: that a *failed* write still leaves the session suspended.
/// Suspension is the safety action, logging is the record, and the record
/// failing must never cost the action. There is no way to make a healthy
/// `IncidentStore` reject a write, so the failure has to be injectable.
pub trait IncidentLog: Send + Sync + 'static {
    fn record(&self, incident: &Incident) -> anyhow::Result<()>;
}

impl IncidentLog for IncidentStore {
    fn record(&self, incident: &Incident) -> anyhow::Result<()> {
        IncidentStore::record(self, incident)
    }
}

/// Where an incident alert goes. [`Notifier`] is the real implementation; a
/// test substitutes a recorder, which also keeps the suite from firing real
/// iMessages on a developer machine that happens to have `PHONE_NUMBER` set.
#[async_trait]
pub trait AlertSink: Send + Sync + 'static {
    async fn deliver(&self, alert: &Alert);
}

#[async_trait]
impl AlertSink for Notifier {
    async fn deliver(&self, alert: &Alert) {
        self.notify(alert).await;
    }
}

/// Open the configured incident log, falling back to an ephemeral one.
///
/// Same trade as `hive-web`'s review page makes: a corrupt or unwritable
/// database costs the operator their incident *history*; refusing to supervise
/// would cost them the safety net entirely. Loud in the log, invisible to the
/// session.
pub fn open_incident_log() -> IncidentStore {
    let root = std::env::var("HIVE_CONFIG_ROOT").unwrap_or_else(|_| ".".to_string());
    let database = match HiveConfig::from_project_root(std::path::Path::new(&root)) {
        Ok(c) => c.database,
        // A machine running the master may carry no config at all; the
        // documented default is where the watchdog writes when it has none.
        Err(_) => DatabaseConfig {
            path: "~/.hive/hive.db".to_string(),
        },
    };
    let path = database.resolved_path();

    match IncidentStore::open(&path) {
        Ok(store) => store,
        Err(e) => {
            error!(
                error = %e,
                path = %path.display(),
                "could not open the incident log — incidents raised this session will not persist"
            );
            IncidentStore::in_memory().expect("an in-memory SQLite database cannot fail to open")
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

/// Everything needed to start watching one session.
pub struct SessionSpec {
    /// Identity and starting state of the session, as the registry will hold it.
    pub session: SessionInfo,
    /// `user@host` for the reattach instructions handed to a reviewer.
    pub ssh_target: String,
    /// What the task is supposed to be doing, for Tier-2 review.
    pub expected_behavior: String,
    /// The session's output and its stop button.
    pub tap: Box<dyn SessionTap>,
    pub llm: Arc<LlmRouter>,
    pub watchdog: Arc<Watchdog>,
    pub alerts: Arc<dyn AlertSink>,
}

/// One row of the supervisor's registry.
#[derive(Debug, Clone)]
pub struct SupervisedSession {
    /// Name, worker, task id, state, and when supervision started.
    pub info: SessionInfo,
    /// Whether a watcher is still attached.
    ///
    /// A finished session keeps its row — `WorkerPool::delegate` documents
    /// `active_sessions()` as the way to observe completion, so dropping the
    /// row the instant the watcher exits would delete the answer. `false` here
    /// with a non-terminal `info.state` is the state worth alarming about: the
    /// session may still be running with nothing watching it.
    pub watching: bool,
}

/// Messages the supervisor accepts.
pub enum SupervisorMsg {
    /// Begin supervising a session. Boxed because the spec carries a tap and
    /// four `Arc`s, and ractor stores mailbox messages by value.
    Supervise(Box<SessionSpec>, RpcReplyPort<Result<(), String>>),
    /// Stop supervising `session` and forget it. Replies `true` if it was
    /// known. This ends *supervision*, not the session: the remote work is
    /// left alone, which is why it is not called anywhere on the safety path.
    Stop(String, RpcReplyPort<bool>),
    /// Everything the supervisor holds.
    List(RpcReplyPort<Vec<SupervisedSession>>),
    /// A watcher reporting its session changed state.
    StateChanged {
        session: String,
        state: TaskState,
    },
}

/// A handle to a running [`SessionSupervisor`]. Cloning shares the actor.
#[derive(Clone)]
pub struct SupervisorHandle {
    actor: ActorRef<SupervisorMsg>,
}

impl SupervisorHandle {
    /// Start the supervisor.
    pub async fn start(incidents: Arc<dyn IncidentLog>) -> anyhow::Result<Self> {
        let (actor, _join) = Actor::spawn(None, SessionSupervisor, incidents)
            .await
            .map_err(|e| anyhow::anyhow!("could not start the session supervisor: {e}"))?;
        Ok(Self { actor })
    }

    /// Start a supervisor writing to the configured incident log.
    pub async fn start_with_default_log() -> anyhow::Result<Self> {
        Self::start(Arc::new(open_incident_log())).await
    }

    /// Begin supervising a session.
    pub async fn supervise(&self, spec: SessionSpec) -> anyhow::Result<()> {
        let outcome: Result<(), String> = ractor::call!(
            self.actor,
            SupervisorMsg::Supervise,
            Box::new(spec)
        )
        .map_err(|e| anyhow::anyhow!("supervisor did not accept the session: {e}"))?;
        outcome.map_err(|e| anyhow::anyhow!(e))
    }

    /// Stop supervising `session`. Returns whether it was in the registry.
    pub async fn stop_session(&self, session: &str) -> anyhow::Result<bool> {
        ractor::call!(self.actor, SupervisorMsg::Stop, session.to_string())
            .map_err(|e| anyhow::anyhow!("supervisor did not answer the stop: {e}"))
    }

    /// Everything the supervisor holds, watched or finished.
    pub async fn supervised(&self) -> anyhow::Result<Vec<SupervisedSession>> {
        ractor::call!(self.actor, SupervisorMsg::List)
            .map_err(|e| anyhow::anyhow!("supervisor did not answer the query: {e}"))
    }

    /// Stop the supervisor and, with it, every watcher it owns.
    pub fn shutdown(&self) {
        self.actor.stop_children(Some("supervisor shutting down".to_string()));
        self.actor.stop(Some("supervisor shutting down".to_string()));
    }
}

// ---------------------------------------------------------------------------
// The supervisor actor
// ---------------------------------------------------------------------------

/// One actor owning every supervised session.
pub struct SessionSupervisor;

struct Entry {
    info: SessionInfo,
    /// `None` once the watcher has exited, for any reason.
    watcher: Option<ActorRef<WatchMsg>>,
}

/// Registry plus the incident log every watcher writes through.
pub struct SupervisorState {
    incidents: Arc<dyn IncidentLog>,
    sessions: HashMap<String, Entry>,
    /// Reverse index, because a supervision event names an [`ActorCell`], not
    /// a session. Children are spawned unnamed on purpose: ractor's name
    /// registry is process-global, and two supervisors in one process (which
    /// the test suite has) would collide on a shared tmux session name.
    by_actor: HashMap<ActorId, String>,
}

impl SupervisorState {
    fn snapshot(&self) -> Vec<SupervisedSession> {
        self.sessions
            .values()
            .map(|e| SupervisedSession {
                info: e.info.clone(),
                watching: e.watcher.is_some(),
            })
            .collect()
    }

    /// Mark the watcher for `who` as gone. Returns the session name, if the
    /// child was one we still track.
    fn watcher_ended(&mut self, who: &ActorCell) -> Option<String> {
        let name = self.by_actor.remove(&who.get_id())?;
        if let Some(entry) = self.sessions.get_mut(&name) {
            entry.watcher = None;
        }
        Some(name)
    }
}

impl Actor for SessionSupervisor {
    type Msg = SupervisorMsg;
    type State = SupervisorState;
    type Arguments = Arc<dyn IncidentLog>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        incidents: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(SupervisorState {
            incidents,
            sessions: HashMap::new(),
            by_actor: HashMap::new(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisorMsg::Supervise(spec, reply) => {
                let name = spec.session.session_name.clone();

                // Two tails on one log would double every Tier-1 hit into two
                // incidents and two suspend attempts on the same pgid.
                if state
                    .sessions
                    .get(&name)
                    .is_some_and(|e| e.watcher.is_some())
                {
                    let _ = reply.send(Err(format!("session '{name}' is already supervised")));
                    return Ok(());
                }

                let info = spec.session.clone();
                let args = WatchArgs {
                    spec,
                    parent: myself.clone(),
                    incidents: state.incidents.clone(),
                };

                // Linked, so this actor is told when the watcher exits — see
                // `handle_supervisor_evt` for why the default handling of that
                // would be catastrophic here.
                match Actor::spawn_linked(None, SessionWatch, args, myself.get_cell()).await {
                    Ok((child, _join)) => {
                        info!(session = %name, "supervision started");
                        state.by_actor.insert(child.get_id(), name.clone());
                        state.sessions.insert(
                            name,
                            Entry {
                                info,
                                watcher: Some(child),
                            },
                        );
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(format!("could not start a watcher: {e}")));
                    }
                }
            }

            SupervisorMsg::Stop(name, reply) => {
                let known = match state.sessions.remove(&name) {
                    Some(entry) => {
                        if let Some(watcher) = entry.watcher {
                            state.by_actor.remove(&watcher.get_id());
                            // `kill`, not `stop`: a graceful stop is processed
                            // between messages, and a watcher parked on
                            // `next_line` has no next message for the whole
                            // life of a quiet session. The kill signal
                            // interrupts the in-flight await.
                            watcher.kill();
                        }
                        info!(session = %name, "supervision stopped");
                        true
                    }
                    None => false,
                };
                let _ = reply.send(known);
            }

            SupervisorMsg::List(reply) => {
                let _ = reply.send(state.snapshot());
            }

            SupervisorMsg::StateChanged { session, state: s } => {
                if let Some(entry) = state.sessions.get_mut(&session) {
                    entry.info.state = s;
                }
            }
        }
        Ok(())
    }

    /// A child watcher exited. Record it against that one session and carry on.
    ///
    /// ractor's default implementation calls `myself.stop(None)` on any child
    /// exit. Inheriting it would mean the first session to finish silently
    /// ended supervision of every other running session — the exact failure
    /// this module exists to remove. Nothing below stops the supervisor.
    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        event: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match event {
            SupervisionEvent::ActorTerminated(who, _, reason) => {
                if let Some(name) = state.watcher_ended(&who) {
                    info!(
                        session = %name,
                        reason = reason.unwrap_or_default(),
                        "session watcher exited"
                    );
                }
            }
            SupervisionEvent::ActorFailed(who, err) => {
                if let Some(name) = state.watcher_ended(&who) {
                    let still_running = state
                        .sessions
                        .get(&name)
                        .is_some_and(|e| !is_terminal(e.info.state));
                    // Loud, because the remote work does not stop when its
                    // watcher does: this is a session that may still be
                    // running with nothing applying Tier-1 rules to it.
                    error!(
                        session = %name,
                        error = %err,
                        still_running,
                        "session watcher FAILED — this session is no longer supervised; \
                         every other session is unaffected"
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Cancelled
    )
}

// ---------------------------------------------------------------------------
// The per-session watcher
// ---------------------------------------------------------------------------

/// Tails one session. Spawned linked to [`SessionSupervisor`], one per session.
struct SessionWatch;

struct WatchArgs {
    spec: Box<SessionSpec>,
    parent: ActorRef<SupervisorMsg>,
    incidents: Arc<dyn IncidentLog>,
}

/// The watcher's only message: "start". The tail loop then runs inside
/// `handle` for the life of the session, which is what puts a panic or an
/// error in it under ractor's supervision rather than in a detached task
/// nobody hears from.
enum WatchMsg {
    Run,
}

impl Actor for SessionWatch {
    type Msg = WatchMsg;
    type State = Option<WatchArgs>;
    type Arguments = WatchArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_message(WatchMsg::Run)?;
        Ok(Some(args))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        _message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // `Run` is sent exactly once, from `pre_start`. Anything else is a bug
        // upstream, not a reason to tail the same log twice.
        let Some(args) = state.take() else {
            return Ok(());
        };
        let result = watch(args).await;
        myself.stop(None);
        result.map_err(ActorProcessingErr::from)
    }
}

/// Tail a session's output, applying Tier-1 rules to every line and Tier-2 LLM
/// review on a timer, until the session finishes or an incident suspends it.
async fn watch(args: WatchArgs) -> anyhow::Result<()> {
    let WatchArgs {
        spec,
        parent,
        incidents,
    } = args;
    let SessionSpec {
        session,
        ssh_target,
        expected_behavior,
        mut tap,
        llm,
        watchdog,
        alerts,
    } = *spec;

    let name = session.session_name.clone();
    let config = watchdog.config().clone();

    let mut poll_interval =
        tokio::time::interval(Duration::from_secs(config.poll_interval_secs.max(1)));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut consecutive_safe: u32 = 0;
    let mut recent_lines: Vec<String> = Vec::new();

    loop {
        // The select's result is bound before anything else runs, so `tap`'s
        // mutable borrow ends here. Handling an incident needs `tap` again (to
        // suspend), and that cannot happen inside a select arm.
        let step = tokio::select! {
            line = tap.next_line() => Step::Line(line),
            _ = poll_interval.tick(), if config.llm_analysis && !recent_lines.is_empty() => Step::Review,
        };

        match step {
            Step::Line(line) => {
                let Some(line) = line? else {
                    // The tail ended without a completion sentinel — the tmux
                    // session or the SSH connection was killed under us.
                    report(&parent, &name, TaskState::Failed);
                    return Ok(());
                };

                if let Some(code_str) = line.strip_prefix("__HIVE_DONE__") {
                    let exit_code: i32 = code_str.trim().parse().unwrap_or(-1);
                    let terminal = if exit_code == 0 {
                        TaskState::Completed
                    } else {
                        TaskState::Failed
                    };
                    report(&parent, &name, terminal);
                    info!("Session '{name}' finished with exit code {exit_code}");
                    return Ok(());
                }

                recent_lines.push(line.clone());
                if recent_lines.len() > config.capture_lines as usize {
                    recent_lines.remove(0);
                }

                if let Some(analysis) = watchdog.scan_line(&line) {
                    handle_incident(
                        tap.as_mut(),
                        &session,
                        &ssh_target,
                        &analysis,
                        &recent_lines,
                        incidents.as_ref(),
                        alerts.as_ref(),
                    )
                    .await;
                    report(&parent, &name, TaskState::PausedByWatchdog);
                    return Ok(());
                }
            }

            Step::Review => {
                let analysis = watchdog
                    .review(&llm, &expected_behavior, &recent_lines.join("\n"))
                    .await;
                if !analysis.is_safe {
                    handle_incident(
                        tap.as_mut(),
                        &session,
                        &ssh_target,
                        &analysis,
                        &recent_lines,
                        incidents.as_ref(),
                        alerts.as_ref(),
                    )
                    .await;
                    report(&parent, &name, TaskState::PausedByWatchdog);
                    return Ok(());
                }
                consecutive_safe += 1;
                if consecutive_safe == config.max_consecutive_safe {
                    poll_interval = tokio::time::interval(Duration::from_secs(
                        config.reduced_poll_interval_secs.max(1),
                    ));
                    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                }
            }
        }
    }
}

/// What one turn of the supervision loop woke up for.
enum Step {
    Line(anyhow::Result<Option<String>>),
    Review,
}

/// Tell the supervisor a session moved state. Best-effort: if the supervisor
/// is gone there is nobody left to tell, and the session's own handling of the
/// transition has already happened.
fn report(parent: &ActorRef<SupervisorMsg>, session: &str, state: TaskState) {
    let _ = parent.send_message(SupervisorMsg::StateChanged {
        session: session.to_string(),
        state,
    });
}

/// Record the incident, suspend the session, and tell the operator.
///
/// # Ordering
///
/// The incident is written **first**, then the session is suspended whatever
/// the write did. Two reasons, in this order of importance:
///
/// 1. Suspension is the safety action; the record is the paperwork. A database
///    that cannot be written must never be able to leave a flagged session
///    running, so the write's result is logged and then ignored.
/// 2. The suspend is a round trip to the worker that takes over a second (it
///    verifies the process group actually reached state `T` before claiming
///    success). Writing first means a master that dies inside that window
///    still left a reviewable record of why.
async fn handle_incident(
    tap: &mut dyn SessionTap,
    session: &SessionInfo,
    ssh_target: &str,
    analysis: &SafetyAnalysis,
    recent_lines: &[String],
    incidents: &dyn IncidentLog,
    alerts: &dyn AlertSink,
) {
    let session_name = session.session_name.as_str();
    warn!(
        "WATCHDOG INCIDENT [{}] session '{session_name}': {}{}",
        analysis.severity,
        analysis
            .category
            .as_ref()
            .map(|c| format!("{c} — "))
            .unwrap_or_default(),
        analysis.reason
    );

    // The whole recent buffer, not just the matched line: a reviewer deciding
    // whether `rm -rf` was a build clean or an accident needs what came before
    // it. `capture_lines` already bounds how much that is.
    let incident = new_incident(
        &session.task_id,
        &session.worker_name,
        session_name,
        analysis.clone(),
        &recent_lines.join("\n"),
    );
    let incident_id = match incidents.record(&incident) {
        Ok(()) => Some(incident.id.clone()),
        Err(e) => {
            error!(
                session = session_name,
                error = %e,
                "FAILED to record the incident — suspending anyway; this incident will not \
                 appear in the review queue"
            );
            None
        }
    };

    // SIGSTOP, not C-c. Interrupting kills the session outright — the shell
    // spawned by `spawn_tmux` has only this one command to run — which
    // destroys the state a reviewer is being told to attach to, and can orphan
    // long-running children. Suspending freezes it intact.
    let outcome = tap.pause().await;

    let title = match &outcome {
        Ok(PauseOutcome::Suspended) => {
            warn!(
                "Session '{session_name}' SUSPENDED for human review. To inspect and take over: \
                 ssh {ssh_target} -t 'tmux attach -t {session_name}'  \
                 (it is stopped; resume with: kill -CONT -<pgid>)"
            );
            format!("Session '{session_name}' suspended for review")
        }
        Ok(PauseOutcome::AlreadyEnded) => {
            warn!(
                "Session '{session_name}' was flagged, but had already finished — nothing left \
                 to suspend. The output is still on the worker for review."
            );
            format!("Session '{session_name}' flagged after it had already finished")
        }
        Err(e) => {
            warn!(
                "Session '{session_name}' flagged and is STILL RUNNING — suspend failed: {e}. \
                 Inspect immediately: ssh {ssh_target} -t 'tmux attach -t {session_name}'"
            );
            format!("Session '{session_name}' flagged and STILL RUNNING")
        }
    };

    // Carries the incident id so the notification can link straight to the
    // review page rather than making the operator find the row.
    alerts
        .deliver(&Alert {
            severity: analysis.severity,
            title,
            reason: analysis.reason.clone(),
            session: session_name.to_string(),
            incident_id,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use hive_common::config::{NotificationConfig, WatchdogConfig};
    use hive_common::protocol::IncidentReviewState;
    use hive_common::{SafetyCategory, Severity};

    // ── Fakes ───────────────────────────────────────────────────────────────

    /// What a fake tap does once its scripted lines run out.
    enum ThenWhat {
        /// End the stream — the "session or transport died" path.
        End,
        /// Never produce another line. Models a live, quiet session, and keeps
        /// a watcher attached for as long as a test needs it.
        Hang,
        /// Panic. Used to prove one watcher's collapse is contained.
        Panic,
    }

    struct FakeTap {
        lines: VecDeque<String>,
        then: ThenWhat,
        paused: Arc<Mutex<Vec<String>>>,
        name: String,
        pause_result: Option<PauseOutcome>,
    }

    impl FakeTap {
        fn new(name: &str, lines: &[&str], then: ThenWhat) -> (Self, Arc<Mutex<Vec<String>>>) {
            let paused = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    lines: lines.iter().map(|s| s.to_string()).collect(),
                    then,
                    paused: paused.clone(),
                    name: name.to_string(),
                    pause_result: Some(PauseOutcome::Suspended),
                },
                paused,
            )
        }
    }

    #[async_trait]
    impl SessionTap for FakeTap {
        async fn next_line(&mut self) -> anyhow::Result<Option<String>> {
            if let Some(line) = self.lines.pop_front() {
                return Ok(Some(line));
            }
            match self.then {
                ThenWhat::End => Ok(None),
                ThenWhat::Hang => std::future::pending().await,
                ThenWhat::Panic => panic!("tail for '{}' blew up", self.name),
            }
        }

        async fn pause(&mut self) -> anyhow::Result<PauseOutcome> {
            self.paused.lock().unwrap().push(self.name.clone());
            match self.pause_result {
                Some(outcome) => Ok(outcome),
                None => anyhow::bail!("pause failed"),
            }
        }
    }

    /// An incident log that always refuses the write.
    struct FailingLog;

    impl IncidentLog for FailingLog {
        fn record(&self, _incident: &Incident) -> anyhow::Result<()> {
            anyhow::bail!("disk is on fire")
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        alerts: Mutex<Vec<Alert>>,
    }

    #[async_trait]
    impl AlertSink for RecordingSink {
        async fn deliver(&self, alert: &Alert) {
            self.alerts.lock().unwrap().push(alert.clone());
        }
    }

    // ── Fixtures ────────────────────────────────────────────────────────────

    fn session(name: &str) -> SessionInfo {
        SessionInfo {
            session_name: name.to_string(),
            worker_name: "worker-a".to_string(),
            task_id: "task-7".to_string(),
            state: TaskState::Running,
            created_at: chrono::Utc::now(),
        }
    }

    /// Tier-2 off, so nothing in this suite can reach for an LLM.
    fn watchdog() -> Arc<Watchdog> {
        Arc::new(
            Watchdog::from_config(WatchdogConfig {
                llm_analysis: false,
                poll_interval_secs: 1,
                ..Default::default()
            })
            .unwrap(),
        )
    }

    fn spec(name: &str, tap: FakeTap, alerts: Arc<dyn AlertSink>) -> SessionSpec {
        SessionSpec {
            session: session(name),
            ssh_target: "user@worker-a".to_string(),
            expected_behavior: "build the project".to_string(),
            tap: Box::new(tap),
            llm: Arc::new(LlmRouter::new(
                "http://localhost:11434".to_string(),
                "test-model".to_string(),
            )),
            watchdog: watchdog(),
            alerts,
        }
    }

    fn sink() -> Arc<RecordingSink> {
        Arc::new(RecordingSink::default())
    }

    /// Poll the registry until `done`, or fail the test. The fakes resolve
    /// immediately, so this settles in a tick or two; the ceiling only exists
    /// so a regression fails instead of hanging CI.
    async fn settle<F>(handle: &SupervisorHandle, mut done: F) -> Vec<SupervisedSession>
    where
        F: FnMut(&[SupervisedSession]) -> bool,
    {
        for _ in 0..400 {
            let snapshot = handle.supervised().await.unwrap();
            if done(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the supervisor never reached the expected state");
    }

    fn find<'a>(list: &'a [SupervisedSession], name: &str) -> &'a SupervisedSession {
        list.iter()
            .find(|s| s.info.session_name == name)
            .unwrap_or_else(|| panic!("'{name}' is not in the registry"))
    }

    // ── Tests ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_tier1_hit_persists_an_incident_with_its_context() {
        let store = IncidentStore::in_memory().unwrap();
        let handle = SupervisorHandle::start(Arc::new(store.clone()))
            .await
            .unwrap();
        let alerts = sink();

        let (tap, paused) = FakeTap::new(
            "hive-a",
            &[
                "Compiling hive-core v0.1.0",
                "warning: unused variable",
                "running: rm -rf /",
            ],
            ThenWhat::Hang,
        );
        handle
            .supervise(spec("hive-a", tap, alerts.clone()))
            .await
            .unwrap();

        let list = settle(&handle, |s| {
            !s.is_empty() && find(s, "hive-a").info.state == TaskState::PausedByWatchdog
        })
        .await;
        assert_eq!(find(&list, "hive-a").info.state, TaskState::PausedByWatchdog);

        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 1, "exactly one incident should be raised");
        let incident = &pending[0];

        // The analysis.
        assert_eq!(incident.analysis.severity, Severity::Critical);
        assert_eq!(
            incident.analysis.category,
            Some(SafetyCategory::DestructiveCommand)
        );
        assert!(incident.analysis.reason.contains("rm -rf /"));
        assert_eq!(incident.review_state, IncidentReviewState::PendingReview);

        // The session identity.
        assert_eq!(incident.tmux_session, "hive-a");
        assert_eq!(incident.worker, "worker-a");
        assert_eq!(incident.task_id, "task-7");

        // The context — not just the matched line. A reviewer judging the hit
        // needs what led up to it.
        assert!(
            incident.flagged_output.contains("Compiling hive-core"),
            "the recent-output buffer should be carried, got: {}",
            incident.flagged_output
        );
        assert!(incident.flagged_output.contains("rm -rf /"));

        // And the session was actually stopped, with the alert pointing at the
        // row that was just written.
        assert_eq!(paused.lock().unwrap().as_slice(), ["hive-a"]);
        let alerts = alerts.alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].incident_id.as_deref(), Some(incident.id.as_str()));
        assert_eq!(alerts[0].session, "hive-a");
    }

    #[tokio::test]
    async fn the_session_is_suspended_even_when_persistence_fails() {
        // Suspension is the safety action; the incident row is the record. A
        // database that cannot be written must never leave a flagged session
        // running.
        let handle = SupervisorHandle::start(Arc::new(FailingLog)).await.unwrap();
        let alerts = sink();

        let (tap, paused) = FakeTap::new("hive-b", &["running: rm -rf /"], ThenWhat::Hang);
        handle
            .supervise(spec("hive-b", tap, alerts.clone()))
            .await
            .unwrap();

        settle(&handle, |s| {
            !s.is_empty() && find(s, "hive-b").info.state == TaskState::PausedByWatchdog
        })
        .await;

        assert_eq!(
            paused.lock().unwrap().as_slice(),
            ["hive-b"],
            "a failed write must not cost the suspension"
        );
        // The alert still goes out, carrying no id — there is no row to link to.
        let alerts = alerts.alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].incident_id.is_none());
    }

    #[tokio::test]
    async fn the_registry_lists_what_it_supervises_and_drops_entries_on_stop() {
        let handle = SupervisorHandle::start(Arc::new(IncidentStore::in_memory().unwrap()))
            .await
            .unwrap();
        let alerts = sink();

        for name in ["hive-1", "hive-2"] {
            let (tap, _) = FakeTap::new(name, &[], ThenWhat::Hang);
            handle
                .supervise(spec(name, tap, alerts.clone()))
                .await
                .unwrap();
        }

        let list = handle.supervised().await.unwrap();
        assert_eq!(list.len(), 2);
        let one = find(&list, "hive-1");
        assert!(one.watching);
        assert_eq!(one.info.worker_name, "worker-a");
        assert_eq!(one.info.task_id, "task-7");
        assert_eq!(one.info.state, TaskState::Running);
        assert!(one.info.created_at <= chrono::Utc::now());

        assert!(handle.stop_session("hive-1").await.unwrap());
        let list = handle.supervised().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].info.session_name, "hive-2");

        // Stopping something unknown is a `false`, not an error.
        assert!(!handle.stop_session("hive-9").await.unwrap());
    }

    #[tokio::test]
    async fn supervising_the_same_session_twice_is_refused() {
        // Two tails on one log would double every Tier-1 hit.
        let handle = SupervisorHandle::start(Arc::new(IncidentStore::in_memory().unwrap()))
            .await
            .unwrap();
        let alerts = sink();

        let (tap, _) = FakeTap::new("hive-dup", &[], ThenWhat::Hang);
        handle
            .supervise(spec("hive-dup", tap, alerts.clone()))
            .await
            .unwrap();

        let (tap, _) = FakeTap::new("hive-dup", &[], ThenWhat::Hang);
        let err = handle
            .supervise(spec("hive-dup", tap, alerts))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already supervised"), "{err}");
        assert_eq!(handle.supervised().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_watcher_failing_does_not_end_the_others() {
        // ractor's default supervision handler stops the supervisor on any
        // child exit. If that default were inherited, the panicking watcher
        // below would take supervision of `hive-live` down with it — silently.
        let handle = SupervisorHandle::start(Arc::new(IncidentStore::in_memory().unwrap()))
            .await
            .unwrap();
        let alerts = sink();

        let (live, _) = FakeTap::new("hive-live", &["still working"], ThenWhat::Hang);
        handle
            .supervise(spec("hive-live", live, alerts.clone()))
            .await
            .unwrap();

        let (doomed, _) = FakeTap::new("hive-doomed", &[], ThenWhat::Panic);
        handle
            .supervise(spec("hive-doomed", doomed, alerts))
            .await
            .unwrap();

        let list = settle(&handle, |s| {
            s.iter()
                .any(|e| e.info.session_name == "hive-doomed" && !e.watching)
        })
        .await;

        assert!(
            find(&list, "hive-live").watching,
            "the surviving session must still be watched"
        );
        assert!(!find(&list, "hive-doomed").watching);

        // And the supervisor is still answering — a third session can start.
        let (extra, _) = FakeTap::new("hive-third", &[], ThenWhat::Hang);
        handle
            .supervise(spec("hive-third", extra, sink()))
            .await
            .expect("the supervisor must still be alive after a child failed");
        assert_eq!(handle.supervised().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_done_sentinel_ends_supervision_with_the_right_terminal_state() {
        let handle = SupervisorHandle::start(Arc::new(IncidentStore::in_memory().unwrap()))
            .await
            .unwrap();

        let (ok, _) = FakeTap::new("hive-ok", &["building", "__HIVE_DONE__0"], ThenWhat::End);
        handle.supervise(spec("hive-ok", ok, sink())).await.unwrap();

        let (bad, _) = FakeTap::new("hive-bad", &["building", "__HIVE_DONE__3"], ThenWhat::End);
        handle
            .supervise(spec("hive-bad", bad, sink()))
            .await
            .unwrap();

        let list = settle(&handle, |s| {
            s.len() == 2 && s.iter().all(|e| !e.watching && is_terminal(e.info.state))
        })
        .await;

        assert_eq!(find(&list, "hive-ok").info.state, TaskState::Completed);
        assert_eq!(find(&list, "hive-bad").info.state, TaskState::Failed);

        // The row survives the watcher: `WorkerPool::delegate` documents
        // `active_sessions()` as how completion is observed.
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn a_tail_that_ends_without_a_sentinel_is_a_failure() {
        let handle = SupervisorHandle::start(Arc::new(IncidentStore::in_memory().unwrap()))
            .await
            .unwrap();

        let (tap, _) = FakeTap::new("hive-cut", &["working"], ThenWhat::End);
        handle
            .supervise(spec("hive-cut", tap, sink()))
            .await
            .unwrap();

        let list = settle(&handle, |s| {
            !s.is_empty() && is_terminal(find(s, "hive-cut").info.state)
        })
        .await;
        assert_eq!(find(&list, "hive-cut").info.state, TaskState::Failed);
    }

    #[tokio::test]
    async fn a_failed_suspend_still_records_and_alerts() {
        // The worst case: flagged, logged, and still running. The operator has
        // to be told, and the record has to exist for them to act on.
        let store = IncidentStore::in_memory().unwrap();
        let handle = SupervisorHandle::start(Arc::new(store.clone()))
            .await
            .unwrap();
        let alerts = sink();

        let (mut tap, _) = FakeTap::new("hive-stuck", &["running: rm -rf /"], ThenWhat::Hang);
        tap.pause_result = None;
        handle
            .supervise(spec("hive-stuck", tap, alerts.clone()))
            .await
            .unwrap();

        settle(&handle, |s| {
            !s.is_empty() && find(s, "hive-stuck").info.state == TaskState::PausedByWatchdog
        })
        .await;

        assert_eq!(store.pending().unwrap().len(), 1);
        let alerts = alerts.alerts.lock().unwrap();
        assert!(
            alerts[0].title.contains("STILL RUNNING"),
            "got: {}",
            alerts[0].title
        );
    }

    #[test]
    fn the_notifier_is_an_alert_sink() {
        // Compile-time check that the real implementation still fits the seam;
        // nothing is delivered.
        let notifier: Arc<dyn AlertSink> =
            Arc::new(Notifier::from_config(NotificationConfig::default()));
        let _ = notifier;
    }
}
