//! LLM Router — multi-provider client with complexity-based routing.

use hive_common::Complexity;

/// Multi-provider LLM router.
///
/// Routes requests to the appropriate AI provider based on task complexity:
/// - Simple → Local (Ollama / Qwen2.5-14B)
/// - Medium → Gemini Flash
/// - Complex → Claude
/// - Code-heavy → Codex
pub struct LlmRouter {
    /// Ollama base URL.
    pub local_url: String,
    /// Local model name.
    pub local_model: String,
}

impl LlmRouter {
    /// Create a new LLM router with the given local Ollama configuration.
    pub fn new(local_url: String, local_model: String) -> Self {
        Self {
            local_url,
            local_model,
        }
    }

    /// Classify the complexity of a task using the local LLM.
    pub async fn classify_complexity(&self, task_description: &str) -> anyhow::Result<Complexity> {
        // TODO: Send classification prompt to local Ollama
        tracing::info!("Classifying complexity for: {}", task_description);
        Ok(Complexity::Medium) // Placeholder
    }

    /// Send a raw completion request to the local LLM.
    pub async fn local_complete(&self, prompt: &str) -> anyhow::Result<String> {
        // TODO: Call Ollama /api/chat endpoint
        tracing::info!("Local LLM completion request ({}B prompt)", prompt.len());
        Ok(String::new()) // Placeholder
    }
}
