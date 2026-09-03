//! Status callbacks to the master.
//!
//! `POST /task` returns immediately, so the master learns about progress one of
//! two ways: it polls `GET /status/{id}`, or the worker pushes. Pushing is
//! strictly better for the watchdog — a task that fails two seconds in should
//! not wait for the next poll — but it is best-effort by design.
//!
//! A worker whose master is unreachable keeps running its tasks and keeps its
//! registry correct. Losing the callback must never lose the work, so every
//! failure here is logged and swallowed; the master's own polling is the
//! backstop.

use std::time::Duration;

use tracing::{debug, warn};

use crate::executor;
use crate::registry::Registry;

/// Lines of task output to include in a pushed status.
const OUTPUT_LINES: usize = 50;

/// Callback timeout. Short on purpose: this runs inline with task state
/// transitions, and a hung master must not stall the executor.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Pushes `TaskStatus` to the master, if one is configured.
#[derive(Clone)]
pub struct Reporter {
    /// Master base URL, e.g. `http://100.121.248.111:9090`. `None` disables
    /// pushing entirely and leaves the master to poll.
    endpoint: Option<String>,
    /// Shared secret the master expects, sent as a bearer token. Must match the
    /// master's `HIVE_WORKER_TOKEN`, or callbacks are refused with 401.
    token: Option<String>,
    http: reqwest::Client,
}

impl Reporter {
    /// Build from `HIVE_MASTER_URL`.
    pub fn from_env() -> Self {
        let endpoint = std::env::var("HIVE_MASTER_URL")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());

        match &endpoint {
            Some(url) => tracing::info!(master = %url, "status callbacks enabled"),
            None => tracing::info!("HIVE_MASTER_URL unset — master must poll for status"),
        }

        let token = std::env::var("HIVE_WORKER_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        if endpoint.is_some() && token.is_none() {
            tracing::warn!(
                "HIVE_WORKER_TOKEN unset — callbacks will be refused if the master requires one"
            );
        }

        Self {
            endpoint,
            token,
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// A reporter that does nothing. Used in tests and when running standalone.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn disabled() -> Self {
        Self {
            endpoint: None,
            token: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    /// Push the current status of `task_id`, attaching recent output.
    ///
    /// Never returns an error: see the module note — a failed callback is a
    /// logged event, not a task failure.
    pub async fn report(&self, registry: &Registry, task_id: &str, worker_name: &str) {
        let Some(endpoint) = &self.endpoint else {
            return;
        };
        let Some(record) = registry.get(task_id).await else {
            warn!(task = task_id, "asked to report an unknown task");
            return;
        };

        let output = executor::tail_log(&record.log_path, OUTPUT_LINES).await;
        let status = record.to_status(worker_name, output);
        let url = format!("{endpoint}/api/worker/status");

        let mut request = self.http.post(&url).json(&status);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                debug!(task = task_id, state = %record.state, "status pushed");
            }
            Ok(response) => warn!(
                task = task_id,
                status = %response.status(),
                "master rejected status callback"
            ),
            Err(e) => warn!(task = task_id, error = %e, "status callback failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::{TaskAssignment, TaskCommand};

    #[tokio::test]
    async fn disabled_reporter_is_a_no_op_and_reports_itself_disabled() {
        let reporter = Reporter::disabled();
        assert!(!reporter.is_enabled());

        // Must not panic or block when there is nowhere to send.
        let registry = Registry::new();
        let task = TaskAssignment::new("t", vec![TaskCommand::new("echo hi")], "hive-x");
        registry.accept(&task, "/tmp/none.log".into()).await.unwrap();
        reporter.report(&registry, &task.task_id, "worker").await;
    }

    #[tokio::test]
    async fn reporting_an_unknown_task_is_survivable() {
        let reporter = Reporter::disabled();
        reporter.report(&Registry::new(), "does-not-exist", "worker").await;
    }
}
