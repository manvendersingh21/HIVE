//! Delegation planning failures against a mocked model provider: a local HTTP
//! server speaking Ollama's chat API that serves scripted answers in order.
use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Prompts = Arc<Mutex<Vec<String>>>;

/// Serves `answers` in order, each after its delay. A client that gives up
/// during the delay (its deadline fired) skips to the next answer.
async fn mocked_provider(answers: Vec<(Duration, String)>) -> (String, Prompts, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let prompts: Prompts = Default::default();
    let seen = prompts.clone();
    let task = tokio::spawn(async move {
        for (delay, answer) in answers {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 8192];
            let body_start = loop {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "request ended early");
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&data[..body_start]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                .unwrap();
            while data.len() < body_start + length {
                let n = stream.read(&mut buf).await.unwrap();
                data.extend_from_slice(&buf[..n]);
            }
            let request: Value = serde_json::from_slice(&data[body_start..body_start + length]).unwrap();
            seen.lock().unwrap().push(request["messages"][0]["content"].as_str().unwrap_or("").to_string());
            if !delay.is_zero() {
                let mut eof = [0u8; 1];
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = stream.read(&mut eof) => continue,
                }
            }
            let body = json!({"message":{"role":"assistant","content":answer},"done":true}).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (url, prompts, task)
}

fn handle_for(url: String) -> AgentHandle {
    let agent = hive_core::agent::MasterAgent::new(
        hive_core::llm::LlmRouter::new(url, "test".into()),
        hive_core::workers::WorkerPool::new(vec![hive_common::protocol::WorkerInfo {
            name: "air".into(),
            host: "ssh-alias".into(),
            user: "test".into(),
            port: None,
            tags: vec![],
            local: false,
            container: None,
        }]),
        hive_core::skills::SkillRegistry::new(),
        hive_core::memory::MemorySystem::new(),
    );
    hive_core::memory::machines::project_into_graph(
        &agent.memory.graph,
        &hive_core::memory::machines::MachineFacts { name: "air".into(), reachable: true, ..Default::default() },
    )
    .unwrap();
    AgentHandle::enabled(std::sync::Arc::new(agent), "master".into()).unwrap()
}

fn begin(h: &AgentHandle) -> SavedTurn {
    let history = h.history.as_ref().unwrap();
    let chat = history.create(None).unwrap();
    match history.begin(&chat.id, &uuid::Uuid::new_v4().to_string(), "implement it").unwrap() {
        hive_core::memory::chats::StartTurn::New(turn) => turn,
        _ => unreachable!(),
    }
}

fn valid_plan() -> String {
    json!({"summary":"implement it","containers":[],"assignments":[{
        "key":"implement","device":"air","agent":"claude","model":null,
        "workspace":"~/hive-workspaces/implement","objective":"implement it",
        "dependencies":[],"peer_dependencies":[],"acceptance_criteria":["verified"],
        "acceptance_checks":[{"kind":"file_exists","path":"done.txt"}],
        "max_rework":2,"owned_paths":["src/**"],"required_capabilities":[]}]})
    .to_string()
}

/// The valid plan with the comma before `"containers"` lost.
fn broken_plan() -> String {
    let broken = valid_plan().replacen(",\"containers\"", " \"containers\"", 1);
    assert!(serde_json::from_str::<Value>(&broken).is_err());
    broken
}

fn missing_device_plan() -> String {
    let mut plan: Value = serde_json::from_str(&valid_plan()).unwrap();
    plan["assignments"][0].as_object_mut().unwrap().remove("device");
    plan.to_string()
}

async fn plan_turn(h: &AgentHandle, turn: SavedTurn, deadline: Duration) -> (StatusCode, String) {
    let agent = h.agent.clone().unwrap();
    let planner = move |context: String, feedback: delegation::PlanFeedback| {
        let agent = agent.clone();
        async move {
            delegation::plan_from_prompt(&agent, "implement it", format!("PLAN\n{context}"), None, &feedback).await
        }
    };
    let slots = tokio::sync::Semaphore::new(2);
    let response = process_with_planner(h.clone(), turn, deadline, &slots, planner).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn an_invalid_then_valid_plan_starts_its_runs() {
    let (url, prompts, task) =
        mocked_provider(vec![(Duration::ZERO, broken_plan()), (Duration::ZERO, valid_plan())]).await;
    let h = handle_for(url);
    let turn = begin(&h);
    let (status, body) = plan_turn(&h, turn.clone(), Duration::from_secs(30)).await;
    task.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(store(&h).unwrap().list().unwrap().0.len(), 1);
    assert_eq!(h.history.as_ref().unwrap().turn(&turn.id).unwrap().unwrap().status, "completed");
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("Your previous answer was invalid JSON at line 1 column"), "{}", prompts[1]);
    assert!(prompts[1].contains("expected `,` or `}`"), "{}", prompts[1]);
}

#[tokio::test]
async fn an_invalid_plan_twice_reports_invalid_plan_not_a_timeout() {
    let (url, prompts, task) =
        mocked_provider(vec![(Duration::ZERO, broken_plan()), (Duration::ZERO, missing_device_plan())]).await;
    let h = handle_for(url);
    let turn = begin(&h);
    let (status, body) = plan_turn(&h, turn.clone(), Duration::from_secs(30)).await;
    task.await.unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.starts_with("Invalid plan: does not match the plan schema: missing field `device` at line 1 column"), "{body}");
    assert!(body.contains("No commands were executed"), "{body}");
    assert!(!body.contains("timed out"), "{body}");
    assert!(prompts.lock().unwrap()[1].contains("expected `,` or `}`"));
    assert!(store(&h).unwrap().list().unwrap().0.is_empty());
    let saved = h.history.as_ref().unwrap().turn(&turn.id).unwrap().unwrap();
    assert_eq!(saved.status, "failed");
    let messages = h.history.as_ref().unwrap().messages(&turn.conversation_id).unwrap();
    assert!(messages.last().unwrap().content.starts_with("Invalid plan:"), "{:?}", messages.last());
}

