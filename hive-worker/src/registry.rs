//! In-memory task registry.
//!
//! `GET /status/{id}` used to answer a hardcoded `Running` for any id, real or
//! not. This is the thing that makes it tell the truth: one record per task the
//! daemon has accepted, updated as the executor moves it through its states.
//!
//! Deliberately not persisted. A worker daemon restart means its tmux sessions
//! are the only surviving state, and inventing a task table that disagrees with
//! `tmux ls` would be worse than admitting the task is gone — the master
//! re-drives anything it still cares about.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use hive_common::{TaskAssignment, TaskState, TaskStatus};
use tokio::sync::RwLock;

/// Everything the daemon knows about one accepted task.
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub task_id: String,
    pub description: String,
    pub tmux_session: String,
    pub state: TaskState,
    /// Set once the task reaches a terminal state.
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// Path to the on-disk log holding this task's combined output.
    pub log_path: String,
    /// Process group suspended by `pause`, needed to resume the right one.
    /// Bash reclaims the terminal when a job stops, so the tty's foreground
    /// group is no longer the stopped work.
    pub paused_pgid: Option<i32>,
    pub accepted_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TaskRecord {
    fn new(task: &TaskAssignment, log_path: String) -> Self {
        Self {
            task_id: task.task_id.clone(),
            description: task.description.clone(),
            tmux_session: task.tmux_session_name.clone(),
            state: TaskState::Queued,
            exit_code: None,
            error: None,
            log_path,
            paused_pgid: None,
            accepted_at: Utc::now(),
            finished_at: None,
        }
    }

    /// Whether this task has reached a state it will not leave on its own.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// Project into the wire type the master expects.
    pub fn to_status(&self, worker_name: &str, output: Option<String>) -> TaskStatus {
        let mut status = TaskStatus::new(&self.task_id, self.state);
        status.tmux_session = Some(self.tmux_session.clone());
        status.worker_name = Some(worker_name.to_string());
        status.exit_code = self.exit_code;
        status.error = self.error.clone();
        status.output = output;
        status
    }
}

