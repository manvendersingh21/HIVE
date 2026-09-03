//! LLM Router — multi-provider client with complexity-based routing.

pub mod claude;
pub mod gemini;
pub mod local;
pub mod openai;

pub use claude::ClaudeClient;
pub use gemini::GeminiClient;
pub use local::OllamaClient;
pub use openai::OpenAiClient;

use hive_common::config::LlmConfig;
use hive_common::{AiProvider, Complexity};
use serde::{Deserialize, Serialize};

/// A single turn in a chat-style LLM request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// The result of routing a request to a provider.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// The provider's raw text response.
    pub text: String,
    /// Which provider actually produced it (may differ from the recommended
    /// provider if routing fell back to the local model).
    pub provider: AiProvider,
}

/// Multi-provider LLM router.
///
/// Routes requests to the appropriate AI provider based on task complexity:
/// - Simple → Local (Ollama / Qwen2.5-14B)
/// - Medium → Gemini Flash
/// - Complex → Claude
/// - Code-heavy → Codex (OpenAI)
///
/// Cloud providers are optional: if a provider isn't configured (no API key
/// available), routing falls back to the local model rather than failing
/// the whole request.
pub struct LlmRouter {
    local: OllamaClient,
    gemini: Option<GeminiClient>,
    claude: Option<ClaudeClient>,
    codex: Option<OpenAiClient>,
}

impl LlmRouter {
    /// Create a router with only the local Ollama client configured.
    pub fn new(local_url: String, local_model: String) -> Self {
        Self {
            local: OllamaClient::new(local_url, local_model),
            gemini: None,
            claude: None,
            codex: None,
        }
    }

    /// Build a router from the full LLM config, wiring up whichever cloud
    /// providers have a resolvable API key. Providers without one are
    /// logged and left unconfigured rather than failing construction.
    pub fn from_config(cfg: &LlmConfig) -> Self {
        let local = OllamaClient::new(cfg.local.base_url.clone(), cfg.local.model.clone());

        let gemini = cfg
            .gemini
            .as_ref()
            .and_then(|c| match GeminiClient::new(c) {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::warn!("Gemini provider not available: {e}");
                    None
                }
            });

        let claude = cfg
            .claude
            .as_ref()
            .and_then(|c| match ClaudeClient::new(c) {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::warn!("Claude provider not available: {e}");
                    None
                }
            });

        let codex = cfg.codex.as_ref().and_then(|c| match OpenAiClient::new(c) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!("Codex (OpenAI) provider not available: {e}");
                None
            }
        });

        Self {
            local,
            gemini,
            claude,
            codex,
        }
    }

    /// Classify the complexity of a task using the local LLM.
    pub async fn classify_complexity(&self, task_description: &str) -> anyhow::Result<Complexity> {
        let prompt = format!(
            "Classify this task's complexity as SIMPLE, MEDIUM, COMPLEX, or CODE_HEAVY.\n\
             Task: {task_description}\n\
             Rules:\n\
             - SIMPLE: single command, file operation, status check\n\
             - MEDIUM: multi-step but straightforward (install, configure, deploy)\n\
             - COMPLEX: requires deep reasoning, multi-file refactoring, debugging\n\
             - CODE_HEAVY: writing or modifying significant code\n\
             Respond with ONLY the classification word, nothing else."
        );

        let raw = self.local.complete_raw(&prompt).await?;
        let complexity = Complexity::from_llm_output(&raw);
        tracing::info!("Classified '{task_description}' as {complexity}");
        Ok(complexity)
    }

    /// Send a raw completion request to the local LLM.
    pub async fn local_complete(&self, prompt: &str) -> anyhow::Result<String> {
        self.local.complete_raw(prompt).await
    }

    /// Route a prompt to the provider recommended for `complexity`, falling
    /// back to the local model if that provider isn't configured or the
    /// request fails.
    /// Whether the local model is reachable. See [`LocalLlm::is_available`].
    pub async fn local_available(&self) -> bool {
        self.local.is_available().await
    }

    pub async fn route_and_execute(
        &self,
        prompt: &str,
        complexity: Complexity,
    ) -> anyhow::Result<LlmResponse> {
        let provider = complexity.recommended_provider();

        let result = match provider {
            AiProvider::Local => self.local.complete_raw(prompt).await,
            AiProvider::GeminiFlash => match &self.gemini {
                Some(client) => client.complete(prompt).await,
                None => Err(anyhow::anyhow!(
                    "Gemini is not configured (set GEMINI_API_KEY or [llm.gemini] in hive.toml)"
                )),
            },
            AiProvider::Claude => match &self.claude {
                Some(client) => client.complete(prompt).await,
                None => Err(anyhow::anyhow!(
                    "Claude is not configured (set ANTHROPIC_API_KEY or [llm.claude] in hive.toml)"
                )),
            },
            AiProvider::Codex => match &self.codex {
                Some(client) => client.complete(prompt).await,
                None => Err(anyhow::anyhow!(
                    "Codex is not configured (set OPENAI_API_KEY or [llm.codex] in hive.toml)"
                )),
            },
        };

        match result {
            Ok(text) => Ok(LlmResponse { text, provider }),
            Err(e) if provider != AiProvider::Local => {
                tracing::warn!(
                    "Provider {provider} unavailable ({e}), falling back to local model"
                );
                let text = self.local.complete_raw(prompt).await?;
                Ok(LlmResponse {
                    text,
                    provider: AiProvider::Local,
                })
            }
            Err(e) => Err(e),
        }
    }
}
