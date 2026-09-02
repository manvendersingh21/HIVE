//! Tool system — defines tools the agent can call (shell, file ops, git).

/// Registry of available tools.
pub struct ToolRegistry {
    // TODO: Vec<Box<dyn Tool>>
}

impl ToolRegistry {
    /// Create an empty tool registry.
    pub fn new() -> Self {
        Self {}
    }
}
