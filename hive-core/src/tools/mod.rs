//! Tool system — defines tools the agent can call (shell, file ops, git).

pub mod file_ops;
pub mod git;
pub mod shell;

pub use file_ops::FileOpsTool;
pub use git::GitTool;
pub use shell::ShellTool;

use async_trait::async_trait;
use serde_json::Value;

/// A tool the agent can invoke to take action in the world.
///
/// Implementors describe themselves (`name`, `description`, `input_schema`)
/// so they can be exposed to an LLM as a callable function, and execute
/// against a JSON payload of arguments matching that schema.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name, used for registry lookup and LLM tool-call routing.
    fn name(&self) -> &str;
    /// Human/LLM-facing description of what the tool does.
    fn description(&self) -> &str;
    /// JSON schema describing the tool's input arguments.
    fn input_schema(&self) -> schemars::schema::RootSchema;
    /// Execute the tool with the given JSON arguments.
    async fn execute(&self, args: Value) -> anyhow::Result<String>;
}

/// Registry of available tools, looked up by name.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create the default tool registry (shell, file ops, git).
    pub fn new() -> Self {
        Self {
            tools: vec![
                Box::new(ShellTool::new()),
                Box::new(FileOpsTool::new()),
                Box::new(GitTool::new()),
            ],
        }
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    /// Iterate over all registered tools (e.g. to build LLM tool definitions).
    pub fn list(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|t| t.as_ref())
    }

    /// Run a shell command directly — convenience wrapper for the agent loop.
    pub async fn run_shell(&self, command: &str) -> anyhow::Result<String> {
        let args = serde_json::json!({ "command": command });
        self.get("shell")
            .ok_or_else(|| anyhow::anyhow!("shell tool not registered"))?
            .execute(args)
            .await
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
