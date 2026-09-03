//! Client for a running master daemon.
//!
//! `hive task` used to build a `MasterAgent` in-process, delegate, and exit —
//! which meant the watchdog supervising the delegated tmux session was a
//! `tokio::spawn` task inside a process that terminated moments later. The
//! remote command kept running on the worker with nothing watching it.
//!
//! Submitting to the long-lived master instead fixes that at the root: the
//! supervisor task lives in `hive-web`, which runs under launchd/systemd, so a
//! delegated session is watched for its whole life rather than for the few
//! hundred milliseconds the CLI happened to stay alive.

use std::time::Duration;

use hive_core::agent::run::{PlannedRun, RunResult};
use serde::{Deserialize, Serialize};

/// Mirrors `hive_web::chat::ChatReply`.
#[derive(Debug, Deserialize)]
pub struct ChatReply {
    pub run: PlannedRun,
    pub result: RunResult,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    message: &'a str,
    project_id: Option<&'a str>,
}

#[derive(Serialize)]
struct ApprovalRequest {
    approved: Vec<usize>,
    denied: Vec<usize>,
}

/// An authenticated session with the master.
pub struct MasterClient {
    base: String,
    http: reqwest::Client,
}

impl MasterClient {
    /// Where the master is expected to live.
    pub fn default_url() -> String {
        std::env::var("HIVE_MASTER_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8090".to_string())
            .trim_end_matches('/')
            .to_string()
    }

    /// Connect and authenticate, or explain why not.
    ///
    /// Returns `Ok(None)` when no master is reachable — that is an expected
    /// state (nothing running yet), not an error, and the caller falls back to
    /// running in-process.
    pub async fn connect(base: &str) -> anyhow::Result<Option<Self>> {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(3))
            .build()?;

        // Health first: a short probe distinguishes "no master" from "master
        // present but rejecting us", which are very different problems.
        if http
            .get(format!("{base}/api/health"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .is_err()
        {
            return Ok(None);
        }

        let password = std::env::var("HIVE_WEB_PASSWORD").map_err(|_| {
            anyhow::anyhow!(
                "a master is running at {base} but HIVE_WEB_PASSWORD is not set, \
                 so the CLI cannot authenticate to it"
            )
        })?;

        let response = http
            .post(format!("{base}/login"))
            .form(&[("password", password)])
            .send()
            .await?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::SEE_OTHER {
            anyhow::bail!("master rejected the password from HIVE_WEB_PASSWORD");
        }

        Ok(Some(Self {
            base: base.to_string(),
            http,
        }))
    }

    /// Submit a request: plan, and run everything not gated.
    pub async fn submit(&self, message: &str, project_id: Option<&str>) -> anyhow::Result<ChatReply> {
        let response = self
            .http
            .post(format!("{}/api/chat", self.base))
            .json(&ChatRequest {
                message,
                project_id,
            })
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!("master returned {}: {}", response.status(), response.text().await?);
        }
        Ok(response.json().await?)
    }

    /// Resume a run with the user's decisions on its gated steps.
    pub async fn approve(
        &self,
        run_id: &str,
        approved: Vec<usize>,
        denied: Vec<usize>,
    ) -> anyhow::Result<ChatReply> {
        let response = self
            .http
            .post(format!("{}/api/chat/{run_id}/approve", self.base))
            .json(&ApprovalRequest { approved, denied })
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!("master returned {}: {}", response.status(), response.text().await?);
        }
        Ok(response.json().await?)
    }
}
