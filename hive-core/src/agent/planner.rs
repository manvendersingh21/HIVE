//! Task planner — decomposes user requests into subtasks.

/// A planned task with subtasks ready for delegation.
#[derive(Debug, Clone)]
pub struct TaskPlan {
    /// High-level summary of the plan.
    pub summary: String,
    /// Individual subtasks to execute.
    pub subtasks: Vec<SubTask>,
}

/// A single subtask within a plan.
#[derive(Debug, Clone)]
pub struct SubTask {
    /// Description of what this subtask does.
    pub description: String,
    /// Whether this subtask requires remote execution (on a worker).
    pub requires_remote: bool,
    /// Commands to execute.
    pub commands: Vec<String>,
    /// Expected behavior (for watchdog).
    pub expected_behavior: Option<String>,
}
