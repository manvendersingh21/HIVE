//! Git tool — runs a git subcommand in a repository and returns its output.

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use super::Tool;

#[derive(Debug, Deserialize, JsonSchema)]
struct GitArgs {
    /// Git subcommand and args, e.g. "status --short" or "diff HEAD~1".
    args: String,
    /// Optional repo directory (defaults to the current directory).
    repo_dir: Option<String>,
}

/// Run a git subcommand (status, diff, log, ...) in a repository.
pub struct GitTool;

impl GitTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Run a git subcommand (e.g. status, diff, log) in a repository and return its output."
    }

    fn input_schema(&self) -> schemars::schema::RootSchema {
        schema_for!(GitArgs)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<String> {
        let args: GitArgs = serde_json::from_value(args)?;

        let mut cmd = Command::new("git");
        cmd.args(args.args.split_whitespace());
        if let Some(dir) = &args.repo_dir {
            cmd.current_dir(dir);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(format!("{stdout}{stderr}"))
    }
}
