//! Error types for the Hive system.

use thiserror::Error;

/// Unified error type for the Hive system.
#[derive(Error, Debug)]
pub enum HiveError {
    // -- Configuration errors --
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Missing configuration: {field}")]
    MissingConfig { field: String },

    #[error("Failed to parse config file '{path}': {source}")]
    ConfigParse {
        path: String,
        source: toml::de::Error,
    },

    // -- LLM errors --
    #[error("LLM request failed ({provider}): {message}")]
    LlmRequest { provider: String, message: String },

    #[error("LLM response parsing failed: {0}")]
    LlmParse(String),

    #[error("LLM provider '{0}' is not configured")]
    LlmProviderNotConfigured(String),

    // -- Worker / SSH errors --
    #[error("Worker '{worker}' is not reachable: {message}")]
    WorkerUnreachable { worker: String, message: String },

    #[error("SSH connection to '{host}' failed: {message}")]
    SshConnection { host: String, message: String },

    #[error("No workers available for task assignment")]
    NoWorkersAvailable,

    #[error("Worker '{worker}' rejected task: {reason}")]
    TaskRejected { worker: String, reason: String },

    // -- Task errors --
    #[error("Task '{task_id}' not found")]
    TaskNotFound { task_id: String },

    #[error("Task '{task_id}' is in state '{state}' and cannot be {action}")]
    InvalidTaskState {
        task_id: String,
        state: String,
        action: String,
    },

    #[error("Task execution failed: {0}")]
    TaskExecution(String),

    #[error("Task timed out after {timeout_secs}s")]
    TaskTimeout { timeout_secs: u64 },

    // -- tmux errors --
    #[error("tmux session '{session}' not found on worker '{worker}'")]
    TmuxSessionNotFound { session: String, worker: String },

    #[error("tmux operation failed: {0}")]
    TmuxError(String),

    // -- Database errors --
    #[error("Database error: {0}")]
    Database(String),

    // -- Project / Memory errors --
    #[error("Project '{0}' not found")]
    ProjectNotFound(String),

    #[error("Project '{0}' already exists")]
    ProjectAlreadyExists(String),

    #[error("Knowledge extraction failed: {0}")]
    KnowledgeExtraction(String),

    #[error("Embedding generation failed: {0}")]
    EmbeddingGeneration(String),

    // -- Skill errors --
    #[error("Skill '{0}' not found")]
    SkillNotFound(String),

    #[error("Skill loading failed for '{path}': {message}")]
    SkillLoad { path: String, message: String },

    // -- Safety / Watchdog errors --
    #[error("Safety violation detected: {reason} (severity: {severity})")]
    SafetyViolation { reason: String, severity: String },

    #[error("Incident '{0}' not found")]
    IncidentNotFound(String),

    // -- Serialization --
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    // -- IO --
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- HTTP --
    #[error("HTTP request error: {0}")]
    Http(String),

    // -- Generic --
    #[error("{0}")]
    Other(String),
}

/// Convenience type alias for Result with HiveError.
pub type HiveResult<T> = Result<T, HiveError>;

impl From<rusqlite::Error> for HiveError {
    fn from(err: rusqlite::Error) -> Self {
        HiveError::Database(err.to_string())
    }
}

impl From<reqwest::Error> for HiveError {
    fn from(err: reqwest::Error) -> Self {
        HiveError::Http(err.to_string())
    }
}

impl From<toml::de::Error> for HiveError {
    fn from(err: toml::de::Error) -> Self {
        HiveError::Config(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = HiveError::WorkerUnreachable {
            worker: "worker-1".to_string(),
            message: "connection refused".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Worker 'worker-1' is not reachable: connection refused"
        );
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let hive_err: HiveError = io_err.into();
        assert!(matches!(hive_err, HiveError::Io(_)));
    }

    #[test]
    fn test_error_from_json() {
        let json_str = "not valid json{{{";
        let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
        let hive_err: HiveError = json_err.into();
        assert!(matches!(hive_err, HiveError::Serialization(_)));
    }
}
