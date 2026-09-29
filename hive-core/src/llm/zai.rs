//! Z.AI client — hosted GLM models via an OpenAI-compatible Chat Completions API.

use hive_common::config::CloudLlmConfig;
use serde::{Deserialize, Serialize};

/// Default GLM model on Z.AI's coding-plan endpoint.
const DEFAULT_MODEL: &str = "glm-5.3";
/// The coding-plan endpoint: the general `/api/paas/v4` endpoint bills
/// against a separate balance that a coding-plan key does not carry.
const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// Body `error.code` values that mean the key is out of balance, rate limited
/// or past a usage window (https://docs.z.ai/api-reference/api-code). Any
/// HTTP 429 is a quota error too, whatever its code.
pub const QUOTA_ERROR_CODES: &[&str] = &[
    "1113", // insufficient balance or no resource package
    "1302", // rate limit reached for requests
    "1308", // usage limit reached for {n} {unit}, e.g. "5 hour"
    "1309", // GLM Coding Plan package expired
    "1310", // weekly/monthly limit exhausted
    "1313", // request frequency limited by the Fair Usage Policy
    "1314", // enterprise package expired
    "1316", // 5-hour limit reached, insufficient balance for extra usage
    "1317", // 7-day limit reached, insufficient balance for extra usage
    "1318", // 5-hour limit reached, monthly spend limit
    "1319", // 7-day limit reached, monthly spend limit
    "1320", // 5-hour limit reached, monthly spend limit
    "1321", // 7-day limit reached, monthly spend limit
];

/// Z.AI refused the request for quota reasons (HTTP 429 or a code in
/// [`QUOTA_ERROR_CODES`]). The router fails over to the local model on this
/// error and on no other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZaiQuotaError {
    pub status: u16,
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ZaiQuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Z.AI quota exhausted (HTTP {}", self.status)?;
        if let Some(code) = &self.code {
            write!(f, ", code {code}")?;
        }
        write!(f, "): {}", self.message)
    }
}

impl std::error::Error for ZaiQuotaError {}

