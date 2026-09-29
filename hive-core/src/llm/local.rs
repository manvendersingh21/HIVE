//! Ollama client — local LLM for classification, planning fallback, and embeddings.

use serde::{Deserialize, Serialize};

use super::ChatMessage;

/// Client for a local Ollama server.
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    max_context: u32,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    options: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a serde_json::Value>,
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
    /// Private memory must never follow a proxy, redirect, or remote endpoint.
    /// The ordinary planner client retains its existing routing behaviour.
    pub fn local_only(base_url: String, model: String) -> anyhow::Result<Self> {
        let url = reqwest::Url::parse(&base_url)?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        anyhow::ensure!(
            loopback
                && matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "memory ingestion requires a loopback Ollama URL"
        );
        anyhow::ensure!(
            !model.trim().is_empty() && !model.to_lowercase().contains("cloud"),
            "memory ingestion requires a locally installed Ollama model"
        );
        Ok(Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .resolve("localhost", ([127, 0, 0, 1], 0).into())
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            base_url,
            model,
            max_context: 8192,
        })
    }

    /// Probe once per ingest batch, before sending any transcript. Both models
    /// must already be installed; ingestion never pulls or signs into models.
    pub async fn require_local_models(&self, models: &[&str]) -> anyhow::Result<()> {
        let response: serde_json::Value =
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let response = self
                    .http
                    .get(format!("{}/api/tags", self.base_url.trim_end_matches('/')))
                    .send()
                    .await?;
                anyhow::ensure!(
                    response.status().is_success(),
                    "Ollama model probe failed: {}",
                    response.status()
                );
                Ok::<_, anyhow::Error>(response.json().await?)
            })
            .await??;
        for model in models {
            let installed = response["models"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| {
                    let name = m["name"]
                        .as_str()
                        .or_else(|| m["model"].as_str())
                        .unwrap_or("");
                    (name == *model || name == format!("{model}:latest"))
                        && m.get("remote_model").is_none()
                        && m.get("remote_host").is_none()
                        && !name.to_lowercase().contains("cloud")
                });
            anyhow::ensure!(installed, "local Ollama model {model} is unavailable");
        }
        Ok(())
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
    /// Create a new client pointed at `base_url` (e.g. `http://localhost:11434`).
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            model,
            max_context: 8192,
        }
    }

    pub fn with_context(mut self, max_context: u32) -> Self {
        self.max_context = max_context.clamp(4096, 131072);
        self
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
        self.chat_formatted(messages, None).await
    }

    async fn chat_formatted(
        &self,
        messages: &[ChatMessage],
        format: Option<&serde_json::Value>,
    ) -> anyhow::Result<String> {
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let req = ChatRequest {
            model: &self.model,
            messages,
            stream: false,
            options: serde_json::json!({"num_ctx": self.max_context, "num_predict": 8192, "temperature": 0.1}),
            format,
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

    /// Constrain JSON syntax and required fields at generation time. Shell
    /// scripts contain backslashes that free-form model output often misescapes.
    pub async fn complete_json(
        &self,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> anyhow::Result<String> {
        self.chat_formatted(&[ChatMessage::user(prompt)], Some(schema))
            .await
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
