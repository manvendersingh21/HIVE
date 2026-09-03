//! Protocol types for communication between master and worker nodes.
//!
//! These types define the JSON-RPC-style protocol used for task delegation,
//! status reporting, and inter-node coordination across the Hive cluster.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Task Assignment (Master → Worker)
// ---------------------------------------------------------------------------

/// A task assignment sent from the master agent to a worker node.
///
/// Contains all information the worker needs to execute the task:
/// the commands to run, the tmux session to create, and optional
/// AI context for sub-reasoning.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskAssignment {
    /// Unique identifier for this task.
    pub task_id: String,
    /// Human-readable description of what this task does.
    pub description: String,
    /// The project this task belongs to (for memory scoping).
    pub project_id: Option<String>,
    /// Ordered list of commands to execute.
    pub commands: Vec<TaskCommand>,
    /// Name of the tmux session to create on the worker.
    pub tmux_session_name: String,
    /// Task priority for scheduling.
    pub priority: TaskPriority,
    /// Optional AI context for sub-task reasoning on the worker.
    pub ai_context: Option<AiContext>,
    /// Expected behavior description (used by the watchdog for safety analysis).
    pub expected_behavior: Option<String>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
}

impl TaskAssignment {
    /// Create a new task assignment with a generated ID and current timestamp.
    pub fn new(
        description: impl Into<String>,
        commands: Vec<TaskCommand>,
        tmux_session_name: impl Into<String>,
    ) -> Self {
        Self {
            task_id: Uuid::new_v4().to_string(),
            description: description.into(),
            project_id: None,
            commands,
            tmux_session_name: tmux_session_name.into(),
            priority: TaskPriority::Normal,
            ai_context: None,
            expected_behavior: None,
            created_at: Utc::now(),
        }
    }

    /// Set the project ID for memory scoping.
    pub fn with_project(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    /// Set the priority.
    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Set the AI context for sub-reasoning.
    pub fn with_ai_context(mut self, ai_context: AiContext) -> Self {
        self.ai_context = Some(ai_context);
        self
    }

    /// Set the expected behavior (for watchdog safety analysis).
    pub fn with_expected_behavior(mut self, behavior: impl Into<String>) -> Self {
        self.expected_behavior = Some(behavior.into());
        self
    }
}

/// An individual command to execute on a worker machine.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskCommand {
    /// The shell command string to execute.
    pub command: String,
    /// Optional working directory (defaults to home dir on worker).
    pub working_dir: Option<String>,
    /// Environment variables to set for this command.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Timeout in seconds (None = no timeout).
    pub timeout_secs: Option<u64>,
    /// Whether to wait for this command to complete before running the next.
    #[serde(default = "default_true")]
    pub wait_for_completion: bool,
}

fn default_true() -> bool {
    true
}

impl TaskCommand {
    /// Create a simple command with no options.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            working_dir: None,
            env_vars: HashMap::new(),
            timeout_secs: None,
            wait_for_completion: true,
        }
    }

    /// Set the working directory.
    pub fn with_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Set a timeout.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Add an environment variable.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_vars.insert(key.into(), value.into());
        self
    }
}

// ---------------------------------------------------------------------------
// AI Provider & Context
// ---------------------------------------------------------------------------

/// Which AI provider to use for reasoning about a task.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    /// Local LLM via Ollama (e.g., Qwen2.5-14B).
    Local,
    /// Google Gemini Flash — medium complexity tasks.
    GeminiFlash,
    /// Anthropic Claude — complex reasoning tasks.
    Claude,
    /// OpenAI Codex / GPT — code-heavy tasks.
    Codex,
}

impl std::fmt::Display for AiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiProvider::Local => write!(f, "local (Ollama)"),
            AiProvider::GeminiFlash => write!(f, "Gemini Flash"),
            AiProvider::Claude => write!(f, "Claude"),
            AiProvider::Codex => write!(f, "Codex"),
        }
    }
}

/// AI context attached to a task for sub-reasoning by the assigned provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AiContext {
    /// Which AI provider should handle reasoning for this task.
    pub provider: AiProvider,
    /// System prompt to use.
    pub system_prompt: String,
    /// Maximum tokens for the AI response.
    pub max_tokens: u32,
}

// ---------------------------------------------------------------------------
// Task Priority
// ---------------------------------------------------------------------------

/// Priority levels for task scheduling and execution ordering.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl Default for TaskPriority {
    fn default() -> Self {
        TaskPriority::Normal
    }
}

