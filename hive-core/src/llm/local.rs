//! Ollama client — local LLM for classification, planning fallback, and embeddings.

use serde::{Deserialize, Serialize};

use super::ChatMessage;

/// Client for a local Ollama server.
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    /// Suppress reasoning tokens on models that emit them.
    ///
    /// Qwen3.x and friends default to "thinking", spending hundreds of tokens
    /// on visible reasoning before answering. Every job Hive gives the local
    /// model — a one-word complexity label, a fixed-shape JSON plan, a JSON
    /// safety verdict — has a known output shape, so that reasoning is pure
    /// latency, and with a bounded token budget it can consume the whole
    /// allowance and return an empty answer. Measured on qwen3.5:9b: 0/3
    /// usable responses with thinking on, 3/3 with it off, and plan latency
    /// dropped from 18.1s to 7.0s.
    ///
    /// Ollama ignores the field for models that do not think, so it is safe
    /// to always send.
    think: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

impl OllamaClient {
    /// Create a new client pointed at `base_url` (e.g. `http://localhost:11434`).
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            model,
        }
    }

    /// Send a multi-turn chat completion request.
    /// Cheap reachability probe against the Ollama server.
    ///
    /// The same binary runs on workers that have no local model; callers use
    /// this to decide whether to offer agent features at all, rather than
    /// advertising a chat that fails on the first message.
    pub async fn is_available(&self) -> bool {
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.http.get(&url).send(),
            )
            .await,
            Ok(Ok(response)) if response.status().is_success()
        )
    }

    pub async fn chat(&self, messages: &[ChatMessage]) -> anyhow::Result<String> {
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let req = ChatRequest {
            model: &self.model,
            messages,
            stream: false,
            think: false,
        };

        let resp = self.http.post(&url).json(&req).send().await.map_err(|e| {
            anyhow::anyhow!("Failed to reach Ollama at {url}: {e} (is `ollama serve` running?)")
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {body}");
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Ollama response: {e}"))?;
        Ok(parsed.message.content)
    }

    /// Send a single-turn completion request.
    pub async fn complete_raw(&self, prompt: &str) -> anyhow::Result<String> {
        self.chat(&[ChatMessage::user(prompt)]).await
    }

    /// Generate an embedding vector for `input` (used by the RAG index in Phase 9).
    pub async fn embed(&self, input: &str) -> anyhow::Result<Vec<f32>> {
        let url = format!("{}/api/embeddings", self.base_url.trim_end_matches('/'));
        let req = EmbedRequest {
            model: &self.model,
            prompt: input,
        };

        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach Ollama at {url}: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama embeddings returned {status}: {body}");
        }

        let parsed: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Ollama embeddings response: {e}"))?;
        Ok(parsed.embedding)
    }
}
