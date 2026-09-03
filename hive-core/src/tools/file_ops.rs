//! File ops tool — read, write, and append to files on the local filesystem.

use async_trait::async_trait;
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::Tool;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum FileOpsArgs {
    Read { path: String },
    Write { path: String, content: String },
    Append { path: String, content: String },
}

/// Read, write, or append to a file on the local filesystem.
pub struct FileOpsTool;

impl FileOpsTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileOpsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FileOpsTool {
    fn name(&self) -> &str {
        "file_ops"
    }

    fn description(&self) -> &str {
        "Read, write, or append to a file on the local filesystem."
    }

    fn input_schema(&self) -> schemars::schema::RootSchema {
        schema_for!(FileOpsArgs)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<String> {
        let args: FileOpsArgs = serde_json::from_value(args)?;
        match args {
            FileOpsArgs::Read { path } => tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to read '{path}': {e}")),
            FileOpsArgs::Write { path, content } => {
                tokio::fs::write(&path, &content)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to write '{path}': {e}"))?;
                Ok(format!("wrote {} bytes to '{path}'", content.len()))
            }
            FileOpsArgs::Append { path, content } => {
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to open '{path}': {e}"))?;
                file.write_all(content.as_bytes())
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to append to '{path}': {e}"))?;
                Ok(format!("appended {} bytes to '{path}'", content.len()))
            }
        }
    }
}