impl std::fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskPriority::Low => write!(f, "low"),
            TaskPriority::Normal => write!(f, "normal"),
            TaskPriority::High => write!(f, "high"),
            TaskPriority::Critical => write!(f, "critical"),
        }
    }
}

// ---------------------------------------------------------------------------
// Task Status (Worker → Master)
// ---------------------------------------------------------------------------

/// Status report sent from a worker back to the master agent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskStatus {
    /// The task ID this status refers to.
    pub task_id: String,
    /// Current state of the task.
    pub state: TaskState,
    /// Captured stdout/output (last N lines).
    pub output: Option<String>,
    /// Error message if the task failed.
    pub error: Option<String>,
    /// Name of the tmux session (for remote access).
    pub tmux_session: Option<String>,
    /// Name of the worker node running this task.
    pub worker_name: Option<String>,
    /// When this status was generated.
    pub timestamp: DateTime<Utc>,
    /// Exit code if the task completed.
    pub exit_code: Option<i32>,
}

impl TaskStatus {
    /// Create a new status report.
    pub fn new(task_id: impl Into<String>, state: TaskState) -> Self {
        Self {
            task_id: task_id.into(),
            state,
            output: None,
            error: None,
            tmux_session: None,
            worker_name: None,
            timestamp: Utc::now(),
            exit_code: None,
        }
    }

    /// Create a "running" status.
    pub fn running(task_id: impl Into<String>, tmux_session: impl Into<String>) -> Self {
        Self {
            tmux_session: Some(tmux_session.into()),
            ..Self::new(task_id, TaskState::Running)
        }
    }

    /// Create a "completed" status.
    pub fn completed(task_id: impl Into<String>, exit_code: i32) -> Self {
        Self {
            exit_code: Some(exit_code),
            ..Self::new(task_id, TaskState::Completed)
        }
    }

    /// Create a "failed" status.
    pub fn failed(task_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::new(task_id, TaskState::Failed)
        }
    }
}

/// The lifecycle state of a task on a worker node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Task is queued but not yet started.
    Queued,
    /// Task is actively executing.
    Running,
    /// Task is paused — waiting for the master agent to make a decision.
    WaitingForDecision,
    /// Task was paused by the watchdog — waiting for human review.
    PausedByWatchdog,
    /// Task completed successfully.
    Completed,
    /// Task failed with an error.
    Failed,
    /// Task was cancelled/aborted.
    Cancelled,
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskState::Queued => write!(f, "queued"),
            TaskState::Running => write!(f, "running"),
            TaskState::WaitingForDecision => write!(f, "waiting"),
            TaskState::PausedByWatchdog => write!(f, "paused (watchdog)"),
            TaskState::Completed => write!(f, "completed"),
            TaskState::Failed => write!(f, "failed"),
            TaskState::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ---------------------------------------------------------------------------
// Complexity Classification
// ---------------------------------------------------------------------------

/// Complexity classification for the LLM router.
/// The local model classifies each task and routes to the appropriate provider.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    /// Single command, file operation, status check → local LLM.
    Simple,
    /// Multi-step but straightforward → Gemini Flash.
    Medium,
    /// Deep reasoning, multi-file refactoring, debugging → Claude.
    Complex,
    /// Writing or modifying significant code → Codex.
    CodeHeavy,
}

impl Complexity {
    /// Parse a complexity string (from LLM output) into an enum variant.
    pub fn from_llm_output(s: &str) -> Self {
        let s = s.trim().to_uppercase();
        match s.as_str() {
            "SIMPLE" => Complexity::Simple,
            "MEDIUM" => Complexity::Medium,
            "COMPLEX" => Complexity::Complex,
            "CODE_HEAVY" | "CODEHEAVY" | "CODE-HEAVY" => Complexity::CodeHeavy,
            _ => {
                // Default to Medium if the LLM gives an unexpected response
                tracing::warn!(
                    "Unknown complexity classification: '{}', defaulting to Medium",
                    s
                );
                Complexity::Medium
            }
        }
    }

    /// Map complexity to the recommended AI provider.
    pub fn recommended_provider(&self) -> AiProvider {
        match self {
            Complexity::Simple => AiProvider::Local,
            Complexity::Medium => AiProvider::GeminiFlash,
            Complexity::Complex => AiProvider::Claude,
            Complexity::CodeHeavy => AiProvider::Codex,
        }
    }
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Complexity::Simple => write!(f, "SIMPLE"),
            Complexity::Medium => write!(f, "MEDIUM"),
            Complexity::Complex => write!(f, "COMPLEX"),
            Complexity::CodeHeavy => write!(f, "CODE_HEAVY"),
        }
    }
}

