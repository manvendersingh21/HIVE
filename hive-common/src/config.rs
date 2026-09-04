//! Configuration types for the Hive system.
//!
//! The master configuration is loaded from `config/hive.toml` and worker
//! definitions from `config/workers.toml`. API keys are loaded from
//! environment variables for security.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{HiveError, HiveResult};
use crate::protocol::WorkerInfo;

// ---------------------------------------------------------------------------
// Top-level Config
// ---------------------------------------------------------------------------

/// Root configuration for the Hive system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiveConfig {
    /// Master agent settings.
    pub master: MasterConfig,
    /// LLM provider configurations.
    pub llm: LlmConfig,
    /// Web terminal server settings.
    pub web: WebConfig,
    /// Database settings.
    pub database: DatabaseConfig,
    /// Skill system settings.
    pub skills: SkillsConfig,
    /// Fine-tuning settings.
    pub finetune: FinetuneConfig,
    /// Memory / knowledge system settings.
    pub memory: MemoryConfig,
    /// Safety watchdog settings.
    #[serde(default)]
    pub watchdog: WatchdogConfig,
}

impl HiveConfig {
    /// Load configuration from a TOML file.
    pub fn from_file(path: &Path) -> HiveResult<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            HiveError::Config(format!(
                "Failed to read config file '{}': {}",
                path.display(),
                e
            ))
        })?;

        toml::from_str(&content).map_err(|e| HiveError::ConfigParse {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Load configuration from the default location (`config/hive.toml`
    /// relative to the given project root).
    pub fn from_project_root(root: &Path) -> HiveResult<Self> {
        let config_path = root.join("config").join("hive.toml");
        Self::from_file(&config_path)
    }
}

// ---------------------------------------------------------------------------
// Sub-configs
// ---------------------------------------------------------------------------

/// Master agent settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterConfig {
    /// Address to listen on for inter-node communication.
    #[serde(default = "default_master_listen_addr")]
    pub listen_addr: String,
}

fn default_master_listen_addr() -> String {
    "0.0.0.0:9090".to_string()
}

/// LLM provider configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Local LLM (Ollama) configuration.
    pub local: LocalLlmConfig,
    /// Google Gemini configuration.
    #[serde(default)]
    pub gemini: Option<CloudLlmConfig>,
    /// Anthropic Claude configuration.
    #[serde(default)]
    pub claude: Option<CloudLlmConfig>,
    /// OpenAI Codex configuration.
    #[serde(default)]
    pub codex: Option<CloudLlmConfig>,
}

/// Configuration for the local LLM (Ollama).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalLlmConfig {
    /// LLM provider name (should be "ollama").
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Model name to use.
    #[serde(default = "default_local_model")]
    pub model: String,
    /// Base URL for the Ollama API.
    #[serde(default = "default_ollama_url")]
    pub base_url: String,
    /// Maximum context window size in tokens.
    #[serde(default = "default_max_context")]
    pub max_context: u32,
}

fn default_provider() -> String {
    "ollama".to_string()
}
fn default_local_model() -> String {
    "qwen2.5:14b-instruct-q4_K_M".to_string()
}
fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_max_context() -> u32 {
    8192
}

/// Configuration for a cloud LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudLlmConfig {
    /// Model name/identifier.
    pub model: String,
    /// API key (if not set, loaded from env var).
    pub api_key: Option<String>,
    /// API key environment variable name (optional override).
    pub api_key_env: Option<String>,
    /// Base URL override (optional, for proxies or custom endpoints).
    pub base_url: Option<String>,
}

impl CloudLlmConfig {
    /// Resolve the API key — from the config, from the specified env var,
    /// or from the default env var for the provider.
    pub fn resolve_api_key(&self, default_env: &str) -> HiveResult<String> {
        // 1. Directly configured
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }

        // 2. Custom env var
        if let Some(env_name) = &self.api_key_env {
            if let Ok(key) = std::env::var(env_name) {
                return Ok(key);
            }
        }

        // 3. Default env var
        std::env::var(default_env).map_err(|_| HiveError::MissingConfig {
            field: format!(
                "API key (set '{}' env var or configure in hive.toml)",
                default_env
            ),
        })
    }
}

/// Web terminal server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    /// Address to listen on.
    #[serde(default = "default_web_listen_addr")]
    pub listen_addr: String,
    /// Basic auth username.
    #[serde(default = "default_web_username")]
    pub auth_username: String,
    /// Basic auth password (if not set, loaded from HIVE_WEB_PASSWORD env var).
    pub auth_password: Option<String>,
}

fn default_web_listen_addr() -> String {
    "0.0.0.0:8080".to_string()
}
fn default_web_username() -> String {
    "hive".to_string()
}

impl WebConfig {
    /// Resolve the web password from config or env var.
    pub fn resolve_password(&self) -> HiveResult<String> {
        if let Some(pw) = &self.auth_password {
            return Ok(pw.clone());
        }
        std::env::var("HIVE_WEB_PASSWORD").map_err(|_| HiveError::MissingConfig {
            field: "Web password (set HIVE_WEB_PASSWORD env var)".to_string(),
        })
    }
}

