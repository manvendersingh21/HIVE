//! Shell tool — runs a command via `sh -c` and captures its output.

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use super::Tool;

#[derive(Debug, Deserialize, JsonSchema)]
struct ShellArgs {
    /// The shell command to run.
    command: String,
    /// Optional working directory.
    working_dir: Option<String>,
    /// Optional timeout in seconds (default 60).
    timeout_secs: Option<u64>,
}

/// Runs a shell command and returns its combined stdout/stderr and exit code.
pub struct ShellTool;

impl ShellTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command and return its combined stdout/stderr and exit code."
    }

    fn input_schema(&self) -> schemars::schema::RootSchema {
        schema_for!(ShellArgs)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<String> {
        let args: ShellArgs = serde_json::from_value(args)?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&args.command);
        if let Some(dir) = &args.working_dir {
            cmd.current_dir(dir);
        }

        let secs = args.timeout_secs.unwrap_or(60);
        let output = timeout(Duration::from_secs(secs), cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("command timed out after {secs}s: {}", args.command))??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(format!("exit_code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"))
    }
}
