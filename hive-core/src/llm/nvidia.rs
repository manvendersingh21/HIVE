//! NVIDIA's hosted chat and asymmetric embedding APIs. No provider fallback.
use hive_common::config::NvidiaConfig;
use serde_json::{json, Value};
use std::time::Duration;

pub struct NvidiaClient {
    http: reqwest::Client,
    key: Option<String>,
    key_env: &'static str,
    pub model: String,
    base_url: String,
    deadline: Duration,
}

impl NvidiaClient {
    pub fn for_reasoning(config: &NvidiaConfig) -> Self {
        Self::from_env(config, "NVIDIA_API_KEY_FLASH")
    }

    pub fn for_embeddings(config: &NvidiaConfig) -> Self {
        Self::from_env(config, "NVIDIA_API_KEY_EMBEDDING")
    }

    fn from_env(config: &NvidiaConfig, key_env: &'static str) -> Self {
        Self {
            http: reqwest::Client::new(),
            key_env,
            key: std::env::var(key_env).ok().filter(|k| !k.trim().is_empty()),
            model: config.model.clone(),
            base_url: config.base_url.trim_end_matches('/').into(),
            deadline: Duration::from_secs(config.timeout_secs.max(1)),
        }
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("NVIDIA is not configured: set {}", self.key_env))?;
        tokio::time::timeout(self.deadline, async {
            for attempt in 0..3 {
                let response = self
                    .http
                    .post(format!("{}/{path}", self.base_url))
                    .bearer_auth(key)
                    .json(&body)
                    .timeout(self.deadline)
                    .send()
                    .await;
                match response {
                    Ok(r) => {
                        let status = r.status();
                        if status == reqwest::StatusCode::OK {
                            return Ok(r.json().await?);
                        }
                        let retry = status.as_u16() == 429
                            || status.as_u16() == 408
                            || status.is_server_error();
                        if !retry || attempt == 2 {
                            // Include structured diagnostics only, bounded and with the key redacted.
                            let reason = match status.as_u16() {
                                401 => "authentication failed",
                                403 | 404 => {
                                    "model unavailable or access denied; check NVIDIA model access"
                                }
                                429 => "rate limit exceeded",
                                _ => "request failed",
                            };
                            let error_body: Value = r.json().await.unwrap_or_default();
                            let detail = error_body
                                .pointer("/error/message")
                                .or_else(|| error_body.get("detail"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .replace(key, "[redacted]");
                            let detail: String = detail.chars().take(500).collect();
                            anyhow::bail!(
                                "NVIDIA {reason} (HTTP {status}, model {}, credential {}): {detail}",
                                body["model"], self.key_env
                            );
                        }
                    }
                    Err(e) => {
                        if attempt == 2 || !(e.is_timeout() || e.is_connect() || e.is_body()) {
                            return Err(anyhow::anyhow!(
                                "NVIDIA transport failure: {}",
                                e.without_url()
                            ));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
            }
            unreachable!()
        })
        .await
        .map_err(|_| anyhow::anyhow!("NVIDIA request deadline exceeded"))?
    }

    fn chat_template_kwargs(&self, effort: &str) -> Value {
        if self.model.starts_with("nvidia/nemotron-") {
            // Auxiliary calls need a short answer, not a full reasoning trace.
            json!({"enable_thinking": effort != "low"})
        } else {
            // Preserve the DeepSeek template for explicitly configured older deployments.
            json!({"thinking": true, "reasoning_effort": effort})
        }
    }

    pub async fn complete(&self, prompt: &str, effort: &str) -> anyhow::Result<super::LlmResponse> {
        let value = self
            .post(
                "chat/completions",
                json!({
                    "model": self.model,
                    "messages": [{"role":"user", "content":prompt}],
                    "temperature":1, "top_p":0.95, "max_tokens":16384, "stream":false,
                    "chat_template_kwargs":self.chat_template_kwargs(effort)
                }),
            )
            .await?;
        let choice = &value["choices"][0];
        anyhow::ensure!(
            choice["finish_reason"] == "stop",
            "NVIDIA response incomplete or truncated (finish_reason: {})",
            choice["finish_reason"]
        );
        let text = choice["message"]["content"].as_str().unwrap_or("").trim();
        anyhow::ensure!(!text.is_empty(), "NVIDIA returned an empty final answer");
        Ok(super::LlmResponse {
            text: text.into(),
            provider: hive_common::AiProvider::Nvidia,
            model: value["model"].as_str().unwrap_or(&self.model).into(),
        })
    }

    pub async fn embed(&self, input: &str, input_type: &str) -> anyhow::Result<Vec<f32>> {
        let value = self
            .post(
                "embeddings",
                json!({"model":self.model,"input":[input],
            "input_type":input_type,"encoding_format":"float","truncate":"NONE"}),
            )
            .await?;
        let vec: Vec<f32> = serde_json::from_value(value["data"][0]["embedding"].clone())?;
        anyhow::ensure!(
            !vec.is_empty() && vec.iter().all(|x| x.is_finite()),
            "NVIDIA returned an invalid embedding"
        );
        Ok(vec)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    pub type Requests = Arc<Mutex<Vec<(String, Value)>>>;

    pub async fn server(
        replies: Vec<(u16, Value, Duration)>,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests: Requests = Default::default();
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            for (status, body, delay) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = vec![];
                let header_end = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                    if let Some(pos) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&data[..header_end]).to_string();
                let len: usize = headers
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                while data.len() < header_end + len {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                captured.lock().unwrap().push((
                    headers,
                    serde_json::from_slice(&data[header_end..header_end + len]).unwrap(),
                ));
                if !delay.is_zero() {
                    let mut eof = [0u8; 1];
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {},
                        _ = stream.read(&mut eof) => return,
                    }
                }
                let body = body.to_string();
                let response = format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (url, requests, task)
    }

    pub fn answer(text: &str) -> Value {
        json!({"model":"actual-nemotron", "choices":[{"finish_reason":"stop","message":{"content":text,"reasoning_content":"NEVER EXECUTE THIS REASONING"}}]})
    }
    pub fn router(url: String) -> crate::llm::LlmRouter {
        let mut config: hive_common::HiveConfig =
            toml::from_str(include_str!("../../../config/hive.toml")).unwrap();
        config.llm.single_provider = Some(hive_common::AiProvider::Nvidia);
        config.llm.nvidia.base_url = url;
        config.llm.local.base_url = "http://127.0.0.1:1".into();
        config.llm.gemini = None;
        config.llm.claude = None;
        config.llm.codex = None;
        let mut router = crate::llm::LlmRouter::from_config(&config.llm);
        router.nvidia.key = Some("test-secret".into());
        router
    }

    #[test]
    fn explicit_deepseek_configuration_keeps_its_template() {
        let client = NvidiaClient::for_reasoning(&NvidiaConfig {
            model: "deepseek-ai/deepseek-v4-flash-0731".into(),
            ..Default::default()
        });
        for effort in ["low", "high"] {
            assert_eq!(
                client.chat_template_kwargs(effort),
                json!({"thinking":true,"reasoning_effort":effort})
            );
        }
    }

    #[tokio::test]
    async fn routes_all_master_calls_and_separates_reasoning() {
        let (url, captured, task) = server(vec![
            (200, answer("SIMPLE"), Duration::ZERO),
            (200, answer("skill"), Duration::ZERO),
            (200, answer("final plan"), Duration::ZERO),
        ])
        .await;
        let router = router(url);
        assert!(!router.uses_local_startup());
        assert_eq!(
            router.classify_complexity("echo hi").await.unwrap(),
            hive_common::Complexity::Simple
        );
        assert_eq!(router.local_complete("pick skill").await.unwrap(), "skill");
        let result = router
            .complete_with("plan", hive_common::AiProvider::Claude)
            .await
            .unwrap();
        assert_eq!(result.provider, hive_common::AiProvider::Nvidia);
        assert_eq!(result.text, "final plan");
        assert_eq!(result.model, "actual-nemotron");
        task.await.unwrap();
        let calls = captured.lock().unwrap();
        for (index, (headers, body)) in calls.iter().enumerate() {
            assert!(headers.contains("Bearer test-secret"));
            assert!(headers.starts_with("POST /v1/chat/completions"));
            assert_eq!(body["temperature"], 1);
            assert_eq!(body["top_p"], 0.95);
            assert_eq!(body["max_tokens"], 16384);
            assert_eq!(body["stream"], false);
            assert_eq!(body["model"], "nvidia/nemotron-3-ultra-550b-a55b");
            assert_eq!(
                body["chat_template_kwargs"],
                json!({"enable_thinking":index == 2})
            );
            assert!(body.get("extra_body").is_none());
        }
    }

    #[tokio::test]
    async fn failures_are_bounded_and_never_fall_back() {
        for (status, count, expected) in [
            (401, 1, "authentication"),
            (403, 1, "access"),
            (404, 1, "model"),
            (429, 3, "rate limit"),
            (503, 3, "request failed"),
        ] {
            let (url, requests, task) =
                server(vec![(status, json!({}), Duration::ZERO); count]).await;
            let error = router(url)
                .complete_with("plan", hive_common::AiProvider::Local)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            task.await.unwrap();
            assert_eq!(requests.lock().unwrap().len(), count);
        }
        let (url, requests, task) = server(vec![
            (429, json!({}), Duration::ZERO),
            (200, answer("ok"), Duration::ZERO),
        ])
        .await;
        assert_eq!(router(url).local_complete("x").await.unwrap(), "ok");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejects_missing_empty_malformed_and_truncated_answers_and_deadlines() {
        for body in [
            json!({}),
            answer("  "),
            json!({"choices":[{"finish_reason":"length","message":{"content":"{}"}}]}),
        ] {
            let (url, _, task) = server(vec![(200, body, Duration::ZERO)]).await;
            assert!(router(url).local_complete("x").await.is_err());
            task.await.unwrap();
        }
        let (url, _, task) = server(vec![(200, answer("late"), Duration::from_secs(2))]).await;
        let mut r = router(url);
        r.nvidia.deadline = Duration::from_millis(30);
        let start = std::time::Instant::now();
        assert!(r
            .local_complete("x")
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline"));
        assert!(start.elapsed() < Duration::from_millis(500));
        task.abort();
        r.nvidia.key = None;
        assert!(r
            .local_complete("x")
            .await
            .unwrap_err()
            .to_string()
            .contains("NVIDIA_API_KEY_FLASH"));
    }

    #[tokio::test]
    async fn upstream_diagnostics_redact_credentials() {
        let (url, _, task) = server(vec![(
            400,
            json!({"error":{"message":"invalid parameter for test-secret"}}),
            Duration::ZERO,
        )])
        .await;
        let error = router(url)
            .local_complete("x")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid parameter"));
        assert!(!error.contains("test-secret"));
        assert!(error.contains("[redacted]"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn credential_errors_identify_the_correct_environment_variable() {
        for embedding in [false, true] {
            let (url, _, task) = server(vec![(401, json!({}), Duration::ZERO)]).await;
            let config = NvidiaConfig {
                base_url: url,
                ..Default::default()
            };
            let mut client = if embedding {
                NvidiaClient::for_embeddings(&config)
            } else {
                NvidiaClient::for_reasoning(&config)
            };
            let expected = if embedding {
                "NVIDIA_API_KEY_EMBEDDING"
            } else {
                "NVIDIA_API_KEY_FLASH"
            };
            client.key = None;
            let error = client
                .post("unused", json!({}))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            client.key = Some("wrong-key".into());
            let error = client
                .post("unused", json!({}))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("authentication failed") && error.contains(expected),
                "{error}"
            );
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn embeddings_use_query_and_passage_modes() {
        use crate::memory::rag::Embedder;
        let (url, requests, task) = server(vec![
            (
                200,
                json!({"data":[{"embedding":[1.0,2.0]}]}),
                Duration::ZERO
            );
            2
        ])
        .await;
        let mut client = NvidiaClient::for_embeddings(&NvidiaConfig {
            base_url: url,
            model: "nvidia/nemotron-3-embed-1b".into(),
            ..Default::default()
        });
        client.key = Some("test-embedding-secret".into());
        assert_eq!(
            Embedder::embed(&client, "index").await.unwrap(),
            vec![1.0, 2.0]
        );
        client.embed_query("search").await.unwrap();
        task.await.unwrap();
        let requests = requests.lock().unwrap();
        for (i, (headers, body)) in requests.iter().enumerate() {
            assert!(headers.starts_with("POST /v1/embeddings"));
            assert!(headers.contains("Bearer test-embedding-secret"));
            assert_eq!(body["input_type"], if i == 0 { "passage" } else { "query" });
        }
    }

    #[tokio::test]
    async fn plans_fail_closed_before_execution() {
        use crate::{
            agent::MasterAgent, memory::MemorySystem, skills::SkillRegistry, workers::WorkerPool,
        };
        for plan in [
            "garbage",
            r#"{"summary":"", "subtasks":[]}"#,
            r#"{"summary":"ok", "subtasks":[{"description":"empty command","commands":["  "],"requires_remote":false}]}"#,
        ] {
            let (url, _, task) = server(vec![
                (200, answer("SIMPLE"), Duration::ZERO),
                (200, answer(plan), Duration::ZERO),
            ])
            .await;
            let agent = MasterAgent::new(
                router(url),
                WorkerPool::new(vec![]),
                SkillRegistry::new(),
                MemorySystem::new(),
            );
            assert!(agent.plan_run("echo hi", None).await.is_err());
            task.await.unwrap();
        }
    }
}