/// Database settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Path to the SQLite database file.
    #[serde(default = "default_db_path")]
    pub path: String,
}

fn default_db_path() -> String {
    "~/.hive/hive.db".to_string()
}

impl DatabaseConfig {
    /// Resolve the database path, expanding `~` to the home directory.
    pub fn resolved_path(&self) -> PathBuf {
        if self.path.starts_with("~/") {
            if let Some(home) = dirs_compat() {
                return home.join(&self.path[2..]);
            }
        }
        PathBuf::from(&self.path)
    }
}

/// Get the user's home directory (compatible helper).
fn dirs_compat() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Skill system settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Directory containing skill definitions.
    #[serde(default = "default_skills_dir")]
    pub directory: String,
}

fn default_skills_dir() -> String {
    "~/.hive/skills".to_string()
}

/// Fine-tuning settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinetuneConfig {
    /// Whether to automatically collect training data from interactions.
    #[serde(default = "default_true_bool")]
    pub auto_collect: bool,
}

fn default_true_bool() -> bool {
    true
}

/// Memory / knowledge system settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Whether to auto-index conversations on completion.
    #[serde(default = "default_true_bool")]
    pub auto_index: bool,
    /// Embedding model name (run via Ollama).
    #[serde(default = "default_embed_model")]
    pub embedding_model: String,
    /// Tokens per RAG chunk.
    #[serde(default = "default_chunk_size")]
    pub chunk_size: u32,
    /// Overlap between chunks (in tokens).
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap: u32,
    /// Max tokens of retrieved context to inject into prompts.
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u32,
    /// Knowledge graph settings.
    #[serde(default)]
    pub knowledge_graph: KnowledgeGraphConfig,
}

fn default_embed_model() -> String {
    "nomic-embed-text".to_string()
}
fn default_chunk_size() -> u32 {
    512
}
fn default_chunk_overlap() -> u32 {
    64
}
fn default_max_context_tokens() -> u32 {
    2048
}

/// Knowledge graph settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeGraphConfig {
    /// Max entities to extract per conversation.
    #[serde(default = "default_max_entities")]
    pub max_entities_per_conversation: u32,
    /// Cosine similarity threshold for merging duplicate entities.
    #[serde(default = "default_dedup_threshold")]
    pub entity_dedup_threshold: f64,
}

fn default_max_entities() -> u32 {
    20
}
fn default_dedup_threshold() -> f64 {
    0.85
}

impl Default for KnowledgeGraphConfig {
    fn default() -> Self {
        Self {
            max_entities_per_conversation: default_max_entities(),
            entity_dedup_threshold: default_dedup_threshold(),
        }
    }
}

/// Safety watchdog settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Whether the watchdog is enabled.
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
    /// How often to poll each active session (in seconds).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Number of terminal lines to capture per check.
    #[serde(default = "default_capture_lines")]
    pub capture_lines: u32,
    /// After N consecutive safe checks, reduce polling frequency.
    #[serde(default = "default_max_consecutive_safe")]
    pub max_consecutive_safe: u32,
    /// Slower poll interval for stable long-running tasks.
    #[serde(default = "default_reduced_poll")]
    pub reduced_poll_interval_secs: u64,
    /// Whether to use LLM analysis in addition to regex rules.
    #[serde(default = "default_true_bool")]
    pub llm_analysis: bool,
    /// Maximum number of files a single operation may affect before the
    /// interceptor pauses for approval (default: 5).
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// Maximum number of lines that may be deleted before the interceptor
    /// pauses for approval (default: 100).
    #[serde(default = "default_max_lines_deleted")]
    pub max_lines_deleted: usize,
    /// Notification settings.
    #[serde(default)]
    pub notifications: NotificationConfig,
    /// Extra blocked patterns (appended to built-in defaults).
    #[serde(default)]
    pub extra_rules: Vec<ExtraRule>,
}

fn default_poll_interval() -> u64 {
    5
}
fn default_capture_lines() -> u32 {
    50
}
fn default_max_consecutive_safe() -> u32 {
    10
}
fn default_reduced_poll() -> u64 {
    15
}
fn default_max_files() -> usize {
    5
}
fn default_max_lines_deleted() -> usize {
    100
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: default_poll_interval(),
            capture_lines: default_capture_lines(),
            max_consecutive_safe: default_max_consecutive_safe(),
            reduced_poll_interval_secs: default_reduced_poll(),
            llm_analysis: true,
            max_files: default_max_files(),
            max_lines_deleted: default_max_lines_deleted(),
            notifications: NotificationConfig::default(),
            extra_rules: vec![],
        }
    }
}

/// Notification configuration for the watchdog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    /// ntfy.sh topic for push notifications.
    pub ntfy_topic: Option<String>,
    /// Webhook URL (Slack, Discord, etc.).
    pub webhook_url: Option<String>,
    /// Base URL for the web dashboard (for links in notifications).
    #[serde(default = "default_web_base_url")]
    pub web_base_url: String,
}

fn default_web_base_url() -> String {
    "http://localhost:8080".to_string()
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            ntfy_topic: None,
            webhook_url: None,
            web_base_url: default_web_base_url(),
        }
    }
}

