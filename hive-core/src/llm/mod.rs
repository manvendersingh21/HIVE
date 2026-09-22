//! LLM Router — multi-provider client with complexity-based routing.

pub mod claude;
pub mod gemini;
pub mod local;
pub mod nvidia;
pub mod openai;
pub mod zai;

pub use claude::ClaudeClient;
pub use gemini::GeminiClient;
pub use local::OllamaClient;
pub use openai::OpenAiClient;
pub use zai::ZaiClient;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use hive_common::config::LlmConfig;
use hive_common::{AiProvider, Complexity};
use serde::{Deserialize, Serialize};

/// Env var NVIDIA's client resolves its key from (see `nvidia::NvidiaClient::for_reasoning`).
const NVIDIA_KEY_ENV: &str = "NVIDIA_API_KEY_FLASH";

fn nvidia_key_configured() -> bool {
    std::env::var(NVIDIA_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_some()
}

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
    /// When set, overrides all routing and disables provider fallback.
    /// Runtime-mutable so the master-agent settings API can switch it
    /// without a restart.
    single_provider: RwLock<Option<AiProvider>>,
    nvidia: nvidia::NvidiaClient,
    models: RwLock<HashMap<AiProvider, String>>,
    local: OllamaClient,
    gemini: Option<GeminiClient>,
    claude: Option<ClaudeClient>,
    codex: Option<OpenAiClient>,
    /// Runtime-mutable: `set_provider` can configure or replace this from a
    /// caller-supplied API key without restarting the process.
    zai: RwLock<Option<Arc<ZaiClient>>>,
}

impl LlmRouter {
    /// Create a router with only the local Ollama client configured.
    pub fn new(local_url: String, local_model: String) -> Self {
        Self {
            models: RwLock::new([(AiProvider::Local, local_model.clone())].into()),
            single_provider: RwLock::new(None),
            nvidia: nvidia::NvidiaClient::for_reasoning(&Default::default()),
            local: OllamaClient::new(local_url, local_model),
            gemini: None,
            claude: None,
            codex: None,
            zai: RwLock::new(None),
        }
    }

