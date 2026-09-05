//! Worker pool — manages SSH connections to worker machines and delegates
//! tasks to supervised tmux sessions on them.

pub mod sessions;
pub mod ssh;

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hive_common::{SessionInfo, TaskAssignment, TaskState, WorkerInfo, WorkerStatus};
use tokio::sync::OnceCell;
use tracing::{error, info, warn};

use crate::llm::LlmRouter;
use crate::watchdog::notifier::Notifier;
use crate::watchdog::supervisor::{
    open_incident_log, SessionSpec, SessionTap, SupervisorHandle,
};
use crate::watchdog::Watchdog;
use ssh::{LogTail, PauseOutcome, SshWorker};

/// Ceiling on one worker health probe.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Pool of worker machines for task delegation.
pub struct WorkerPool {
    /// Worker definitions loaded from config.
    pub workers: Vec<WorkerNode>,
    /// The one supervisor watching everything this pool has delegated.
    ///
    /// Started on first use rather than in [`WorkerPool::new`]: `new` is sync
    /// and called from both binaries' startup paths, and spawning an actor is
    /// not. A pool that never delegates never pays for a supervisor.
    supervisor: OnceCell<SupervisorHandle>,
}

/// A worker node with connection state.
pub struct WorkerNode {
    /// Static worker info from config.
    pub info: WorkerInfo,
    /// Current status, held atomically so health can be refreshed through a
    /// shared `&self`.
    ///
    /// The master owns its `WorkerPool` inside an `Arc<MasterAgent>` and hands
    /// it to request handlers, so there is no `&mut` to take when a background
    /// task wants to re-probe reachability. Encoded as a `u8` for the same
    /// reason `active_tasks` is an `AtomicUsize`.
    status: AtomicU8,
    /// Number of active tasks on this worker.
    pub active_tasks: AtomicUsize,
}

/// Encode/decode `WorkerStatus` for atomic storage.
fn status_to_u8(status: WorkerStatus) -> u8 {
    match status {
        WorkerStatus::Online => 0,
        WorkerStatus::Busy => 1,
        WorkerStatus::Offline => 2,
        WorkerStatus::Unhealthy => 3,
    }
}

fn status_from_u8(value: u8) -> WorkerStatus {
    match value {
        0 => WorkerStatus::Online,
        1 => WorkerStatus::Busy,
        3 => WorkerStatus::Unhealthy,
        // Anything unexpected is treated as offline: refusing to place work on
        // a worker whose state we cannot read is the safe direction.
        _ => WorkerStatus::Offline,
    }
}

impl WorkerNode {
    pub fn status(&self) -> WorkerStatus {
        status_from_u8(self.status.load(Ordering::Relaxed))
    }

    pub fn set_status(&self, status: WorkerStatus) {
        self.status.store(status_to_u8(status), Ordering::Relaxed);
    }

    pub fn is_online(&self) -> bool {
        self.status() == WorkerStatus::Online
    }
}

impl WorkerPool {
    /// Create a new worker pool from worker configurations.
    pub fn new(workers: Vec<WorkerInfo>) -> Self {
        let nodes = workers
            .into_iter()
            .map(|info| WorkerNode {
                info,
                status: AtomicU8::new(status_to_u8(WorkerStatus::Offline)),
                active_tasks: AtomicUsize::new(0),
            })
            .collect();

        Self {
            workers: nodes,
            supervisor: OnceCell::new(),
        }
    }

    /// Select the least-loaded online worker.
    pub fn select_worker(&self) -> Option<&WorkerNode> {
        self.workers
            .iter()
            .filter(|w| w.is_online())
            .min_by_key(|w| w.active_tasks.load(Ordering::Relaxed))
    }

    /// Get the number of online workers.
    pub fn online_count(&self) -> usize {
        self.workers
            .iter()
            .filter(|w| w.is_online())
            .count()
    }

