//! Z.AI client — hosted GLM models via an OpenAI-compatible Chat Completions API.

use hive_common::config::CloudLlmConfig;
use serde::{Deserialize, Serialize};

/// Default GLM model on Z.AI's coding-plan endpoint.
const DEFAULT_MODEL: &str = "glm-5.3";
/// The coding-plan endpoint: the general `/api/paas/v4` endpoint bills
/// against a separate balance that a coding-plan key does not carry.
const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// Client for the Z.AI Chat Completions API.
pub struct ZaiClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

#[derive(Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatCompletionsResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: String,
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    #[serde(default)]
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

impl ZaiClient {
    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Build a client from config, resolving the API key from config or
    /// the `Z_AI` environment variable. Fails if no key is available anywhere.
    pub fn new(cfg: &CloudLlmConfig) -> anyhow::Result<Self> {
        let api_key = cfg.resolve_api_key("Z_AI")?;
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self {
            http: reqwest::Client::new(),
            api_key,
            model: cfg.model.clone(),
            base_url,
        })
    }

    /// Build a client directly from a caller-supplied key (e.g. one entered
    /// through the master-agent settings API), using the default coding-plan
    /// model and endpoint. Never resolves from config or the environment.
    pub fn with_key(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Send a single-turn completion request.
    pub async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let req = ChatCompletionsRequest {
            model: &self.model,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach Z.AI API: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Z.AI API returned {status}: {body}");
        }

        let parsed: ChatCompletionsResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Z.AI response: {e}"))?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("Z.AI returned no choices"))
    }

    /// Generate an embedding vector for `input` (used by the RAG index and
    /// entity dedup).
    pub async fn embed(&self, input: &str) -> anyhow::Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let req = EmbeddingsRequest {
            model: &self.model,
            input,
        };

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach Z.AI API: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Z.AI API returned {status}: {body}");
        }

        let parsed: EmbeddingsResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Z.AI embeddings response: {e}"))?;

        parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| anyhow::anyhow!("Z.AI returned no embedding data"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits the real Z.AI coding-plan API. Run with:
    /// `Z_AI=... cargo test -p hive-core --offline zai:: -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the live Z.AI API; requires Z_AI in the environment"]
    async fn live_coding_plan_completion() {
        let cfg = CloudLlmConfig {
            model: "glm-5.3".into(),
            ..Default::default()
        };
        let client = ZaiClient::new(&cfg).expect("Z_AI must be set");
        let text = client
            .complete("Reply with exactly: OK")
            .await
            .expect("live Z.AI call failed");
        assert!(text.contains("OK"), "unexpected response: {text}");
    }
}
