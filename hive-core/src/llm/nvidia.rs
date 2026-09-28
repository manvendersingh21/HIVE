//! NVIDIA's hosted chat and asymmetric embedding APIs. No provider fallback.
use hive_common::config::NvidiaConfig;
use serde_json::{json, Value};
use std::time::Duration;

/// How many attempts a single POST gets inside the overall deadline.
const ATTEMPTS: u32 = 2;

pub struct NvidiaClient {
    http: reqwest::Client,
    key: Option<String>,
    key_env: &'static str,
    pub model: String,
    base_url: String,
    pub deadline: Duration,
    pub stream: bool,
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
            stream: config.stream,
        }
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("NVIDIA is not configured: set {}", self.key_env))?;
        let start = std::time::Instant::now();
        tokio::time::timeout(self.deadline, async {
            for attempt in 0..ATTEMPTS {
                // Without streaming, a hung connection and a healthy but slow
                // generation look the same, so the first attempt gets most of
                // the budget (two thirds / ~80s of 120s): a long generation still
                // completes on the first try, while a hung one is cut off with the
                // remainder left for its retry. Retries after a fast failure
                // (connect error or 5xx) get whatever budget is left. The overall
                // deadline around this loop stays the hard cap.
                let attempt_timeout = if attempt == 0 {
                    self.deadline * 2 / 3
                } else {
                    self.deadline.saturating_sub(start.elapsed())
                }
                .max(Duration::from_millis(1));

                let response = self
                    .http
                    .post(format!("{}/{path}", self.base_url))
                    .bearer_auth(key)
                    .json(&body)
                    .timeout(attempt_timeout)
                    .send()
                    .await;
                match response {
                    Ok(r) => {
                        let status = r.status();
                        if status == reqwest::StatusCode::OK {
                            return Ok(r.json().await?);
                        }
                        let retry = status.is_server_error();
                        if !retry || attempt + 1 >= ATTEMPTS {
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
                        let retry = e.is_connect() || e.is_timeout();
                        if !retry || attempt + 1 >= ATTEMPTS {
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
        if self.stream {
            return self.complete_streaming(prompt, effort).await;
        }
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

    /// Streaming (SSE) alternative for completion calls with per-chunk idle timeout
    /// and overall deadline as hard cap.
    pub async fn complete_streaming(
        &self,
        prompt: &str,
        effort: &str,
    ) -> anyhow::Result<super::LlmResponse> {
        self.complete_streaming_with_idle(prompt, effort, Duration::from_secs(15))
            .await
    }

    /// Streaming (SSE) alternative with caller-configurable idle timeout.
    pub async fn complete_streaming_with_idle(
        &self,
        prompt: &str,
        effort: &str,
        idle_timeout: Duration,
    ) -> anyhow::Result<super::LlmResponse> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("NVIDIA is not configured: set {}", self.key_env))?;
        let start = std::time::Instant::now();
        let body = json!({
            "model": self.model,
            "messages": [{"role":"user", "content":prompt}],
            "temperature": 1,
            "top_p": 0.95,
            "max_tokens": 16384,
            "stream": true,
            "chat_template_kwargs": self.chat_template_kwargs(effort)
        });

        tokio::time::timeout(self.deadline, async {
            for attempt in 0..ATTEMPTS {
                let attempt_timeout = if attempt == 0 {
                    self.deadline * 2 / 3
                } else {
                    self.deadline.saturating_sub(start.elapsed())
                }
                .max(Duration::from_millis(1));

                // Limit only time-to-headers; body progress is governed by
                // idle_timeout and the overall deadline.
                let response = match tokio::time::timeout(
                    attempt_timeout,
                    self.http
                        .post(format!("{}/chat/completions", self.base_url))
                        .bearer_auth(key)
                        .json(&body)
                        .send(),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) if attempt + 1 < ATTEMPTS => {
                        tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                        continue;
                    }
                    Err(_) => anyhow::bail!("NVIDIA streaming response headers timed out"),
                };

                let mut r = match response {
                    Ok(r) => {
                        let status = r.status();
                        if status == reqwest::StatusCode::OK {
                            r
                        } else {
                            let retry = status.is_server_error();
                            if !retry || attempt + 1 >= ATTEMPTS {
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
                            tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                            continue;
                        }
                    }
                    Err(e) => {
                        let retry = e.is_connect() || e.is_timeout();
                        if !retry || attempt + 1 >= ATTEMPTS {
                            return Err(anyhow::anyhow!(
                                "NVIDIA transport failure: {}",
                                e.without_url()
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                        continue;
                    }
                };

                let mut full_text = String::new();
                let mut model_name = self.model.clone();
                let mut line_buffer: Vec<u8> = Vec::new();
                let mut finish_reason = None;
                let mut stream_retry = false;
                let mut received = false;

                'stream: loop {
                    let chunk_result = tokio::time::timeout(idle_timeout, r.chunk()).await;
                    match chunk_result {
                        Ok(Ok(Some(bytes))) => {
                            received = true;
                            line_buffer.extend_from_slice(&bytes);
                            anyhow::ensure!(
                                line_buffer.len() <= 1 << 20,
                                "NVIDIA streaming line exceeds 1 MiB"
                            );
                            while let Some(idx) = line_buffer.iter().position(|&b| b == b'\n') {
                                let raw: Vec<u8> = line_buffer.drain(..=idx).collect();
                                let line = String::from_utf8_lossy(&raw).trim().to_string();
                                if let Some(data) = line.strip_prefix("data: ") {
                                    let data = data.trim();
                                    if data == "[DONE]" {
                                        break 'stream;
                                    }
                                    if let Ok(val) = serde_json::from_str::<Value>(data) {
                                        if let Some(m) = val["model"].as_str() {
                                            model_name = m.to_string();
                                        }
                                        if let Some(choice) = val["choices"].get(0) {
                                            if let Some(c) = choice
                                                .pointer("/delta/content")
                                                .and_then(Value::as_str)
                                            {
                                                full_text.push_str(c);
                                            }
                                            if let Some(fr) =
                                                choice.get("finish_reason").and_then(Value::as_str)
                                            {
                                                finish_reason = Some(fr.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Ok(None)) => {
                            break 'stream;
                        }
                        Ok(Err(e)) => {
                            // Body errors are never connect/timeout errors on this
                            // client (it has no reqwest timeout). A failure before
                            // the first chunk consumed nothing, so it is replayable.
                            let retry = !received || e.is_connect() || e.is_timeout();
                            if retry && attempt + 1 < ATTEMPTS {
                                stream_retry = true;
                                break 'stream;
                            }
                            return Err(anyhow::anyhow!(
                                "NVIDIA streaming transport error: {}",
                                e.without_url()
                            ));
                        }
                        Err(_) => {
                            if attempt + 1 < ATTEMPTS {
                                stream_retry = true;
                                break 'stream;
                            }
                            anyhow::bail!("NVIDIA streaming idle timeout exceeded");
                        }
                    }
                }

                if stream_retry {
                    tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                    continue;
                }

                let remaining = String::from_utf8_lossy(&line_buffer).trim().to_string();
                if let Some(data) = remaining.strip_prefix("data: ") {
                    let data = data.trim();
                    if data != "[DONE]" {
                        if let Ok(val) = serde_json::from_str::<Value>(data) {
                            if let Some(m) = val["model"].as_str() {
                                model_name = m.to_string();
                            }
                            if let Some(choice) = val["choices"].get(0) {
                                if let Some(c) =
                                    choice.pointer("/delta/content").and_then(Value::as_str)
                                {
                                    full_text.push_str(c);
                                }
                                if let Some(fr) =
                                    choice.get("finish_reason").and_then(Value::as_str)
                                {
                                    finish_reason = Some(fr.to_string());
                                }
                            }
                        }
                    }
                }

                anyhow::ensure!(
                    finish_reason.as_deref() == Some("stop"),
                    "NVIDIA response incomplete or truncated (finish_reason: {})",
                    finish_reason.as_deref().unwrap_or("none")
                );
                let text = full_text.trim();
                anyhow::ensure!(!text.is_empty(), "NVIDIA returned an empty final answer");
                return Ok(super::LlmResponse {
                    text: text.into(),
                    provider: hive_common::AiProvider::Nvidia,
                    model: model_name,
                });
            }
            unreachable!()
        })
        .await
        .map_err(|_| anyhow::anyhow!("NVIDIA request deadline exceeded"))?
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
                        // The client gave up on this attempt (e.g. its
                        // per-attempt timeout fired): skip this reply and
                        // let the next scripted reply serve the retry.
                        _ = stream.read(&mut eof) => continue,
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
            (429, 1, "rate limit"),
            (503, 2, "request failed"),
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
            (503, json!({}), Duration::ZERO),
            (200, answer("ok"), Duration::ZERO),
        ])
        .await;
        assert_eq!(router(url).local_complete("x").await.unwrap(), "ok");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_hung_first_attempt_is_cut_off_and_the_retry_succeeds() {
        // The first reply hangs past the first attempt's share of the budget
        // (two thirds of the overall deadline) but inside the overall
        // deadline itself, so the retry still has the remainder available.
        let (url, requests, task) = server(vec![
            (200, answer("late"), Duration::from_millis(1100)),
            (200, answer("retried"), Duration::ZERO),
        ])
        .await;
        let mut r = router(url);
        r.nvidia.deadline = Duration::from_millis(1500);
        let start = std::time::Instant::now();
        assert_eq!(r.local_complete("x").await.unwrap(), "retried");
        // Success must come from the retry, before the overall deadline and
        // without waiting for the hung first attempt to finish.
        assert!(start.elapsed() < Duration::from_millis(1500));
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_slow_first_attempt_completes_without_a_retry() {
        // A generation slower than an even share of the deadline, but inside
        // the first attempt's two-thirds share, must succeed on the first
        // try: the budget split must not cut off healthy slow calls.
        let (url, requests, task) = server(vec![(200, answer("slow but fine"), Duration::from_millis(800))])
            .await;
        let mut r = router(url);
        r.nvidia.deadline = Duration::from_millis(1500);
        assert_eq!(r.local_complete("x").await.unwrap(), "slow but fine");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    pub async fn streaming_server_bytes(
        replies: Vec<(u16, Vec<(Vec<u8>, Duration)>)>,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests: Requests = Default::default();
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            for (status, chunks) in replies {
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
                    .unwrap_or(0);
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
                    serde_json::from_slice(&data[header_end..header_end + len]).unwrap_or_default(),
                ));

                if status != 200 {
                    let err_body = "{\"error\":{\"message\":\"server error\"}}";
                    let resp = format!(
                        "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err_body}",
                        err_body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    continue;
                }

                let resp_head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(resp_head.as_bytes()).await;
                for (chunk, delay) in chunks {
                    if !delay.is_zero() {
                        let mut eof = [0u8; 1];
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {},
                            _ = stream.read(&mut eof) => break,
                        }
                    }
                    if stream.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            }
        });
        (url, requests, task)
    }

    pub async fn streaming_server(
        replies: Vec<(u16, Vec<(&'static str, Duration)>)>,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        streaming_server_bytes(
            replies
                .into_iter()
                .map(|(status, chunks)| {
                    (
                        status,
                        chunks
                            .into_iter()
                            .map(|(s, d)| (s.as_bytes().to_vec(), d))
                            .collect(),
                    )
                })
                .collect(),
        )
        .await
    }

    #[tokio::test]
    async fn streaming_planner_call_succeeds() {
        let (url, requests, task) = streaming_server(vec![(
            200,
            vec![
                ("data: {\"model\":\"nemotron-stream\",\"choices\":[{\"delta\":{\"content\":\"plan \"},\"finish_reason\":null}]}\n\n", Duration::from_millis(10)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"step 1\"},\"finish_reason\":\"stop\"}]}\n\n", Duration::from_millis(10)),
                ("data: [DONE]\n\n", Duration::ZERO),
            ],
        )])
        .await;
        let mut r = router(url);
        r.nvidia.stream = true;
        let resp = r.nvidia.complete("make plan", "high").await.unwrap();
        assert_eq!(resp.text, "plan step 1");
        assert_eq!(resp.model, "nemotron-stream");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn streaming_idle_timeout_triggers_retry_and_succeeds() {
        let (url, requests, task) = streaming_server(vec![
            (
                200,
                vec![
                    ("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n", Duration::ZERO),
                    // Hangs for 300ms, exceeding the 50ms idle timeout!
                    ("data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n", Duration::from_millis(300)),
                ],
            ),
            (
                200,
                vec![
                    ("data: {\"choices\":[{\"delta\":{\"content\":\"retried ok\"},\"finish_reason\":\"stop\"}]}\n\n", Duration::ZERO),
                    ("data: [DONE]\n\n", Duration::ZERO),
                ],
            ),
        ])
        .await;
        let r = router(url);
        let resp = r
            .nvidia
            .complete_streaming_with_idle("plan", "high", Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(resp.text, "retried ok");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    /// Answers each connection with raw bytes and then closes it, so a reply
    /// can promise more body than it sends.
    async fn raw_server(replies: Vec<Vec<u8>>) -> (String, Requests, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests: Requests = Default::default();
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = vec![];
                let header_end = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
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
                    .unwrap_or(0);
                while data.len() < header_end + len {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    data.extend_from_slice(&buf[..n]);
                }
                captured.lock().unwrap().push((headers, Value::Null));
                let _ = stream.write_all(&reply).await;
            }
        });
        (url, requests, task)
    }

    fn truncated_stream(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{body}").into_bytes()
    }

    #[tokio::test]
    async fn streaming_error_before_first_chunk_is_retried() {
        let ok = "data: {\"choices\":[{\"delta\":{\"content\":\"retried ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let (url, requests, task) = raw_server(vec![
            truncated_stream(""),
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok}", ok.len()).into_bytes(),
        ])
        .await;
        let resp = router(url)
            .nvidia
            .complete_streaming_with_idle("plan", "high", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(resp.text, "retried ok");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn streaming_error_after_first_chunk_is_not_retried() {
        let (url, requests, task) = raw_server(vec![truncated_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        )])
        .await;
        let err = router(url)
            .nvidia
            .complete_streaming_with_idle("plan", "high", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("streaming transport error"), "{err}");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn streaming_overall_deadline_is_hard_cap() {
        let (url, _, task) = streaming_server(vec![(
            200,
            vec![
                ("data: {\"choices\":[{\"delta\":{\"content\":\"slow\"}}]}\n\n", Duration::from_millis(10)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"...\"}}]}\n\n", Duration::from_millis(200)),
            ],
        )])
        .await;
        let mut r = router(url);
        r.nvidia.deadline = Duration::from_millis(50);
        let err = r
            .nvidia
            .complete_streaming_with_idle("plan", "high", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
        task.abort();
    }

    #[tokio::test]
    async fn streaming_split_utf8_across_chunks_is_preserved() {
        // "✓" is 3 bytes in UTF-8: 0xE2, 0x9C, 0x93
        let part1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"valid \xe2".to_vec();
        let part2 = b"\x9c\x93 plan\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec();
        let (url, _, task) = streaming_server_bytes(vec![(
            200,
            vec![
                (part1, Duration::ZERO),
                (part2, Duration::ZERO),
            ],
        )])
        .await;
        let mut r = router(url);
        r.nvidia.stream = true;
        let resp = r.nvidia.complete("make plan", "high").await.unwrap();
        assert_eq!(resp.text, "valid ✓ plan");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn streaming_finish_reason_validation() {
        // Case 1: finish_reason is "length"
        let (url, _, task) = streaming_server(vec![(
            200,
            vec![
                ("data: {\"choices\":[{\"delta\":{\"content\":\"truncated plan\"},\"finish_reason\":\"length\"}]}\n\n", Duration::ZERO),
                ("data: [DONE]\n\n", Duration::ZERO),
            ],
        )])
        .await;
        let mut r = router(url);
        r.nvidia.stream = true;
        let err = r.nvidia.complete("make plan", "high").await.unwrap_err().to_string();
        assert!(err.contains("finish_reason: length"), "{err}");
        task.await.unwrap();

        // Case 2: stream ends without finish_reason before [DONE] or EOF
        let (url, _, task) = streaming_server(vec![(
            200,
            vec![
                ("data: {\"choices\":[{\"delta\":{\"content\":\"incomplete plan\"}}]}\n\n", Duration::ZERO),
                ("data: [DONE]\n\n", Duration::ZERO),
            ],
        )])
        .await;
        let mut r = router(url);
        r.nvidia.stream = true;
        let err = r.nvidia.complete("make plan", "high").await.unwrap_err().to_string();
        assert!(err.contains("finish_reason: none"), "{err}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn streaming_slow_steady_stream_exceeding_first_attempt_share_succeeds() {
        // Total deadline = 1500 ms. First attempt share in non-streaming would be ~1000 ms.
        // 6 chunks arriving every 200 ms = ~1200 ms total, well within 1500 ms deadline.
        // In streaming mode, body read must NOT time out on the 1000 ms header timeout.
        let (url, requests, task) = streaming_server(vec![(
            200,
            vec![
                ("data: {\"choices\":[{\"delta\":{\"content\":\"chunk 1 \"}}]}\n\n", Duration::from_millis(200)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"chunk 2 \"}}]}\n\n", Duration::from_millis(200)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"chunk 3 \"}}]}\n\n", Duration::from_millis(200)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"chunk 4 \"}}]}\n\n", Duration::from_millis(200)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"chunk 5 \"}}]}\n\n", Duration::from_millis(200)),
                ("data: {\"choices\":[{\"delta\":{\"content\":\"chunk 6\"},\"finish_reason\":\"stop\"}]}\n\n", Duration::from_millis(200)),
                ("data: [DONE]\n\n", Duration::ZERO),
            ],
        )])
        .await;
        let mut r = router(url);
        r.nvidia.deadline = Duration::from_millis(1500);
        let resp = r
            .nvidia
            .complete_streaming_with_idle("plan", "high", Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(resp.text, "chunk 1 chunk 2 chunk 3 chunk 4 chunk 5 chunk 6");
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
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