    /// Probe every configured worker over SSH and flip its status between
    /// `Online`/`Offline` based on real reachability.
    /// Re-probe every worker's reachability.
    ///
    /// Takes `&self`, so a long-lived master can run this on a timer while
    /// handlers are using the same pool. Each probe is bounded: SSH's own
    /// `ConnectTimeout` does not cover every stall, and one wedged host must
    /// not hold up the rest of the fleet.
    pub async fn refresh_health(&self) {
        for worker in &self.workers {
            let target = worker.info.ssh_target();
            let probe = async {
                match SshWorker::connect(&target).await {
                    Ok(ssh) => ssh.ping().await.is_ok(),
                    Err(e) => {
                        warn!("Worker '{}' unreachable: {e}", worker.info.name);
                        false
                    }
                }
            };
            let reachable = tokio::time::timeout(HEALTH_PROBE_TIMEOUT, probe)
                .await
                .unwrap_or_else(|_| {
                    warn!("Worker '{}' health probe timed out", worker.info.name);
                    false
                });

            let status = if reachable {
                WorkerStatus::Online
            } else {
                WorkerStatus::Offline
            };
            if status != worker.status() {
                info!("Worker '{}' health: {:?}", worker.info.name, status);
            }
            worker.set_status(status);
        }
    }

    /// The supervisor watching this pool's sessions, starting it if this is
    /// the first delegation.
    ///
    /// Its incident log is the configured one (`~/.hive/hive.db`), falling
    /// back to an ephemeral log if that cannot be opened — losing the history
    /// is survivable, refusing to supervise is not.
    pub async fn supervisor(&self) -> anyhow::Result<&SupervisorHandle> {
        self.supervisor
            .get_or_try_init(|| async {
                SupervisorHandle::start(Arc::new(open_incident_log())).await
            })
            .await
    }

    /// Delegate `task` to `worker`: SSH in, start a supervised tmux
    /// session on it, and hand the session to the central supervisor, which
    /// applies the watchdog's Tier-1/Tier-2 checks to its output as it
    /// streams in.
    ///
    /// Returns as soon as the remote session is confirmed started — the
    /// caller does not block on the delegated command finishing. Track
    /// completion via [`WorkerPool::active_sessions`].
    pub async fn delegate(
        &self,
        worker: &WorkerNode,
        task: TaskAssignment,
        llm: Arc<LlmRouter>,
        watchdog: Arc<Watchdog>,
    ) -> anyhow::Result<SessionInfo> {
        let ssh_target = worker.info.ssh_target();
        let ssh = SshWorker::connect(&ssh_target).await?;

        let command = task
            .commands
            .iter()
            .map(|c| c.command.as_str())
            .collect::<Vec<_>>()
            .join(" && ");
        if command.is_empty() {
            anyhow::bail!("task '{}' has no commands to delegate", task.task_id);
        }
        let log_path = format!("/tmp/{}.log", task.tmux_session_name);

        // The supervisor is started before the remote session, not after: if
        // it cannot start, nothing would be watching the session we are about
        // to create, and an unwatched remote session is the state this whole
        // subsystem exists to prevent. Failing here costs one delegation;
        // failing after `spawn_tmux` would leave real work running blind.
        let supervisor = self.supervisor().await?;

        ssh.spawn_tmux(&task.tmux_session_name, &command, &log_path)
            .await?;
        worker.active_tasks.fetch_add(1, Ordering::Relaxed);

        let session_info = SessionInfo {
            session_name: task.tmux_session_name.clone(),
            worker_name: worker.info.name.clone(),
            task_id: task.task_id.clone(),
            state: TaskState::Running,
            created_at: chrono::Utc::now(),
        };

        let expected_behavior = task
            .expected_behavior
            .clone()
            .unwrap_or_else(|| task.description.clone());

        // The tail is opened here, on the connection that just started the
        // session, so a failure to attach to the log is reported to the caller
        // rather than surfacing minutes later inside a detached task.
        //
        // Past this point the remote work is already running, so both failures
        // below leave a live, *unwatched* session behind. That is not something
        // to clean up by killing it — killing is what destroys the state a
        // human would need — so it is reported instead, with the command to go
        // and look. Returning `Err` while the session runs is the honest
        // answer: the caller was promised supervision and did not get it.
        let tail = match ssh.tail(&log_path).await {
            Ok(tail) => tail,
            Err(e) => {
                error!(
                    "Session '{}' is RUNNING UNWATCHED on '{}': could not tail its log ({e}). \
                     Inspect: ssh {ssh_target} -t 'tmux attach -t {}'",
                    task.tmux_session_name, worker.info.name, task.tmux_session_name
                );
                return Err(e);
            }
        };
        let tap = SshTap {
            ssh,
            tail,
            session_name: task.tmux_session_name.clone(),
        };

        if let Err(e) = supervisor
            .supervise(SessionSpec {
                session: session_info.clone(),
                ssh_target: ssh_target.clone(),
                expected_behavior,
                tap: Box::new(tap),
                llm,
                alerts: Arc::new(Notifier::from_config(
                    watchdog.config().notifications.clone(),
                )),
                watchdog,
            })
            .await
        {
            error!(
                "Session '{}' is RUNNING UNWATCHED on '{}': the supervisor refused it ({e}). \
                 Inspect: ssh {ssh_target} -t 'tmux attach -t {}'",
                task.tmux_session_name, worker.info.name, task.tmux_session_name
            );
            return Err(e);
        }

        Ok(session_info)
    }

