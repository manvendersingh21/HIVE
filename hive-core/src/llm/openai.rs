//! OpenAI client — cloud provider for CODE_HEAVY tasks ("Codex" in the routing table).

use hive_common::config::CloudLlmConfig;
use serde::{Deserialize, Serialize};

/// Client for the OpenAI Chat Completions API.
pub struct OpenAiClient {
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

impl OpenAiClient {
    /// Build a client from config, resolving the API key from config or
    /// `OPENAI_API_KEY`. Fails if no key is available anywhere.
    pub fn new(cfg: &CloudLlmConfig) -> anyhow::Result<Self> {
        let api_key = cfg.resolve_api_key("OPENAI_API_KEY")?;
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        Ok(Self {
            http: reqwest::Client::new(),
            api_key,
            model: cfg.model.clone(),
            base_url,
        })
    }

    /// Send a single-turn completion request.
    pub async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
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
            .map_err(|e| anyhow::anyhow!("Failed to reach OpenAI API: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API returned {status}: {body}");
        }

        let parsed: ChatCompletionsResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse OpenAI response: {e}"))?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("OpenAI returned no choices"))
    }
}
