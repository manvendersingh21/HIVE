//! Claude client — cloud provider for COMPLEX tasks requiring deep reasoning.

use hive_common::config::CloudLlmConfig;
use serde::{Deserialize, Serialize};

/// Client for the Anthropic Messages API.
pub struct ClaudeClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

impl ClaudeClient {
    /// Build a client from config, resolving the API key from config or
    /// `ANTHROPIC_API_KEY`. Fails if no key is available anywhere.
    pub fn new(cfg: &CloudLlmConfig) -> anyhow::Result<Self> {
        let api_key = cfg.resolve_api_key("ANTHROPIC_API_KEY")?;
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Ok(Self {
            http: reqwest::Client::new(),
            api_key,
            model: cfg.model.clone(),
            base_url,
        })
    }

    /// Send a single-turn completion request.
    pub async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let req = MessagesRequest {
            model: &self.model,
            max_tokens: 4096,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach Claude API: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Claude API returned {status}: {body}");
        }

        let parsed: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Claude response: {e}"))?;

        Ok(parsed
            .content
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join(""))
    }
}