    /// Build a router from the full LLM config, wiring up whichever cloud
    /// providers have a resolvable API key. Providers without one are
    /// logged and left unconfigured rather than failing construction.
    pub fn from_config(cfg: &LlmConfig) -> Self {
        let local = OllamaClient::new(cfg.local.base_url.clone(), cfg.local.model.clone())
            .with_context(cfg.local.max_context);

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

        let zai = cfg.zai.as_ref().and_then(|c| match ZaiClient::new(c) {
            Ok(client) => Some(Arc::new(client)),
            Err(e) => {
                tracing::warn!("Z.AI provider not available: {e}");
                None
            }
        });

        let mut models = HashMap::new();
        models.insert(AiProvider::Local, cfg.local.model.clone());
        models.insert(AiProvider::Nvidia, cfg.nvidia.model.clone());
        for (provider, config) in [
            (AiProvider::GeminiFlash, &cfg.gemini),
            (AiProvider::Claude, &cfg.claude),
            (AiProvider::Codex, &cfg.codex),
            (AiProvider::Zai, &cfg.zai),
        ] {
            if let Some(config) = config {
                models.insert(provider, config.model.clone());
            }
        }
        Self {
            single_provider: RwLock::new(cfg.single_provider),
            nvidia: nvidia::NvidiaClient::for_reasoning(&cfg.nvidia),
            models: RwLock::new(models),
            local,
            gemini,
            claude,
            codex,
            zai: RwLock::new(zai),
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
        let single_provider = *self.single_provider.read().unwrap();
        if let Some(provider) = single_provider {
            if provider == AiProvider::Nvidia {
                return Ok(self.nvidia.complete(prompt, "low").await?.text);
            }
            return Ok(self.complete_with(prompt, provider).await?.text);
        }
        self.local.complete_raw(prompt).await
    }

    pub fn effective_provider(&self, provider: AiProvider) -> AiProvider {
        self.single_provider.read().unwrap().unwrap_or(provider)
    }

    pub fn uses_local_startup(&self) -> bool {
        let single_provider = *self.single_provider.read().unwrap();
        single_provider.is_none() || single_provider == Some(AiProvider::Local)
    }

    /// Which provider the master agent currently answers with. Defaults to
    /// `Local` when no single-provider mode is set (legacy multi-provider
    /// routing still uses the local model as its classifier and fallback).
    pub fn current_provider(&self) -> AiProvider {
        self.single_provider
            .read()
            .unwrap()
            .unwrap_or(AiProvider::Local)
    }

    /// Whether a Z.AI client is currently configured (via `hive.toml`/`Z_AI`
    /// at startup, or a key supplied later through [`Self::set_provider`]).
    pub fn zai_configured(&self) -> bool {
        self.zai.read().unwrap().is_some()
    }

    /// Whether NVIDIA is configured, i.e. `NVIDIA_API_KEY_FLASH` is set.
    pub fn nvidia_configured(&self) -> bool {
        nvidia_key_configured()
    }

    /// The local (Ollama) model name, for display in the master-agent picker.
    pub fn local_model_name(&self) -> String {
        self.models
            .read()
            .unwrap()
            .get(&AiProvider::Local)
            .cloned()
            .unwrap_or_default()
    }

    /// Explicitly select the master agent's sole provider.
    ///
    /// `Local` always succeeds. `Zai` requires either an already-configured
    /// client or a freshly supplied `api_key` (which replaces any previously
    /// configured Z.AI client). `Nvidia` requires `NVIDIA_API_KEY_FLASH` to
    /// already be set in the environment — no live key entry for it here.
    /// Any other provider is rejected: those are routed automatically by
    /// task complexity, not selectable as the master agent's sole provider.
    ///
    /// Once set, this disables fallback to the local model for every other
    /// provider (see `complete_formatted`) — the caller has explicitly
    /// opted out of ever silently running the local Qwen model again.
    pub fn set_provider(
        &self,
        provider: AiProvider,
        api_key: Option<String>,
    ) -> anyhow::Result<()> {
        match provider {
            AiProvider::Local => {}
            AiProvider::Zai => {
                if let Some(key) = api_key {
                    *self.zai.write().unwrap() = Some(Arc::new(ZaiClient::with_key(key)));
                }
                if self.zai.read().unwrap().is_none() {
                    anyhow::bail!("Z.AI API key required");
                }
            }
            AiProvider::Nvidia => {
                if !nvidia_key_configured() {
                    anyhow::bail!("NVIDIA is not configured: set {NVIDIA_KEY_ENV}");
                }
            }
            other => {
                anyhow::bail!("{other} cannot be selected as the master agent's sole provider")
            }
        }
        *self.single_provider.write().unwrap() = Some(provider);
        Ok(())
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
        self.complete_formatted(prompt, provider, None).await
    }

    /// Structured local output also applies when a cloud route falls back to Ollama.
    pub async fn complete_json_with(
        &self,
        prompt: &str,
        provider: AiProvider,
        schema: &serde_json::Value,
    ) -> anyhow::Result<LlmResponse> {
        self.complete_formatted(prompt, provider, Some(schema))
            .await
    }

    async fn local_formatted(
        &self,
        prompt: &str,
        schema: Option<&serde_json::Value>,
    ) -> anyhow::Result<String> {
        match schema {
            Some(schema) => self.local.complete_json(prompt, schema).await,
            None => self.local.complete_raw(prompt).await,
        }
    }

    async fn complete_formatted(
        &self,
        prompt: &str,
        provider: AiProvider,
        schema: Option<&serde_json::Value>,
    ) -> anyhow::Result<LlmResponse> {
        let provider = self.effective_provider(provider);
        // Only Ollama enforces the schema natively; cloud clients take a bare
        // prompt, so without this they invent field names the strict callers
        // (e.g. the delegation planner) reject. The local fallback below still
        // gets the original prompt and the native schema.
        let schema_prompt = match schema {
            Some(s) if provider != AiProvider::Local => Some(with_schema_instructions(prompt, s)),
            _ => None,
        };
        let cloud_prompt = schema_prompt.as_deref().unwrap_or(prompt);
        if provider == AiProvider::Nvidia {
            let mut response = self.nvidia.complete(cloud_prompt, "high").await?;
            if schema.is_some() {
                response.text = extract_json(&response.text);
            }
            return Ok(response);
        }
        let result = match provider {
            AiProvider::Nvidia => unreachable!(),
            AiProvider::Local => self.local_formatted(prompt, schema).await,
            AiProvider::GeminiFlash => match &self.gemini {
                Some(client) => client.complete(cloud_prompt).await,
                None => Err(anyhow::anyhow!(
                    "Gemini is not configured (set GEMINI_API_KEY or [llm.gemini] in hive.toml)"
                )),
            },
            AiProvider::Claude => match &self.claude {
                Some(client) => client.complete(cloud_prompt).await,
                None => Err(anyhow::anyhow!(
                    "Claude is not configured (set ANTHROPIC_API_KEY or [llm.claude] in hive.toml)"
                )),
            },
            AiProvider::Codex => match &self.codex {
                Some(client) => client.complete(cloud_prompt).await,
                None => Err(anyhow::anyhow!(
                    "Codex is not configured (set OPENAI_API_KEY or [llm.codex] in hive.toml)"
                )),
            },
            AiProvider::Zai => {
                // Clone the Arc and drop the read guard before the await —
                // holding a std::sync::RwLock guard across an await point
                // would poison the lock if this task is cancelled mid-hold.
                let client = self.zai.read().unwrap().clone();
                match client {
                    Some(client) => client.complete(cloud_prompt).await,
                    None => Err(anyhow::anyhow!(
                        "Z.AI is not configured (set Z_AI or [llm.zai] in hive.toml, or select it from the master-agent settings with an API key)"
                    )),
                }
            }
        };

        let single_provider = *self.single_provider.read().unwrap();
        match result {
            Ok(text) => Ok(LlmResponse {
                text: if schema_prompt.is_some() { extract_json(&text) } else { text },
                provider,
                model: self
                    .models
                    .read()
                    .unwrap()
                    .get(&provider)
                    .cloned()
                    .unwrap_or_default(),
            }),
            Err(e) if provider != AiProvider::Local && single_provider.is_none() => {
                tracing::warn!(
                    "Provider {provider} unavailable ({e}), falling back to local model"
                );
                let text = self.local_formatted(prompt, schema).await?;
                Ok(LlmResponse {
                    text,
                    provider: AiProvider::Local,
                    model: self
                        .models
                        .read()
                        .unwrap()
                        .get(&AiProvider::Local)
                        .cloned()
                        .unwrap_or_default(),
                })
            }
            Err(e) => Err(e),
        }
    }
}

/// Appends the JSON Schema to a prompt for providers that cannot enforce it.
fn with_schema_instructions(prompt: &str, schema: &serde_json::Value) -> String {
    format!(
        "{prompt}\n\nRespond with only one JSON object: no prose and no markdown fences. \
         It must validate against this JSON Schema, using exactly these property names \
         and no others:\n{schema}"
    )
}

/// Pulls the JSON object out of a reply that wrapped it in fences or prose.
fn extract_json(text: &str) -> String {
    let trimmed = text.trim();
    match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if start < end => trimmed[start..=end].to_string(),
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::config::{LlmConfig, LocalLlmConfig, NvidiaConfig};

    fn base_config() -> LlmConfig {
        LlmConfig {
            single_provider: None,
            nvidia: NvidiaConfig::default(),
            local: LocalLlmConfig::default(),
            gemini: None,
            claude: None,
            codex: None,
            zai: None,
        }
    }


    #[test]
    fn schema_instructions_name_every_field() {
        let schema = serde_json::json!({"properties": {"key": {}, "objective": {}}});
        let prompt = with_schema_instructions("Plan it.", &schema);
        assert!(prompt.starts_with("Plan it."));
        assert!(prompt.contains("\"key\"") && prompt.contains("\"objective\""));
        assert!(prompt.contains("no others"));
    }

    #[test]
    fn extract_json_strips_fences_and_prose() {
        assert_eq!(extract_json("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("Here is the plan: {\"a\":{\"b\":2}} done"), "{\"a\":{\"b\":2}}");
        assert_eq!(extract_json("  {\"a\":1}  "), "{\"a\":1}");
        assert_eq!(extract_json("no json here"), "no json here");
    }

    #[test]
    fn set_provider_refuses_zai_without_any_key() {
        let router = LlmRouter::from_config(&base_config());
        let err = router.set_provider(AiProvider::Zai, None).unwrap_err();
        assert!(err.to_string().contains("API key"), "{err}");
        assert_eq!(router.current_provider(), AiProvider::Local);
        assert!(!router.zai_configured());
    }

    #[test]
    fn set_provider_accepts_zai_with_a_key_and_local_always_succeeds() {
        let router = LlmRouter::from_config(&base_config());
        router
            .set_provider(AiProvider::Zai, Some("test-key".into()))
            .unwrap();
        assert_eq!(router.current_provider(), AiProvider::Zai);
        assert!(router.zai_configured());

        router.set_provider(AiProvider::Local, None).unwrap();
        assert_eq!(router.current_provider(), AiProvider::Local);
        // Switching back to local does not drop the previously entered key.
        assert!(router.zai_configured());
    }

    #[test]
    fn set_provider_rejects_non_selectable_providers() {
        let router = LlmRouter::from_config(&base_config());
        let err = router.set_provider(AiProvider::Claude, None).unwrap_err();
        assert!(err.to_string().contains("cannot be selected"), "{err}");
    }

    // The core "turn off local qwen" invariant: once Z.AI is the sole
    // provider, a request never silently falls back to the local model —
    // even when no Z.AI client is actually configured to serve it. A
    // regression here would mean the master agent quietly keeps running
    // local Qwen after the user asked to shift fully to Z.AI.
    #[tokio::test]
    async fn selecting_zai_disables_fallback_to_local_even_when_unconfigured() {
        let mut cfg = base_config();
        cfg.single_provider = Some(AiProvider::Zai);
        // Unreachable on purpose: if this were ever hit, the test would hang
        // or fail with a connection error instead of the expected message.
        cfg.local.base_url = "http://127.0.0.1:1".into();
        let router = LlmRouter::from_config(&cfg);
        assert!(!router.zai_configured());

        let result = router.complete_with("hello", AiProvider::Claude).await;
        let err = result
            .expect_err("must not silently fall back to local when Z.AI is the sole provider");
        assert!(err.to_string().contains("Z.AI is not configured"), "{err}");
    }
}