#[tokio::test]
async fn a_deadline_after_an_invalid_plan_reports_the_plan_error() {
    // The corrected answers arrive only after the deadline, both times.
    let slow = Duration::from_secs(10);
    let (url, prompts, task) = mocked_provider(vec![
        (Duration::ZERO, broken_plan()),
        (slow, valid_plan()),
        (slow, valid_plan()),
    ])
    .await;
    let h = handle_for(url);
    let turn = begin(&h);
    let (status, body) = plan_turn(&h, turn, Duration::from_millis(500)).await;
    task.await.unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.starts_with("Invalid plan: invalid JSON: expected `,` or `}` at line 1 column"), "{body}");
    assert!(body.contains("within the planning deadline"), "{body}");
    assert!(!body.contains("timed out"), "{body}");
    // The whole-plan retry re-prompts with the error instead of resending blindly.
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].contains("Your previous answer was invalid JSON"), "{}", prompts[2]);
}

#[test]
fn the_planning_deadline_comes_from_hive_toml() {
    let dir = std::env::temp_dir().join(format!("hive-deadline-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let shipped = include_str!("../../config/hive.toml");
    assert!(shipped.contains("planning_deadline_secs = 240"));
    std::fs::write(dir.join("config/hive.toml"), shipped.replace("planning_deadline_secs = 240", "planning_deadline_secs = 90")).unwrap();
    let config = hive_common::config::HiveConfig::from_project_root(&dir).unwrap();
    assert_eq!(planning_deadline(), Duration::from_secs(240));
    configure(&config.delegation);
    assert_eq!(planning_deadline(), Duration::from_secs(90));
    configure(&hive_common::config::DelegationConfig { planning_deadline_secs: 0 });
    assert_eq!(planning_deadline(), Duration::from_secs(90), "zero is ignored");
    configure(&hive_common::config::DelegationConfig::default());
    assert_eq!(planning_deadline(), Duration::from_secs(240));
    std::fs::remove_dir_all(dir).unwrap();
}
