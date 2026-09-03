//! Worker pool — manages SSH connections to worker machines and delegates
//! tasks to supervised tmux sessions on them.

pub mod ssh;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hive_common::{
    SafetyAnalysis, SessionInfo, TaskAssignment, TaskState, WorkerInfo, WorkerStatus,
};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::llm::LlmRouter;
use crate::watchdog::Watchdog;
use ssh::SshWorker;

/// Ceiling on one worker health probe.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Pool of worker machines for task delegation.
pub struct WorkerPool {
    /// Worker definitions loaded from config.
    pub workers: Vec<WorkerNode>,
    sessions: Arc<Mutex<HashMap<String, SessionInfo>>>,
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
            sessions: Arc::new(Mutex::new(HashMap::new())),
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

    /// Delegate `task` to `worker`: SSH in, start a supervised tmux
    /// session on it, and spawn a background monitor that applies the
    /// watchdog's Tier-1/Tier-2 checks to its output as it streams in.
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
        self.sessions
            .lock()
            .await
            .insert(task.tmux_session_name.clone(), session_info.clone());

        let sessions = self.sessions.clone();
        let session_name = task.tmux_session_name.clone();
        let expected_behavior = task
            .expected_behavior
            .clone()
            .unwrap_or_else(|| task.description.clone());

        tokio::spawn(async move {
            let session_name_for_log = session_name.clone();
            let result = supervise(SuperviseParams {
                ssh,
                session_name,
                ssh_target,
                log_path,
                expected_behavior,
                llm,
                watchdog,
                sessions,
            })
            .await;
            if let Err(e) = result {
                warn!("Supervisor for session '{session_name_for_log}' ended with error: {e}");
            }
        });

        Ok(session_info)
    }

    /// Currently tracked delegated sessions (across all workers).
    pub async fn active_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.lock().await.values().cloned().collect()
    }
}

/// Owned inputs for [`supervise`] — grouped so the spawned task can move
/// everything in one shot without a long argument list.
struct SuperviseParams {
    ssh: SshWorker,
    session_name: String,
    ssh_target: String,
    log_path: String,
    expected_behavior: String,
    llm: Arc<LlmRouter>,
    watchdog: Arc<Watchdog>,
    sessions: Arc<Mutex<HashMap<String, SessionInfo>>>,
}

/// Tail a delegated session's log, applying Tier-1 rules to every line and
/// Tier-2 LLM review on a timer, until the task finishes or an incident
/// pauses it.
async fn supervise(params: SuperviseParams) -> anyhow::Result<()> {
    let SuperviseParams {
        ssh,
        session_name,
        ssh_target,
        log_path,
        expected_behavior,
        llm,
        watchdog,
        sessions,
    } = params;
    let session_name = session_name.as_str();
    let ssh_target = ssh_target.as_str();
    let expected_behavior = expected_behavior.as_str();

    let mut tail = ssh.tail(&log_path).await?;
    let config = watchdog.config().clone();

    let mut poll_interval =
        tokio::time::interval(Duration::from_secs(config.poll_interval_secs.max(1)));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut consecutive_safe: u32 = 0;
    let mut recent_lines: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            line = tail.next_line() => {
                let Some(line) = line? else {
                    // Remote tail process ended without a completion sentinel
                    // (e.g. the tmux session or SSH connection was killed).
                    if let Some(info) = sessions.lock().await.get_mut(session_name) {
                        info.state = TaskState::Failed;
                    }
                    return Ok(());
                };

                if let Some(code_str) = line.strip_prefix("__HIVE_DONE__") {
                    let exit_code: i32 = code_str.trim().parse().unwrap_or(-1);
                    if let Some(info) = sessions.lock().await.get_mut(session_name) {
                        info.state = if exit_code == 0 { TaskState::Completed } else { TaskState::Failed };
                    }
                    info!("Session '{session_name}' finished with exit code {exit_code}");
                    return Ok(());
                }

                recent_lines.push(line.clone());
                if recent_lines.len() > config.capture_lines as usize {
                    recent_lines.remove(0);
                }

                if let Some(analysis) = watchdog.scan_line(&line) {
                    handle_incident(&ssh, session_name, ssh_target, &analysis, &sessions).await;
                    return Ok(());
                }
            }
            _ = poll_interval.tick(), if config.llm_analysis && !recent_lines.is_empty() => {
                let analysis = watchdog.review(&llm, expected_behavior, &recent_lines.join("\n")).await;
                if !analysis.is_safe {
                    handle_incident(&ssh, session_name, ssh_target, &analysis, &sessions).await;
                    return Ok(());
                }
                consecutive_safe += 1;
                if consecutive_safe == config.max_consecutive_safe {
                    poll_interval = tokio::time::interval(Duration::from_secs(config.reduced_poll_interval_secs.max(1)));
                    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                }
            }
        }
    }
}

/// Pause the session (not kill it — preserves state for human review),
/// mark it in the registry, and log a handover notification with the
/// exact command to reattach. Full incident logging / IncidentReviewState
/// tracking / push notifications are still Phase 10; this is the minimum
/// needed to not run an unattended session with zero safety net.
async fn handle_incident(
    ssh: &SshWorker,
    session_name: &str,
    ssh_target: &str,
    analysis: &SafetyAnalysis,
    sessions: &Arc<Mutex<HashMap<String, SessionInfo>>>,
) {
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

    // SIGSTOP, not C-c. Interrupting kills the session outright — the shell
    // spawned by `spawn_tmux` has only this one command to run — which
    // destroys the state a reviewer is being told to attach to, and can orphan
    // long-running children. Suspending freezes it intact.
    let outcome = ssh.pause_session(session_name).await;

    if let Some(info) = sessions.lock().await.get_mut(session_name) {
        info.state = TaskState::PausedByWatchdog;
    }

    match outcome {
        Ok(crate::workers::ssh::PauseOutcome::Suspended) => warn!(
            "Session '{session_name}' SUSPENDED for human review. To inspect and take over: \
             ssh {ssh_target} -t 'tmux attach -t {session_name}'  \
             (it is stopped; resume with: kill -CONT -<pgid>)"
        ),
        Ok(crate::workers::ssh::PauseOutcome::AlreadyEnded) => warn!(
            "Session '{session_name}' was flagged, but had already finished — nothing left \
             to suspend. The output is still on the worker for review."
        ),
        Err(e) => warn!(
            "Session '{session_name}' flagged and is STILL RUNNING — suspend failed: {e}. \
             Inspect immediately: ssh {ssh_target} -t 'tmux attach -t {session_name}'"
        ),
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
