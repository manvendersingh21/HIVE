//! LLM Router — multi-provider client with complexity-based routing.

pub mod claude;
pub mod gemini;
pub mod local;
pub mod nvidia;
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
    pub model: String,
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
/// In legacy mode, missing cloud providers fall back to the local model.
/// Single-provider mode overrides every recommendation and skill override,
/// and propagates errors without switching providers.
pub struct LlmRouter {
    single_provider: Option<AiProvider>,
    nvidia: nvidia::NvidiaClient,
    models: std::collections::HashMap<AiProvider, String>,
    local: OllamaClient,
    gemini: Option<GeminiClient>,
    claude: Option<ClaudeClient>,
    codex: Option<OpenAiClient>,
}

impl LlmRouter {
    /// Create a router with only the local Ollama client configured.
    pub fn new(local_url: String, local_model: String) -> Self {
        Self {
            models: [(AiProvider::Local, local_model.clone())].into(),
            single_provider: None,
            nvidia: nvidia::NvidiaClient::for_reasoning(&Default::default()),
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

        let mut models = std::collections::HashMap::new();
        models.insert(AiProvider::Local, cfg.local.model.clone());
        models.insert(AiProvider::Nvidia, cfg.nvidia.model.clone());
        for (provider, config) in [
            (AiProvider::GeminiFlash, &cfg.gemini),
            (AiProvider::Claude, &cfg.claude),
            (AiProvider::Codex, &cfg.codex),
        ] {
            if let Some(config) = config {
                models.insert(provider, config.model.clone());
            }
        }
        Self {
            single_provider: cfg.single_provider,
            nvidia: nvidia::NvidiaClient::for_reasoning(&cfg.nvidia),
            models,
            local,
            gemini,
            claude,
            codex,
        }
    }

    /// Classify using the single provider, or the local model in legacy mode.
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

        let raw = self.local_complete(&prompt).await?;
        let complexity = Complexity::from_llm_output(&raw);
        tracing::info!("Classified '{task_description}' as {complexity}");
        Ok(complexity)
    }

    /// Auxiliary completion: NVIDIA master model, or local in legacy mode.
    pub async fn local_complete(&self, prompt: &str) -> anyhow::Result<String> {
        if let Some(provider) = self.single_provider {
            if provider == AiProvider::Nvidia {
                return Ok(self.nvidia.complete(prompt, "low").await?.text);
            }
            return Ok(self.complete_with(prompt, provider).await?.text);
        }
        self.local.complete_raw(prompt).await
    }

    pub fn effective_provider(&self, provider: AiProvider) -> AiProvider {
        self.single_provider.unwrap_or(provider)
    }

    pub fn uses_local_startup(&self) -> bool {
        self.single_provider.is_none() || self.single_provider == Some(AiProvider::Local)
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
        self.complete_with(prompt, complexity.recommended_provider())
            .await
    }

    /// Run a prompt against one explicit provider.
    ///
    /// Skill overrides take precedence over complexity in legacy mode.
    /// Single-provider mode takes precedence over both and disables fallback.
    pub async fn complete_with(
        &self,
        prompt: &str,
        provider: AiProvider,
    ) -> anyhow::Result<LlmResponse> {
        let provider = self.effective_provider(provider);
        if provider == AiProvider::Nvidia {
            return self.nvidia.complete(prompt, "high").await;
        }
        let result = match provider {
            AiProvider::Nvidia => unreachable!(),
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
            Ok(text) => Ok(LlmResponse {
                text,
                provider,
                model: self.models.get(&provider).cloned().unwrap_or_default(),
            }),
            Err(e) if provider != AiProvider::Local && self.single_provider.is_none() => {
                tracing::warn!(
                    "Provider {provider} unavailable ({e}), falling back to local model"
                );
                let text = self.local.complete_raw(prompt).await?;
                Ok(LlmResponse {
                    text,
                    provider: AiProvider::Local,
                    model: self
                        .models
                        .get(&AiProvider::Local)
                        .cloned()
                        .unwrap_or_default(),
                })
            }
            Err(e) => Err(e),
        }
    }
}
