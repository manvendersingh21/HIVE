//! Shell tool — runs a command via `bash -c` and captures bounded output.

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt};
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

        let mut cmd = Command::new("bash");
        cmd.args(["-o", "pipefail", "-c"]).arg(&args.command);
        if let Some(dir) = &args.working_dir {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.as_std_mut().process_group(0);
        let secs = args.timeout_secs.unwrap_or(60).clamp(1, 120);
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap();
        let mut stdout_task = tokio::spawn(tail(child.stdout.take().unwrap()));
        let mut stderr_task = tokio::spawn(tail(child.stderr.take().unwrap()));
        let exit_code = match timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(status) => status?.code().unwrap_or(-1),
            Err(_) => {
                // Stop the shell's process group so a timeout cannot leave a
                // foreground command running and later repeat its side effects.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
                let _ = child.wait().await;
                124
            }
        };
        let stdout = match timeout(Duration::from_secs(2), &mut stdout_task).await {
            Ok(Ok(Ok(s))) => s,
            _ => {
                stdout_task.abort();
                "[output stream did not close]".into()
            }
        };
        let mut stderr = match timeout(Duration::from_secs(2), &mut stderr_task).await {
            Ok(Ok(Ok(s))) => s,
            _ => {
                stderr_task.abort();
                "[error stream did not close]".into()
            }
        };
        if exit_code == 124 {
            stderr.push_str(&format!("\ncommand timed out after {secs}s"));
        }

        Ok(format!(
            "exit_code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ))
    }
}

/// Drain the stream while retaining only a bounded diagnostic tail.
async fn tail(mut input: impl AsyncRead + Unpin) -> std::io::Result<String> {
    let mut saved = Vec::new();
    let mut buf = [0u8; 4096];
    let mut truncated = false;
    loop {
        let n = input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        saved.extend_from_slice(&buf[..n]);
        if saved.len() > 24576 {
            saved.drain(..saved.len() - 24576);
            truncated = true;
        }
    }
    Ok(format!(
        "{}{}",
        if truncated {
            "[earlier output truncated]\n"
        } else {
            ""
        },
        String::from_utf8_lossy(&saved)
    ))
}