// ---------------------------------------------------------------------------
// Worker Node Info
// ---------------------------------------------------------------------------

/// Information about a worker node in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkerInfo {
    /// Human-readable name for this worker.
    pub name: String,
    /// Hostname or IP address.
    pub host: String,
    /// SSH username.
    pub user: String,
    /// Optional SSH port (defaults to 22).
    pub port: Option<u16>,
    /// Tags for categorization (e.g., ["gpu", "beefy"]).
    #[serde(default)]
    pub tags: Vec<String>,
}

impl WorkerInfo {
    /// Get the SSH connection string (user@host).
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// Current status of a worker node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    /// Worker is online and accepting tasks.
    Online,
    /// Worker is online but fully loaded.
    Busy,
    /// Worker is unreachable.
    Offline,
    /// Worker health check failed.
    Unhealthy,
}

impl std::fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerStatus::Online => write!(f, "online"),
            WorkerStatus::Busy => write!(f, "busy"),
            WorkerStatus::Offline => write!(f, "offline"),
            WorkerStatus::Unhealthy => write!(f, "unhealthy"),
        }
    }
}

// ---------------------------------------------------------------------------
// Safety / Watchdog Types
// ---------------------------------------------------------------------------

/// Severity levels for safety incidents.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Unusual but probably fine (e.g., unexpected warning).
    Low,
    /// Potentially problematic (e.g., permission errors, retrying too many times).
    Medium,
    /// Dangerous action detected (e.g., rm -rf, DROP TABLE).
    High,
    /// Immediate threat (e.g., accessing credentials, network exfiltration).
    Critical,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Low => write!(f, "LOW"),
            Severity::Medium => write!(f, "MEDIUM"),
            Severity::High => write!(f, "HIGH"),
            Severity::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Categories of safety violations detected by the watchdog.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SafetyCategory {
    /// rm -rf, DROP TABLE, format disk.
    DestructiveCommand,
    /// Printing API keys, passwords, tokens.
    CredentialExposure,
    /// curl to unknown external hosts.
    UnexpectedNetworkCall,
    /// Process stuck, repeating same output.
    InfiniteLoop,
    /// sudo, chmod 777, editing /etc/.
    PrivilegeEscalation,
    /// Filling disk, memory leak, fork bomb.
    ResourceExhaustion,
    /// Doing something completely unrelated to the task.
    DeviationFromPlan,
    /// Repeated failures, stack traces.
    UnexpectedError,
}

impl std::fmt::Display for SafetyCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafetyCategory::DestructiveCommand => write!(f, "Destructive Command"),
            SafetyCategory::CredentialExposure => write!(f, "Credential Exposure"),
            SafetyCategory::UnexpectedNetworkCall => write!(f, "Unexpected Network Call"),
            SafetyCategory::InfiniteLoop => write!(f, "Infinite Loop"),
            SafetyCategory::PrivilegeEscalation => write!(f, "Privilege Escalation"),
            SafetyCategory::ResourceExhaustion => write!(f, "Resource Exhaustion"),
            SafetyCategory::DeviationFromPlan => write!(f, "Deviation from Plan"),
            SafetyCategory::UnexpectedError => write!(f, "Unexpected Error"),
        }
    }
}

/// Result of a safety analysis by the watchdog.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SafetyAnalysis {
    /// Whether the output is considered safe.
    pub is_safe: bool,
    /// Severity if unsafe.
    pub severity: Severity,
    /// Category of the violation (if unsafe).
    pub category: Option<SafetyCategory>,
    /// Human-readable reason for the assessment.
    pub reason: String,
    /// Suggested action for the human reviewer.
    pub suggested_action: String,
}

/// A safety incident logged by the watchdog.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Incident {
    /// Unique incident ID.
    pub id: String,
    /// The task that triggered this incident.
    pub task_id: String,
    /// Worker where the incident occurred.
    pub worker: String,
    /// tmux session name.
    pub tmux_session: String,
    /// The safety analysis that triggered the incident.
    pub analysis: SafetyAnalysis,
    /// The terminal output that was flagged.
    pub flagged_output: String,
    /// Current state of the incident review.
    pub review_state: IncidentReviewState,
    /// When the incident was created.
    pub created_at: DateTime<Utc>,
    /// When the incident was resolved (if applicable).
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Human decision on an incident.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanDecision {
    /// User says it's fine, continue the task.
    Resume,
    /// Kill the task permanently.
    Abort,
    /// Resume with an additional note added to context.
    ResumeWithNote(String),
    /// User provides a corrected command to run instead.
    ModifyAndResume(String),
}

