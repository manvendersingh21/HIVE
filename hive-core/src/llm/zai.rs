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

/// Never carries `response_format`: GLM's `json_object` mode deletes every
/// lowercase `json` from its answer (`corpus.jsonl` becomes `corpus.l`), see
/// https://github.com/zai-org/GLM-5/issues/133. Structured calls rely on the
/// schema instructions in the prompt, JSON extraction and one retry instead.
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
    #[serde(default)]
    finish_reason: String,
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

    /// Same-crate tests assert key precedence (env/config vs. the persisted
    /// state file); production code must never read the key back out.
    #[cfg(test)]
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
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

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Z.AI returned no choices"))?;

        anyhow::ensure!(
            choice.finish_reason == "stop",
            "Z.AI response incomplete or truncated (finish_reason: {})",
            choice.finish_reason
        );

        let text = choice.message.content.trim();
        anyhow::ensure!(!text.is_empty(), "Z.AI returned an empty final answer");

        Ok(text.to_string())
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
pub(crate) mod tests {
    use super::*;
    use crate::llm::nvidia::tests::{server, server_fn, Requests};
    use serde_json::json;
    use std::time::Duration;

    /// A mocked Z.AI endpoint that answers `answers` in order and behaves like
    /// GLM (zai-org/GLM-5#133): under `response_format: json_object` every
    /// lowercase `json` disappears from the answer; otherwise it is verbatim.
    pub(crate) async fn glm_server(
        answers: Vec<String>,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        let count = answers.len();
        let answers = std::sync::Mutex::new(answers.into_iter());
        server_fn(count, move |body| {
            let answer = answers.lock().unwrap().next().expect("scripted answer");
            let content = if body["response_format"]["type"] == "json_object" {
                answer.replace("json", "")
            } else {
                answer
            };
            (200, json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
        })
        .await
    }

    fn cfg(base_url: String) -> CloudLlmConfig {
        CloudLlmConfig {
            model: "test-model".into(),
            api_key: Some("test-key".into()),
            api_key_env: None,
            base_url: Some(base_url),
        }
    }

    #[tokio::test]
    async fn truncated_completions_are_rejected() {
        let (url, _requests, task) = server(vec![(
            200,
            json!({"choices":[{"finish_reason":"length","message":{"content":"cut off mid-sent"}}]}),
            Duration::ZERO,
        )])
        .await;
        let client = ZaiClient::new(&cfg(url)).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        assert!(err.to_string().contains("incomplete or truncated"), "{err}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn missing_finish_reason_is_treated_as_incomplete() {
        let (url, _requests, task) = server(vec![(
            200,
            json!({"choices":[{"message":{"content":"whatever"}}]}),
            Duration::ZERO,
        )])
        .await;
        let client = ZaiClient::new(&cfg(url)).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        assert!(err.to_string().contains("incomplete or truncated"), "{err}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn empty_content_is_rejected_even_with_finish_reason_stop() {
        let (url, _requests, task) = server(vec![(
            200,
            json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}),
            Duration::ZERO,
        )])
        .await;
        let client = ZaiClient::new(&cfg(url)).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        assert!(err.to_string().contains("empty final answer"), "{err}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_complete_stop_response_is_accepted() {
        let (url, _requests, task) = server(vec![(
            200,
            json!({"choices":[{"finish_reason":"stop","message":{"content":"full answer"}}]}),
            Duration::ZERO,
        )])
        .await;
        let client = ZaiClient::new(&cfg(url)).unwrap();
        let text = client.complete("hi").await.unwrap();
        assert_eq!(text, "full answer");
        task.await.unwrap();
    }

    fn zai_router(url: String) -> crate::llm::LlmRouter {
        crate::llm::LlmRouter::from_config(&hive_common::config::LlmConfig {
            single_provider: Some(hive_common::AiProvider::Zai),
            nvidia: Default::default(),
            local: hive_common::config::LocalLlmConfig {
                base_url: "http://127.0.0.1:1".into(),
                ..Default::default()
            },
            gemini: None,
            claude: None,
            codex: None,
            zai: Some(cfg(url)),
        })
    }

    #[tokio::test]
    async fn schema_calls_never_request_json_mode() {
        let ok = |text: &str| {
            (
                200,
                json!({"choices":[{"finish_reason":"stop","message":{"content":text}}]}),
                Duration::ZERO,
            )
        };
        let (url, requests, task) = server(vec![ok("{\"a\":1}"), ok("hello")]).await;
        let router = zai_router(url);
        let schema = json!({"type":"object","properties":{"a":{"type":"integer"}}});
        let structured = router
            .complete_json_with("give json", hive_common::AiProvider::Claude, &schema)
            .await
            .unwrap();
        assert_eq!(structured.text, "{\"a\":1}");
        let plain = router
            .complete_with("say hi", hive_common::AiProvider::Claude)
            .await
            .unwrap();
        assert_eq!(plain.text, "hello");
        task.await.unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (headers, body) = &requests[0];
        assert!(headers.starts_with("POST /v1/chat/completions"), "{headers}");
        assert!(
            body.get("response_format").is_none(),
            "GLM JSON mode drops lowercase json tokens: {body}"
        );
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("give json"));
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Respond with only one JSON object"));
        let (_, body) = &requests[1];
        assert!(
            body.get("response_format").is_none(),
            "plain chat must not request JSON mode: {body}"
        );
        assert_eq!(body["messages"][0]["content"], "say hi");
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn planner__token_strip_the_glm_mock_strips_only_under_json_mode() {
        let answer = r#"{"objective":"read corpus.jsonl; import json; keep JSON"}"#;
        let (url, _requests, task) = glm_server(vec![answer.into(), answer.into()]).await;
        let post = |format: Option<serde_json::Value>| {
            let mut body = json!({"model":"m","messages":[{"role":"user","content":"p"}]});
            if let Some(format) = format {
                body["response_format"] = format;
            }
            let url = format!("{url}/chat/completions");
            async move {
                let reply: serde_json::Value =
                    reqwest::Client::new().post(url).json(&body).send().await.unwrap().json().await.unwrap();
                reply["choices"][0]["message"]["content"].as_str().unwrap().to_string()
            }
        };
        assert_eq!(
            post(Some(json!({"type":"json_object"}))).await,
            r#"{"objective":"read corpus.l; import ; keep JSON"}"#
        );
        assert_eq!(post(None).await, answer);
        task.await.unwrap();
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn planner__token_strip_structured_zai_answers_keep_lowercase_json() {
        let answer = r#"{"objective":"read corpus.jsonl; import json;json.loads(x); stats.json"}"#;
        let (url, requests, task) = glm_server(vec![answer.into()]).await;
        let schema = json!({"type":"object","properties":{"objective":{"type":"string"}}});
        let reply = zai_router(url)
            .complete_json_with("plan", hive_common::AiProvider::Claude, &schema)
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(reply.text, answer);
        assert!(requests.lock().unwrap()[0].1.get("response_format").is_none());
    }

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