/// A user-defined blocked pattern for the watchdog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraRule {
    /// Regex pattern to match against terminal output.
    pub pattern: String,
    /// Severity level if matched.
    pub severity: String,
    /// Safety category.
    pub category: String,
    /// Human-readable description.
    pub description: String,
}

// ---------------------------------------------------------------------------
// Workers Config (separate file)
// ---------------------------------------------------------------------------

/// Workers configuration loaded from `config/workers.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkersConfig {
    /// List of worker machine definitions.
    pub workers: Vec<WorkerInfo>,
}

impl WorkersConfig {
    /// Load workers configuration from a TOML file.
    pub fn from_file(path: &Path) -> HiveResult<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            HiveError::Config(format!(
                "Failed to read workers config '{}': {}",
                path.display(),
                e
            ))
        })?;

        toml::from_str(&content).map_err(|e| HiveError::ConfigParse {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Load from the default location relative to the project root.
    pub fn from_project_root(root: &Path) -> HiveResult<Self> {
        let config_path = root.join("config").join("workers.toml");
        Self::from_file(&config_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_db_path_expansion() {
        let db_config = DatabaseConfig {
            path: "~/.hive/hive.db".to_string(),
        };
        let resolved = db_config.resolved_path();
        // Should NOT start with ~
        assert!(!resolved.to_string_lossy().starts_with('~'));
        assert!(resolved.to_string_lossy().ends_with(".hive/hive.db"));
    }

    #[test]
    fn test_absolute_db_path() {
        let db_config = DatabaseConfig {
            path: "/var/data/hive.db".to_string(),
        };
        assert_eq!(
            db_config.resolved_path(),
            PathBuf::from("/var/data/hive.db")
        );
    }

    #[test]
    fn test_hive_config_from_toml_string() {
        let toml_str = r#"
[master]
listen_addr = "0.0.0.0:9090"

[llm.local]
provider = "ollama"
model = "qwen2.5:14b-instruct-q4_K_M"
base_url = "http://localhost:11434"
max_context = 8192

[web]
listen_addr = "0.0.0.0:8080"
auth_username = "hive"

[database]
path = "~/.hive/hive.db"

[skills]
directory = "~/.hive/skills"

[finetune]
auto_collect = true

[memory]
auto_index = true
embedding_model = "nomic-embed-text"
chunk_size = 512
chunk_overlap = 64
max_context_tokens = 2048

[memory.knowledge_graph]
max_entities_per_conversation = 20
entity_dedup_threshold = 0.85
"#;

        let config: HiveConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.master.listen_addr, "0.0.0.0:9090");
        assert_eq!(config.llm.local.model, "qwen2.5:14b-instruct-q4_K_M");
        assert_eq!(config.llm.local.max_context, 8192);
        assert_eq!(config.web.auth_username, "hive");
        assert_eq!(config.memory.embedding_model, "nomic-embed-text");
        assert_eq!(
            config.memory.knowledge_graph.max_entities_per_conversation,
            20
        );
        assert!(config.watchdog.enabled); // default
    }

    #[test]
    fn test_workers_config_from_toml_string() {
        let toml_str = r#"
[[workers]]
name = "worker-1"
host = "192.168.1.101"
user = "admin"
tags = ["gpu"]

[[workers]]
name = "worker-2"
host = "192.168.1.102"
user = "admin"
tags = []
"#;

        let config: WorkersConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.workers.len(), 2);
        assert_eq!(config.workers[0].name, "worker-1");
        assert_eq!(config.workers[0].host, "192.168.1.101");
        assert_eq!(config.workers[0].tags, vec!["gpu"]);
        assert_eq!(config.workers[1].name, "worker-2");
    }

    #[test]
    fn test_cloud_llm_api_key_from_env() {
        // Set a test env var
        std::env::set_var("TEST_HIVE_API_KEY", "test-key-12345");

        let config = CloudLlmConfig {
            model: "test-model".to_string(),
            api_key: None,
            api_key_env: Some("TEST_HIVE_API_KEY".to_string()),
            base_url: None,
        };

        let key = config.resolve_api_key("FALLBACK_KEY").unwrap();
        assert_eq!(key, "test-key-12345");

        // Clean up
        std::env::remove_var("TEST_HIVE_API_KEY");
    }

    #[test]
    fn test_cloud_llm_direct_api_key() {
        let config = CloudLlmConfig {
            model: "test-model".to_string(),
            api_key: Some("direct-key".to_string()),
            api_key_env: None,
            base_url: None,
        };

        let key = config.resolve_api_key("ANYTHING").unwrap();
        assert_eq!(key, "direct-key");
    }

    #[test]
    fn test_watchdog_config_defaults() {
        let config = WatchdogConfig::default();
        assert!(config.enabled);
        assert_eq!(config.poll_interval_secs, 5);
        assert_eq!(config.capture_lines, 50);
        assert_eq!(config.max_consecutive_safe, 10);
        assert_eq!(config.reduced_poll_interval_secs, 15);
        assert!(config.llm_analysis);
        assert!(config.extra_rules.is_empty());
    }
}