/// The `error.code` / `error.message` pair of a Z.AI error body. Codes are
/// documented as strings but accepted as numbers too.
fn body_error(body: &serde_json::Value) -> Option<(Option<String>, String)> {
    let error = body.get("error")?;
    let code = match error.get("code") {
        Some(serde_json::Value::String(s)) => Some(s.trim().to_string()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    let message = error
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    Some((code, message))
}

/// A quota error when `status` is 429 or the body carries a quota code.
fn quota_error(status: u16, body: &str) -> Option<ZaiQuotaError> {
    let parsed = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| body_error(&v));
    let (code, message) = parsed.unwrap_or((None, String::new()));
    let listed = code
        .as_deref()
        .is_some_and(|c| QUOTA_ERROR_CODES.contains(&c));
    if status != 429 && !listed {
        return None;
    }
    let message = if message.is_empty() { body.trim().to_string() } else { message };
    Some(ZaiQuotaError { status, code, message })
}

/// Turn a Z.AI response into its body text, or the error it carries.
async fn checked_body(resp: reqwest::Response) -> anyhow::Result<String> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read Z.AI response: {e}"))?;
    if let Some(quota) = quota_error(status.as_u16(), &body) {
        return Err(quota.into());
    }
    if !status.is_success() {
        anyhow::bail!("Z.AI API returned {status}: {body}");
    }
    Ok(body)
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

/// Z.AI's JSON mode guarantees syntactically valid JSON; the shape still
/// comes from the schema instructions in the prompt.
#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
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
        self.send(prompt, None).await
    }

    /// Send a single-turn completion request in JSON mode, for structured
    /// (schema) calls. Plain chat must use [`Self::complete`].
    pub async fn complete_json(&self, prompt: &str) -> anyhow::Result<String> {
        self.send(prompt, Some(ResponseFormat { kind: "json_object" }))
            .await
    }

    async fn send(
        &self,
        prompt: &str,
        response_format: Option<ResponseFormat>,
    ) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let req = ChatCompletionsRequest {
            model: &self.model,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
            response_format,
        };

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach Z.AI API: {e}"))?;

        let body = checked_body(resp).await?;
        let parsed: ChatCompletionsResponse = serde_json::from_str(&body)
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

        let body = checked_body(resp).await?;
        let parsed: EmbeddingsResponse = serde_json::from_str(&body)
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
    use crate::llm::nvidia::tests::server;
    use serde_json::json;
    use std::time::Duration;

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
    async fn json_mode_is_requested_for_schema_calls_only() {
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
        assert_eq!(body["response_format"], json!({"type":"json_object"}));
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("give json"));
        let (_, body) = &requests[1];
        assert!(
            body.get("response_format").is_none(),
            "plain chat must not request JSON mode: {body}"
        );
        assert_eq!(body["messages"][0]["content"], "say hi");
    }

    #[test]
    fn quota_errors_are_429_or_a_listed_body_code() {
        let body = |code: serde_json::Value| {
            json!({"error":{"code":code,"message":"Usage limit reached for 5 hour"}}).to_string()
        };
        let quota = quota_error(400, &body(json!("1308"))).expect("listed code");
        assert_eq!(quota.code.as_deref(), Some("1308"));
        assert_eq!(quota.message, "Usage limit reached for 5 hour");
        assert!(quota.to_string().contains("code 1308"), "{quota}");
        assert!(quota_error(200, &body(json!(1113))).is_some(), "numeric codes count");
        assert!(quota_error(429, "rate limited").is_some(), "any 429");
        assert!(quota_error(429, &body(json!("1305"))).is_some(), "any 429");
        for code in QUOTA_ERROR_CODES {
            assert!(quota_error(400, &body(json!(code))).is_some(), "{code}");
        }
        assert!(QUOTA_ERROR_CODES.contains(&"1308"));

        assert!(quota_error(500, "Internal Error").is_none());
        assert!(quota_error(503, &body(json!("1234"))).is_none());
        assert!(quota_error(400, &body(json!("1214"))).is_none());
        assert!(quota_error(401, &body(json!("1001"))).is_none());
    }

    fn quota_reply(status: u16) -> (u16, serde_json::Value, Duration) {
        (
            status,
            json!({"error":{"code":"1308","message":"Usage limit reached for 5 hour. Your limit will reset at 2026-09-29 08:00:00"}}),
            Duration::ZERO,
        )
    }

    fn zai_answer(text: &str) -> (u16, serde_json::Value, Duration) {
        (
            200,
            json!({"choices":[{"finish_reason":"stop","message":{"content":text}}]}),
            Duration::ZERO,
        )
    }

    fn ollama_answer(text: &str) -> (u16, serde_json::Value, Duration) {
        (200, json!({"message":{"content":text}}), Duration::ZERO)
    }

    type ManualClock = std::sync::Arc<std::sync::Mutex<chrono::DateTime<chrono::Utc>>>;

    /// A Z.AI-only router whose `[llm.local]` points at `local_url`, on a
    /// clock the test advances by hand.
    fn failover_router(zai_url: String, local_url: &str) -> (crate::llm::LlmRouter, ManualClock) {
        let now: ManualClock = std::sync::Arc::new(std::sync::Mutex::new(
            chrono::DateTime::parse_from_rfc3339("2026-09-29T09:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ));
        let clock = now.clone();
        let router = crate::llm::LlmRouter::from_config(&hive_common::config::LlmConfig {
            single_provider: Some(hive_common::AiProvider::Zai),
            nvidia: Default::default(),
            local: hive_common::config::LocalLlmConfig {
                base_url: local_url.into(),
                model: "qwen-test".into(),
                ..Default::default()
            },
            gemini: None,
            claude: None,
            codex: None,
            zai: Some(cfg(zai_url)),
        })
        .with_clock(std::sync::Arc::new(move || *clock.lock().unwrap()));
        (router, now)
    }

    fn advance(clock: &ManualClock, by: Duration) {
        let mut now = clock.lock().unwrap();
        *now += chrono::Duration::from_std(by).unwrap();
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn warn_lines(logs: &Captured) -> Vec<String> {
        String::from_utf8(logs.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|l| l.contains("WARN"))
            .map(str::to_string)
            .collect()
    }

    async fn assert_quota_answers_locally(status: u16) {
        let (zai_url, zai_requests, zai_task) = server(vec![quota_reply(status)]).await;
        let (local_url, local_requests, local_task) =
            server(vec![ollama_answer("local answer")]).await;
        let (router, _clock) = failover_router(zai_url, &local_url);

        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let response = router
            .complete_with("hello", hive_common::AiProvider::Claude)
            .await
            .expect("a quota error is answered by the local model");
        assert_eq!(response.text, "local answer");
        assert_eq!(response.provider, hive_common::AiProvider::Local);
        assert_eq!(response.model, "qwen-test");
        zai_task.await.unwrap();
        local_task.await.unwrap();
        assert_eq!(zai_requests.lock().unwrap().len(), 1);
        assert_eq!(local_requests.lock().unwrap().len(), 1);

        let warns = warn_lines(&logs);
        assert_eq!(warns.len(), 1, "exactly one warn line: {warns:?}");
        assert!(warns[0].contains("1308") && warns[0].contains("local model"), "{}", warns[0]);

        let fallback = router.zai_fallback().expect("cooldown started");
        assert_eq!(fallback.error.code.as_deref(), Some("1308"));
        assert_eq!(fallback.error.status, status);
        assert_eq!(
            fallback.until.to_rfc3339(),
            "2026-09-29T09:30:00+00:00",
            "cooldown is ZAI_QUOTA_COOLDOWN from the injected clock"
        );
        assert_eq!(router.current_provider(), hive_common::AiProvider::Zai);
        assert_eq!(router.active_provider(), hive_common::AiProvider::Local);
    }

    #[tokio::test]
    async fn http_429_is_answered_by_the_local_model() {
        assert_quota_answers_locally(429).await;
    }

    #[tokio::test]
    async fn code_1308_is_answered_by_the_local_model_whatever_the_status() {
        assert_quota_answers_locally(400).await;
    }

    #[tokio::test]
    async fn server_errors_do_not_fail_over() {
        let (zai_url, _zai_requests, zai_task) = server(vec![(
            500,
            json!({"error":{"code":"1234","message":"Network error"}}),
            Duration::ZERO,
        )])
        .await;
        let (local_url, local_requests, local_task) =
            server(vec![ollama_answer("must not be used")]).await;
        let (router, _clock) = failover_router(zai_url, &local_url);

        let err = router
            .complete_with("hello", hive_common::AiProvider::Claude)
            .await
            .expect_err("a 5xx is an ordinary error in single-provider mode");
        assert!(!err.is::<ZaiQuotaError>(), "{err}");
        assert!(err.to_string().contains("500"), "{err}");
        zai_task.await.unwrap();
        local_task.abort();
        assert!(local_requests.lock().unwrap().is_empty(), "no local call");
        assert!(router.zai_fallback().is_none());
        assert_eq!(router.active_provider(), hive_common::AiProvider::Zai);
    }

    #[tokio::test]
    async fn unreachable_zai_does_not_fail_over() {
        let (local_url, local_requests, local_task) =
            server(vec![ollama_answer("must not be used")]).await;
        let (router, _clock) = failover_router("http://127.0.0.1:1".into(), &local_url);
        let err = router
            .complete_with("hello", hive_common::AiProvider::Claude)
            .await
            .expect_err("a transport error is an ordinary error");
        assert!(err.to_string().contains("Failed to reach Z.AI"), "{err}");
        local_task.abort();
        assert!(local_requests.lock().unwrap().is_empty());
        assert!(router.zai_fallback().is_none());
    }

    #[tokio::test]
    async fn zai_is_skipped_during_the_cooldown_and_called_again_after_it() {
        let (zai_url, zai_requests, zai_task) =
            server(vec![quota_reply(429), zai_answer("zai is back")]).await;
        let (local_url, local_requests, local_task) = server(vec![
            ollama_answer("local 1"),
            ollama_answer("local 2"),
            ollama_answer("{\"a\":1}"),
            ollama_answer("local 3"),
        ])
        .await;
        let (router, clock) = failover_router(zai_url, &local_url);
        let schema = json!({"type":"object","properties":{"a":{"type":"integer"}}});

        let first = router
            .complete_with("one", hive_common::AiProvider::Claude)
            .await
            .unwrap();
        assert_eq!(first.text, "local 1");

        // Every Z.AI call path stays local for the whole cooldown.
        advance(&clock, Duration::from_secs(10 * 60));
        let plain = router
            .route_and_execute("two", hive_common::Complexity::Complex)
            .await
            .unwrap();
        assert_eq!((plain.text.as_str(), plain.provider), ("local 2", hive_common::AiProvider::Local));
        let structured = router
            .complete_json_with("three", hive_common::AiProvider::Codex, &schema)
            .await
            .unwrap();
        assert_eq!(structured.text, "{\"a\":1}");
        advance(&clock, crate::llm::ZAI_QUOTA_COOLDOWN - Duration::from_secs(10 * 60 + 1));
        assert_eq!(router.local_complete("four").await.unwrap(), "local 3");
        assert_eq!(zai_requests.lock().unwrap().len(), 1, "no Z.AI call during the cooldown");
        local_task.await.unwrap();
        {
            let local_requests = local_requests.lock().unwrap();
            assert_eq!(local_requests.len(), 4);
            assert_eq!(local_requests[2].1["format"], schema, "local keeps the native schema");
        }

        advance(&clock, Duration::from_secs(1));
        assert!(router.zai_fallback().is_none(), "cooldown over");
        let back = router
            .complete_with("five", hive_common::AiProvider::Claude)
            .await
            .unwrap();
        assert_eq!(back.text, "zai is back");
        assert_eq!(back.provider, hive_common::AiProvider::Zai);
        zai_task.await.unwrap();
        assert_eq!(zai_requests.lock().unwrap().len(), 2, "Z.AI called again after the cooldown");
        assert_eq!(router.active_provider(), hive_common::AiProvider::Zai);
    }

    #[tokio::test]
    async fn the_quota_error_is_returned_when_the_local_model_fails_too() {
        let (zai_url, zai_requests, zai_task) = server(vec![quota_reply(429)]).await;
        let (router, _clock) = failover_router(zai_url, "http://127.0.0.1:1");

        let err = router
            .complete_with("hello", hive_common::AiProvider::Claude)
            .await
            .expect_err("local is unreachable");
        let quota = err.downcast_ref::<ZaiQuotaError>().expect("the original quota error");
        assert_eq!(quota.code.as_deref(), Some("1308"));
        zai_task.await.unwrap();

        // Still in the cooldown: Z.AI is not called, the quota error stands.
        let err = router
            .complete_with("again", hive_common::AiProvider::Claude)
            .await
            .expect_err("local is still unreachable");
        assert!(err.is::<ZaiQuotaError>(), "{err}");
        assert_eq!(zai_requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_new_zai_key_ends_the_cooldown() {
        let (zai_url, _zai_requests, zai_task) = server(vec![quota_reply(429)]).await;
        let (router, _clock) = failover_router(zai_url, "http://127.0.0.1:1");
        let _ = router
            .complete_with("hello", hive_common::AiProvider::Claude)
            .await;
        zai_task.await.unwrap();
        assert!(router.zai_fallback().is_some());
        router
            .set_provider(hive_common::AiProvider::Zai, Some("new-key".into()))
            .unwrap();
        assert!(router.zai_fallback().is_none());
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