/// State of an incident review.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IncidentReviewState {
    /// Waiting for human review.
    PendingReview,
    /// Human reviewed and resumed.
    Resumed,
    /// Human reviewed and aborted.
    Aborted,
}

// ---------------------------------------------------------------------------
// Agent Response
// ---------------------------------------------------------------------------

/// Response from the master agent after processing a user request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    /// Summary of what was done / planned.
    pub summary: String,
    /// Active tmux sessions the user can access.
    pub sessions: Vec<SessionInfo>,
    /// Which AI provider handled the reasoning.
    pub provider_used: AiProvider,
    /// Complexity classification.
    pub complexity: Complexity,
}

/// Information about an active tmux session on a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// tmux session name.
    pub session_name: String,
    /// Worker node name.
    pub worker_name: String,
    /// Task ID.
    pub task_id: String,
    /// Current task state.
    pub state: TaskState,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_assignment_builder() {
        let cmd = TaskCommand::new("echo hello")
            .with_dir("/tmp")
            .with_timeout(30)
            .with_env("FOO", "bar");

        let task = TaskAssignment::new("Test task", vec![cmd], "hive-test-001")
            .with_project("my-project")
            .with_priority(TaskPriority::High)
            .with_expected_behavior("Should print hello");

        assert_eq!(task.project_id, Some("my-project".to_string()));
        assert_eq!(task.priority, TaskPriority::High);
        assert_eq!(task.commands.len(), 1);
        assert_eq!(task.commands[0].working_dir, Some("/tmp".to_string()));
        assert_eq!(task.commands[0].timeout_secs, Some(30));
        assert_eq!(
            task.commands[0].env_vars.get("FOO"),
            Some(&"bar".to_string())
        );
    }

    #[test]
    fn test_task_status_constructors() {
        let running = TaskStatus::running("task-1", "hive-task-1");
        assert_eq!(running.state, TaskState::Running);
        assert_eq!(running.tmux_session, Some("hive-task-1".to_string()));

        let completed = TaskStatus::completed("task-2", 0);
        assert_eq!(completed.state, TaskState::Completed);
        assert_eq!(completed.exit_code, Some(0));

        let failed = TaskStatus::failed("task-3", "command not found");
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.error, Some("command not found".to_string()));
    }

    #[test]
    fn test_complexity_parsing() {
        assert_eq!(Complexity::from_llm_output("SIMPLE"), Complexity::Simple);
        assert_eq!(
            Complexity::from_llm_output("  medium  "),
            Complexity::Medium
        );
        assert_eq!(Complexity::from_llm_output("COMPLEX"), Complexity::Complex);
        assert_eq!(
            Complexity::from_llm_output("CODE_HEAVY"),
            Complexity::CodeHeavy
        );
        assert_eq!(
            Complexity::from_llm_output("CODEHEAVY"),
            Complexity::CodeHeavy
        );
        assert_eq!(
            Complexity::from_llm_output("CODE-HEAVY"),
            Complexity::CodeHeavy
        );
        // Unknown defaults to Medium
        assert_eq!(Complexity::from_llm_output("banana"), Complexity::Medium);
    }

    #[test]
    fn test_complexity_provider_mapping() {
        assert_eq!(Complexity::Simple.recommended_provider(), AiProvider::Local);
        assert_eq!(
            Complexity::Medium.recommended_provider(),
            AiProvider::GeminiFlash
        );
        assert_eq!(
            Complexity::Complex.recommended_provider(),
            AiProvider::Claude
        );
        assert_eq!(
            Complexity::CodeHeavy.recommended_provider(),
            AiProvider::Codex
        );
    }

    #[test]
    fn test_worker_info_ssh_target() {
        let worker = WorkerInfo {
            name: "worker-1".to_string(),
            host: "192.168.1.101".to_string(),
            user: "admin".to_string(),
            port: None,
            tags: vec!["gpu".to_string()],
        };
        assert_eq!(worker.ssh_target(), "admin@192.168.1.101");
    }

    #[test]
    fn test_task_assignment_serialization() {
        let task = TaskAssignment::new(
            "Deploy app",
            vec![TaskCommand::new("git pull")],
            "hive-deploy-001",
        );

        let json = serde_json::to_string_pretty(&task).unwrap();
        let deserialized: TaskAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.description, "Deploy app");
        assert_eq!(deserialized.tmux_session_name, "hive-deploy-001");
    }

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn test_priority_ordering() {
        assert!(TaskPriority::Low < TaskPriority::Normal);
        assert!(TaskPriority::Normal < TaskPriority::High);
        assert!(TaskPriority::High < TaskPriority::Critical);
    }
}