    /// Currently tracked delegated sessions (across all workers).
    ///
    /// The supervisor's registry is the source: there is no second map here to
    /// drift out of step with what is actually being watched. Empty before the
    /// first delegation, because nothing is being supervised yet.
    ///
    /// Still process-local, and still not what `hive sessions` should ask —
    /// see [`crate::workers::sessions`] for why that command reads `tmux ls`
    /// instead.
    pub async fn active_sessions(&self) -> Vec<SessionInfo> {
        let Some(supervisor) = self.supervisor.get() else {
            return Vec::new();
        };
        match supervisor.supervised().await {
            Ok(sessions) => sessions.into_iter().map(|s| s.info).collect(),
            Err(e) => {
                warn!("could not read the supervisor's registry: {e}");
                Vec::new()
            }
        }
    }
}

/// The [`SessionTap`] the real system uses.
///
/// `tail -f` over the worker's pooled SSH connection for output, and that same
/// connection for the suspend. Both halves are held together so supervision
/// owns exactly one connection per session and cannot end up suspending
/// through a connection that has since been dropped.
struct SshTap {
    ssh: SshWorker,
    tail: LogTail,
    session_name: String,
}

#[async_trait]
impl SessionTap for SshTap {
    async fn next_line(&mut self) -> anyhow::Result<Option<String>> {
        self.tail.next_line().await
    }

    async fn pause(&mut self) -> anyhow::Result<PauseOutcome> {
        self.ssh.pause_session(&self.session_name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::TaskCommand;

    /// Exercises real SSH delegation and the Tier-1 watchdog pause against
    /// whatever worker `hive-worker-1` resolves to in `~/.ssh/config` on
    /// the machine running the test. Not run by default — no such host
    /// exists in CI or on a fresh checkout. Run explicitly with:
    ///   cargo test -p hive-core --lib -- --ignored live_delegation_pauses_on_tier1_match
    #[tokio::test]
    #[ignore = "requires a real, reachable worker configured as the `hive-worker-1` SSH alias"]
    async fn live_delegation_pauses_on_tier1_match() {
        let worker_info = hive_common::WorkerInfo {
            name: "test-worker".to_string(),
            host: "hive-worker-1".to_string(),
            user: "azureuser".to_string(),
            port: None,
            tags: vec![],
        };
        let pool = WorkerPool::new(vec![worker_info]);
        let worker = &pool.workers[0];

        // Tier-2 (LLM review) is disabled so this test only depends on the
        // SSH worker being reachable, not on a local Ollama instance.
        let watchdog = Arc::new(
            Watchdog::from_config(hive_common::config::WatchdogConfig {
                llm_analysis: false,
                poll_interval_secs: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        let llm = Arc::new(LlmRouter::new(
            "http://localhost:11434".to_string(),
            "qwen2.5:14b-instruct-q4_K_M".to_string(),
        ));

        // Echoes the literal string "rm -rf /" — never actually runs it.
        // The point is to prove the Tier-1 regex scan over real remote
        // output pauses the session, not to test `rm` itself.
        let task = TaskAssignment::new(
            "tier-1 pause test",
            vec![TaskCommand::new("echo 'about to rm -rf / for real'")],
            format!("hive-test-{}", uuid::Uuid::new_v4()),
        );

        let session = pool
            .delegate(worker, task, llm, watchdog)
            .await
            .expect("delegation should succeed against a reachable worker");

        // Give the background supervisor time to tail the line and react.
        let mut paused = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let sessions = pool.active_sessions().await;
            if let Some(info) = sessions
                .iter()
                .find(|s| s.session_name == session.session_name)
            {
                if info.state == TaskState::PausedByWatchdog {
                    paused = true;
                    break;
                }
            }
        }

        assert!(
            paused,
            "expected the session to be paused by the Tier-1 watchdog"
        );
    }
}
