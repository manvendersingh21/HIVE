//! Gemini client — cloud provider for MEDIUM-complexity tasks.

use hive_common::config::CloudLlmConfig;
use serde::{Deserialize, Serialize};

/// Client for the Google Generative Language API (Gemini).
pub struct GeminiClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

#[derive(Serialize)]
struct GenerateContentRequest<'a> {
    contents: Vec<Content<'a>>,
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'static str,
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Deserialize)]
struct CandidatePart {
    #[serde(default)]
    text: String,
}

impl GeminiClient {
    /// Build a client from config, resolving the API key from config or
    /// `GEMINI_API_KEY`. Fails if no key is available anywhere.
    pub fn new(cfg: &CloudLlmConfig) -> anyhow::Result<Self> {
        let api_key = cfg.resolve_api_key("GEMINI_API_KEY")?;
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
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
            "{}/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );
        let req = GenerateContentRequest {
            contents: vec![Content {
                role: "user",
                parts: vec![Part { text: prompt }],
            }],
        };

        // API key goes in a header, never the URL, so it can't leak through
        // reqwest error messages or logs that include the request URL.
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach Gemini: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API returned {status}: {body}");
        }

        let parsed: GenerateContentResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Gemini response: {e}"))?;

        parsed
            .candidates
            .into_iter()
            .next()
            .and_then(|c| c.content.parts.into_iter().next())
            .map(|p| p.text)
            .ok_or_else(|| anyhow::anyhow!("Gemini returned no candidates"))
    }
}
