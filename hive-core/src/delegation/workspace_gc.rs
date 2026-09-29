//! Conservative, device-local cache collection for terminal workspaces.
use super::{
    store::{Run, RunStore},
    transport,
};
use crate::agent::MasterAgent;
use serde_json::{json, Value};

pub const GRACE_SECONDS: i64 = 2 * 60 * 60;
pub const SCRIPT: &str = include_str!("workspace_gc.py");
pub fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "superseded")
}
pub fn eligible(run: &Run, runs: &[Run], now: i64) -> bool {
    terminal(&run.state)
        && run.metadata["workspace_gc"].is_null()
        && run.metadata["terminal_since"]
            .as_i64()
            .is_some_and(|since| now.saturating_sub(since) >= GRACE_SECONDS)
        && !runs.iter().any(|other| {
            other.assignment.device == run.assignment.device
                && !terminal(&other.state)
                && (other
                    .assignment
                    .workspace
                    .starts_with(&run.assignment.workspace)
                    || run
                        .assignment
                        .workspace
                        .starts_with(&other.assignment.workspace))
        })
}
pub async fn collect(agent: &MasterAgent, store: &RunStore) -> anyhow::Result<()> {
    let (runs, errors) = store.list()?;
    // An unreadable row may be a live run sharing a workspace; its caches must survive.
    if errors > 0 {
        tracing::warn!(errors, "workspace cache collection skipped: unreadable delegated runs");
        return Ok(());
    }
    for run in &runs {
        if !eligible(run, &runs, chrono::Utc::now().timestamp()) {
            continue;
        }
        let Some(worker) = super::target(agent, &run.assignment.device) else {
            continue;
        };
        // Completed runners retain tmux sessions for followups. The script
        // locks their journal and checks current state before touching caches.
        let command = format!("python3 -c {}", transport::quote(SCRIPT));
        let input = json!({"workspace":run.assignment.workspace, "run_id":run.id,
            "journal":run.runner_path.is_some(), "superseded":run.state == "superseded", "tmux":run.tmux_name,
            "live":runs.iter().filter(|r| r.assignment.device == run.assignment.device && !terminal(&r.state))
                .map(|r| &r.assignment.workspace).collect::<Vec<_>>()});
        match transport::ssh_timeout(&worker, &command, Some(&input), 120).await {
            Ok(raw) => {
                let mut result: Value = serde_json::from_str(&raw)?;
                result["collected_at"] = json!(chrono::Utc::now().to_rfc3339());
                store.record_workspace_gc(
                    &run.id,
                    run.metadata["terminal_since"].as_i64().unwrap(),
                    &result,
                )?;
            }
            Err(error) => {
                tracing::warn!(run_id=%run.id, %error, "workspace cache collection deferred")
            }
        }
    }
    Ok(())
}