/// Shared, concurrent registry of tasks.
#[derive(Clone, Default)]
pub struct Registry {
    tasks: Arc<RwLock<HashMap<String, TaskRecord>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly accepted task. Returns an error if the id is already
    /// known — a repeated `POST /task` for a live task is a master-side bug,
    /// and silently starting a second tmux session for it would be worse than
    /// refusing.
    pub async fn accept(&self, task: &TaskAssignment, log_path: String) -> anyhow::Result<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(existing) = tasks.get(&task.task_id) {
            anyhow::bail!(
                "task '{}' already exists in state {}",
                task.task_id,
                existing.state
            );
        }
        tasks.insert(task.task_id.clone(), TaskRecord::new(task, log_path));
        Ok(())
    }

    pub async fn get(&self, task_id: &str) -> Option<TaskRecord> {
        self.tasks.read().await.get(task_id).cloned()
    }

    pub async fn list(&self) -> Vec<TaskRecord> {
        let mut all: Vec<_> = self.tasks.read().await.values().cloned().collect();
        all.sort_by(|a, b| b.accepted_at.cmp(&a.accepted_at));
        all
    }

    /// Move a task to a new state. Terminal states stamp `finished_at`.
    ///
    /// Returns the updated record, or `None` if the task is unknown.
    pub async fn set_state(&self, task_id: &str, state: TaskState) -> Option<TaskRecord> {
        let mut tasks = self.tasks.write().await;
        let record = tasks.get_mut(task_id)?;
        record.state = state;
        if matches!(
            state,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        ) {
            record.finished_at = Some(Utc::now());
        }
        Some(record.clone())
    }

    /// Remember (or clear) the process group suspended by `pause`.
    pub async fn set_paused_pgid(&self, task_id: &str, pgid: Option<i32>) {
        if let Some(record) = self.tasks.write().await.get_mut(task_id) {
            record.paused_pgid = pgid;
        }
    }

    /// Record a terminal outcome carrying an exit code.
    ///
    /// A task that already reached a terminal state keeps it. Killing a task
    /// makes its executor observe the session vanish and try to report a
    /// failure a moment later; without this the deliberate `Cancelled` would
    /// be overwritten by an incidental `Failed`, and the record would say the
    /// task broke rather than that someone stopped it.
    pub async fn finish(&self, task_id: &str, exit_code: i32) -> Option<TaskRecord> {
        let mut tasks = self.tasks.write().await;
        let record = tasks.get_mut(task_id)?;
        if record.is_terminal() {
            return Some(record.clone());
        }
        record.exit_code = Some(exit_code);
        record.state = if exit_code == 0 {
            TaskState::Completed
        } else {
            TaskState::Failed
        };
        record.finished_at = Some(Utc::now());
        Some(record.clone())
    }

    /// Record a failure that never produced an exit code (timeout, tmux error).
    ///
    /// Terminal states are sticky here too — see [`Registry::finish`].
    pub async fn fail(&self, task_id: &str, error: impl Into<String>) -> Option<TaskRecord> {
        let mut tasks = self.tasks.write().await;
        let record = tasks.get_mut(task_id)?;
        if record.is_terminal() {
            return Some(record.clone());
        }
        record.error = Some(error.into());
        record.state = TaskState::Failed;
        record.finished_at = Some(Utc::now());
        Some(record.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::TaskCommand;

    fn task(id: &str) -> TaskAssignment {
        let mut t = TaskAssignment::new(
            "test task",
            vec![TaskCommand::new("echo hi")],
            format!("hive-{id}"),
        );
        t.task_id = id.to_string();
        t
    }

    #[tokio::test]
    async fn unknown_tasks_are_reported_as_unknown_not_running() {
        // The bug this registry exists to fix: the old handler answered
        // `Running` for any id at all, including ones it had never seen.
        let reg = Registry::new();
        assert!(reg.get("never-submitted").await.is_none());
    }

    #[tokio::test]
    async fn accepts_then_tracks_state_through_completion() {
        let reg = Registry::new();
        reg.accept(&task("t1"), "/tmp/t1.log".into()).await.unwrap();

        let r = reg.get("t1").await.expect("registered");
        assert_eq!(r.state, TaskState::Queued);
        assert!(!r.is_terminal());
        assert!(r.finished_at.is_none());

        reg.set_state("t1", TaskState::Running).await.unwrap();
        assert_eq!(reg.get("t1").await.unwrap().state, TaskState::Running);

        let done = reg.finish("t1", 0).await.unwrap();
        assert_eq!(done.state, TaskState::Completed);
        assert_eq!(done.exit_code, Some(0));
        assert!(done.is_terminal());
        assert!(done.finished_at.is_some());
    }

    #[tokio::test]
    async fn nonzero_exit_marks_failed() {
        let reg = Registry::new();
        reg.accept(&task("t2"), "/tmp/t2.log".into()).await.unwrap();
        let done = reg.finish("t2", 127).await.unwrap();
        assert_eq!(done.state, TaskState::Failed);
        assert_eq!(done.exit_code, Some(127));
    }

    #[tokio::test]
    async fn failure_without_exit_code_still_terminal() {
        let reg = Registry::new();
        reg.accept(&task("t3"), "/tmp/t3.log".into()).await.unwrap();
        let failed = reg.fail("t3", "timed out after 30s").await.unwrap();
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.exit_code, None);
        assert!(failed.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn cancelling_survives_the_executors_late_failure_report() {
        // Killing a task makes its executor see the tmux session disappear and
        // report a failure moments later. The record must still say the task
        // was cancelled, not that it broke.
        let reg = Registry::new();
        reg.accept(&task("k1"), "/tmp/k1.log".into()).await.unwrap();
        reg.set_state("k1", TaskState::Cancelled).await;

        reg.fail("k1", "tmux session disappeared").await;
        assert_eq!(reg.get("k1").await.unwrap().state, TaskState::Cancelled);

        reg.finish("k1", 1).await;
        let r = reg.get("k1").await.unwrap();
        assert_eq!(r.state, TaskState::Cancelled);
        assert_eq!(r.exit_code, None, "a cancelled task has no exit code to report");
    }

    #[tokio::test]
    async fn duplicate_task_id_is_refused() {
        let reg = Registry::new();
        reg.accept(&task("dup"), "/tmp/a.log".into()).await.unwrap();
        let err = reg.accept(&task("dup"), "/tmp/b.log".into()).await;
        assert!(err.is_err(), "must not start a second session for a live id");
    }

    #[tokio::test]
    async fn status_projection_carries_worker_and_session() {
        let reg = Registry::new();
        reg.accept(&task("t4"), "/tmp/t4.log".into()).await.unwrap();
        reg.finish("t4", 0).await;
        let r = reg.get("t4").await.unwrap();
        let status = r.to_status("lawfinder", Some("output".into()));
        assert_eq!(status.worker_name.as_deref(), Some("lawfinder"));
        assert_eq!(status.tmux_session.as_deref(), Some("hive-t4"));
        assert_eq!(status.exit_code, Some(0));
        assert_eq!(status.output.as_deref(), Some("output"));
    }

    #[tokio::test]
    async fn list_is_newest_first() {
        let reg = Registry::new();
        for id in ["a", "b", "c"] {
            reg.accept(&task(id), format!("/tmp/{id}.log")).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let ids: Vec<_> = reg.list().await.into_iter().map(|r| r.task_id).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
    }
}
