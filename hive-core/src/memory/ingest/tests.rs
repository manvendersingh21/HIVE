use super::*;
use crate::memory::{chats::ChatStore, MemorySystem};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};

#[derive(Default)]
struct FixtureModel {
    calls: AtomicUsize,
    fail: AtomicBool,
    seen: Mutex<Vec<String>>,
    called: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl Completer for FixtureModel {
    async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(prompt.into());
        self.called.notify_one();
        anyhow::ensure!(
            !self.fail.load(Ordering::SeqCst),
            "fixture extraction unavailable"
        );
        Ok(r#"{"entities":[{"name":"SQLite","kind":"tool","description":"Durable local memory"},{"name":"Ingestion","kind":"concept","description":"A nightly batch"}],"relations":[{"from":"Ingestion","relation":"uses","to":"SQLite"}]}"#.into())
    }
}

fn fixture(memory: &mut MemorySystem, model: Arc<FixtureModel>) {
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder { dim: 128 });
    memory.ingestor.models = Arc::new(Ok(Models {
        rag: RagIndex::new(memory.graph.shared_conn(), embedder.clone(), 512, 64).unwrap(),
        embedder,
        completer: model,
        probe: None,
        extraction_model: "fixture-v1".into(),
    }));
}
fn date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 29).unwrap()
}
fn chat(memory: &MemorySystem) -> String {
    let chats = ChatStore::new(memory.graph.shared_conn()).unwrap();
    let chat = chats.create(Some("test-project")).unwrap();
    chats
        .begin(
            &chat.id,
            &uuid::Uuid::new_v4().to_string(),
            "Remember the nightly SQLite decision",
        )
        .unwrap();
    let messages = chats.messages(&chat.id).unwrap();
    chats
        .finish(
            messages[1].turn_id.as_deref().unwrap(),
            "completed",
            "Saved the decision",
            None,
        )
        .unwrap();
    chat.id
}
fn runs(memory: &MemorySystem, conversation: &str) -> Vec<String> {
    use crate::delegation::{store::RunStore, DelegationPlan};
    let store = RunStore::new(memory.graph.shared_conn()).unwrap();
    let fixtures: Vec<Value> = serde_json::from_str(include_str!("fixtures/runs.json")).unwrap();
    let plan = DelegationPlan {
        summary: "Fixture runs".into(),
        containers: vec![],
        assignments: fixtures
            .iter()
            .map(|f| serde_json::from_value(f["assignment"].clone()).unwrap())
            .collect(),
    };
    let runs = store.create("fixture-task", conversation, &plan).unwrap();
    for run in &runs {
        let f = fixtures
            .iter()
            .find(|f| f["assignment"]["key"] == run.assignment.key)
            .unwrap();
        store.sync(&run.id, f).unwrap();
        store
            .state(
                &run.id,
                f["metadata"]["state"].as_str().unwrap(),
                f["metadata"]["reason"].as_str().unwrap(),
            )
            .unwrap();
    }
    runs.into_iter().map(|r| r.id).collect()
}
fn table_count(memory: &MemorySystem, table: &str) -> i64 {
    memory
        .graph
        .shared_conn()
        .lock()
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn ingest_twice_is_durable_idempotent_and_watermark_advances_for_edits() {
    let path = std::env::temp_dir().join(format!("hive-ingest-{}.db", uuid::Uuid::new_v4()));
    let model = Arc::new(FixtureModel::default());
    let (chat_id, watermark, counts) = {
        let graph = KnowledgeGraph::open(&path).unwrap();
        let mut memory = MemorySystem::from_parts(graph, None, &path);
        fixture(&mut memory, model.clone());
        let chat_id = chat(&memory);
        let legacy = memory
            .projects
            .begin_conversation("legacy", "Earlier chat")
            .unwrap();
        memory
            .projects
            .append_message(&legacy.id, "user", "Remember the local database")
            .unwrap();
        runs(&memory, &chat_id);
        let first = memory.ingestor.ingest(date()).await.unwrap();
        assert_eq!((first.conversations, first.runs, first.lessons), (2, 2, 1));
        assert_eq!(first.failed, 0);
        assert_eq!(first.watermark, 5);
        let counts = [
            "entities",
            "edges",
            "rag_chunks",
            "kg_embeddings",
            "memory_ingested",
        ]
        .map(|t| table_count(&memory, t));
        let calls = model.calls.load(Ordering::SeqCst);
        let second = memory.ingestor.ingest(date()).await.unwrap();
        assert_eq!(
            (
                second.conversations,
                second.runs,
                second.lessons,
                second.unchanged
            ),
            (0, 0, 0, 4)
        );
        assert_eq!(second.watermark, first.watermark);
        assert_eq!(model.calls.load(Ordering::SeqCst), calls);
        assert_eq!(
            counts,
            [
                "entities",
                "edges",
                "rag_chunks",
                "kg_embeddings",
                "memory_ingested"
            ]
            .map(|t| table_count(&memory, t))
        );
        assert!(memory.ingestor.claim_nightly(date()).unwrap());
        (chat_id, first.watermark, counts)
    };
    {
        let graph = KnowledgeGraph::open(&path).unwrap();
        let mut memory = MemorySystem::from_parts(graph, None, &path);
        fixture(&mut memory, model.clone());
        assert!(
            !memory.ingestor.claim_nightly(date()).unwrap(),
            "claim survives server restart"
        );
        let restarted = memory.ingestor.ingest(date()).await.unwrap();
        assert_eq!(restarted.watermark, watermark);
        assert_eq!(restarted.unchanged, 4);
        assert_eq!(
            counts,
            [
                "entities",
                "edges",
                "rag_chunks",
                "kg_embeddings",
                "memory_ingested"
            ]
            .map(|t| table_count(&memory, t))
        );
        let conn = memory.graph.shared_conn();
        conn.lock().unwrap().execute("UPDATE messages SET content='Edited assistant reply' WHERE conversation_id=?1 AND role='assistant'", [&chat_id]).unwrap();
        let edited = memory.ingestor.ingest(date()).await.unwrap();
        assert_eq!((edited.conversations, edited.watermark), (1, watermark + 1));
        memory
            .projects
            .append_message(&chat_id, "user", "A later decision")
            .unwrap();
        let appended = memory.ingestor.ingest(date()).await.unwrap();
        assert_eq!(
            (appended.conversations, appended.watermark),
            (1, watermark + 2)
        );
        let (revision, latest): (i64,i64) = conn.lock().unwrap().query_row("SELECT source_revision,(SELECT MAX(id) FROM messages WHERE conversation_id=?1) FROM memory_ingested WHERE kind='conversation' AND source_id=?1", [&chat_id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(revision, latest);
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn fixture_runs_create_one_daily_lesson_retrievable_by_the_planner() {
    let mut memory = MemorySystem::new();
    fixture(&mut memory, Arc::new(FixtureModel::default()));
    let id = chat(&memory);
    runs(&memory, &id);
    memory.ingestor.ingest(date()).await.unwrap();
    let nodes = memory.graph.recent_lessons(10).unwrap();
    assert_eq!(nodes.len(), 1);
    let description = nodes[0].attr_str("description").unwrap();
    for text in [
        "codex on fixture-builder",
        "Compiler could not resolve a dependency",
        "retry",
        "attempt",
        "cursor on fixture-reviewer",
        "Recovered the existing conversation",
    ] {
        assert!(description.contains(text), "{text}: {description}");
    }
    let context = memory
        .conversation_context(&id, "lessons from the compiler failure")
        .await;
    assert!(context.render().contains("codex on fixture-builder"));
    assert!(memory
        .planner_lessons()
        .join("\n")
        .contains("Compiler could not resolve a dependency"));
    assert!(memory
        .search_all("lessons", None)
        .await
        .entities
        .iter()
        .any(|e| e == "Lessons learned 2026-09-29"));
    memory.ingestor.ingest(date()).await.unwrap();
    assert_eq!(memory.graph.recent_lessons(10).unwrap().len(), 1);
}

#[tokio::test]
async fn extraction_and_commit_failures_leave_both_indexes_and_watermark_untouched() {
    let mut memory = MemorySystem::new();
    let model = Arc::new(FixtureModel::default());
    fixture(&mut memory, model.clone());
    let id = chat(&memory);
    model.fail.store(true, Ordering::SeqCst);
    assert_eq!(memory.ingestor.conversation(&id).await.unwrap().failed, 1);
    for table in ["entities", "rag_chunks", "kg_embeddings", "memory_ingested"] {
        assert_eq!(table_count(&memory, table), 0);
    }
    assert_eq!(memory.ingestor.watermark().unwrap(), 0);
    model.fail.store(false, Ordering::SeqCst);
    let conn = memory.graph.shared_conn();
    conn.lock().unwrap().execute_batch("CREATE TRIGGER reject_watermark BEFORE INSERT ON memory_ingested BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END;").unwrap();
    assert_eq!(memory.ingestor.conversation(&id).await.unwrap().failed, 1);
    for table in [
        "entities",
        "edges",
        "rag_chunks",
        "kg_embeddings",
        "memory_ingested",
    ] {
        assert_eq!(table_count(&memory, table), 0);
    }
    assert_eq!(memory.ingestor.watermark().unwrap(), 0);
    conn.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_watermark")
        .unwrap();
    let retried = memory.ingestor.conversation(&id).await.unwrap();
    assert_eq!((retried.conversations, retried.watermark), (1, 1));
    assert_eq!(
        memory.ingestor.conversation(&id).await.unwrap().unchanged,
        1
    );
}

#[tokio::test]
async fn web_finish_triggers_auto_index_without_appending_a_duplicate_reply() {
    let mut memory = MemorySystem::new();
    let model = Arc::new(FixtureModel::default());
    fixture(&mut memory, model.clone());
    let chats = ChatStore::new(memory.graph.shared_conn())
        .unwrap()
        .with_auto_index(memory.clone());
    let chat = chats.create(None).unwrap();
    chats
        .begin(&chat.id, "auto-request", "Remember the local database")
        .unwrap();
    // A partial/active turn is not learned as a finished answer.
    assert_eq!(
        memory
            .ingestor
            .conversation(&chat.id)
            .await
            .unwrap()
            .conversations,
        0
    );
    chats
        .finish("auto-request", "completed", "Use SQLite", None)
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.called.notified())
        .await
        .unwrap();
    let guard = memory.ingestor.gate.lock().await;
    assert_eq!(memory.ingestor.watermark().unwrap(), 1);
    assert!(memory.rag.chunk_count().unwrap() > 0);
    assert_eq!(chats.messages(&chat.id).unwrap().len(), 2);
    assert_eq!(memory.graph.entities_in_project("web").unwrap().len(), 2);
    drop(guard);
    assert_eq!(
        memory
            .ingestor
            .conversation(&chat.id)
            .await
            .unwrap()
            .unchanged,
        1
    );
}

#[tokio::test]
async fn late_events_beyond_the_ui_page_and_long_transcript_tail_are_ingested() {
    let mut memory = MemorySystem::new();
    let model = Arc::new(FixtureModel::default());
    fixture(&mut memory, model.clone());
    let id = chat(&memory);
    let ids = runs(&memory, &id);
    let run = &ids[0];
    memory.ingestor.ingest(date()).await.unwrap();
    let conn = memory.graph.shared_conn();
    conn.lock().unwrap().execute("UPDATE delegated_runs SET metadata=json_set(metadata,'$.last_seen','new heartbeat') WHERE id=?1", [run]).unwrap();
    assert_eq!(memory.ingestor.ingest(date()).await.unwrap().runs, 0);
    for seq in 4..=305 {
        conn.lock().unwrap().execute("INSERT INTO delegated_events VALUES (?1,?2,?3,'native',?4)", params![run,format!("event-{seq}"),seq,json!({"text":format!("Transcript event {seq} includes full output and TAIL-305")}).to_string()]).unwrap();
    }
    let updated = memory.ingestor.ingest(date()).await.unwrap();
    assert_eq!(updated.runs, 1);
    let transcript: String = conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT group_concat(text,' ') FROM rag_chunks WHERE conversation_id=?1",
            [format!("run:{run}")],
            |r| r.get(0),
        )
        .unwrap();
    assert!(transcript.contains("Transcript event 305"));
    assert!(model
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.contains("Transcript event 305")));
    assert_eq!(memory.graph.recent_lessons(10).unwrap().len(), 1);
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
fn config() -> HiveConfig {
    toml::from_str(include_str!("../../../../config/hive.toml")).unwrap()
}

#[tokio::test]
async fn unreachable_ollama_logs_exactly_one_warning_and_skips_the_entire_batch() {
    use tracing::instrument::WithSubscriber;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = config();
    config.llm.local.base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    config.llm.single_provider = Some(hive_common::AiProvider::Zai);
    config.memory.embedding_provider = Some(hive_common::config::EmbeddingProvider::Nvidia);
    let memory = MemorySystem::from_parts(
        KnowledgeGraph::in_memory().unwrap(),
        Some(&config),
        std::path::Path::new(":memory:"),
    );
    for _ in 0..5 {
        chat(&memory);
    }
    let log = Log::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();
    let result = memory
        .ingestor
        .ingest(date())
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert!(result.skipped);
    assert_eq!(
        (result.conversations, result.runs, result.watermark),
        (0, 0, 0)
    );
    assert_eq!(table_count(&memory, "memory_ingested"), 0);
    assert_eq!(table_count(&memory, "entities"), 0);
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert_eq!(text.matches("WARN").count(), 1, "{text}");
    assert!(text.contains("memory ingest skipped: local Ollama unavailable"));
}

#[tokio::test]
async fn concurrent_manual_triggers_share_the_same_durable_watermarks() {
    let mut memory = MemorySystem::new();
    fixture(&mut memory, Arc::new(FixtureModel::default()));
    chat(&memory);
    let (a, b) = tokio::join!(
        memory.ingestor.ingest(date()),
        memory.ingestor.ingest(date())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.conversations + b.conversations, 1);
    assert_eq!(a.lessons + b.lessons, 1);
    assert_eq!(memory.ingestor.watermark().unwrap(), 2);
}

struct OllamaFixture {
    url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for OllamaFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl OllamaFixture {
    async fn start(remote_alias: bool, redirect: bool) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let location = format!("{url}/must-not-follow");
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let (header_end, content_length) = loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(at) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..at]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|n| n.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (at + 4, len);
                    }
                };
                while bytes.len() < header_end + content_length {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let path = String::from_utf8_lossy(&bytes[..header_end])
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_string();
                let body = if content_length > 0 {
                    serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
                } else {
                    Value::Null
                };
                recorded.lock().unwrap().push((path.clone(), body));
                let response = match path.as_str() {
                    "/api/tags" => {
                        let mut model = json!({"name":"qwen3.5:9b"});
                        if remote_alias { model["remote_model"] = "remote-inference".into(); }
                        json!({"models":[model,{"name":"nomic-embed-text:latest"}]})
                    }
                    "/api/chat" => json!({"message":{"content":r#"{"entities":[{"name":"Local inference","kind":"concept","description":"Private memory stays local"}]}"#}}),
                    "/api/embeddings" => json!({"embedding":[1.0,0.5,0.25]}),
                    _ => panic!("unexpected inference endpoint: {path}"),
                }.to_string();
                let header = if redirect {
                    format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\n")
                } else {
                    "HTTP/1.1 200 OK\r\n".into()
                };
                let response = format!("{header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}

#[tokio::test]
async fn ingestion_uses_configured_ollama_models_even_with_zai_and_nvidia_selected() {
    let server = OllamaFixture::start(false, false).await;
    let mut config = config();
    config.llm.local.base_url = server.url.clone();
    config.llm.single_provider = Some(hive_common::AiProvider::Zai);
    config.llm.zai = Some(hive_common::config::CloudLlmConfig {
        model: "must-not-be-used".into(),
        api_key: Some("fixture-key".into()),
        api_key_env: None,
        base_url: Some(format!("{}/must-not-use-zai", server.url)),
    });
    config.memory.embedding_provider = Some(hive_common::config::EmbeddingProvider::Nvidia);
    config.llm.nvidia.base_url = format!("{}/must-not-use-nvidia", server.url);
    let memory = MemorySystem::from_parts(
        KnowledgeGraph::in_memory().unwrap(),
        Some(&config),
        std::path::Path::new(":memory:"),
    );
    let id = chat(&memory);
    runs(&memory, &id);
    let first = memory.ingestor.ingest(date()).await.unwrap();
    assert!(!first.skipped);
    assert_eq!(
        (first.conversations, first.runs, first.lessons, first.failed),
        (1, 2, 1, 0)
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|(p, _)| p == "/api/tags").count(), 1);
    assert!(requests.iter().any(|(p, _)| p == "/api/chat"));
    assert!(requests.iter().any(|(p, _)| p == "/api/embeddings"));
    for (path, body) in requests.iter() {
        match path.as_str() {
            "/api/tags" => {}
            "/api/chat" => {
                assert_eq!(body["model"], config.llm.local.model);
                assert_eq!(body["options"]["num_ctx"], config.llm.local.max_context);
            }
            "/api/embeddings" => assert_eq!(body["model"], config.memory.embedding_model),
            _ => panic!("content left the local inference path: {path}"),
        }
    }
    drop(requests);
    let providers: String = memory
        .graph
        .shared_conn()
        .lock()
        .unwrap()
        .query_row(
            "SELECT group_concat(DISTINCT provider) FROM rag_chunks",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(providers, "ollama");
}

#[tokio::test]
async fn ingestion_rejects_remote_aliases_and_redirects_before_sending_transcripts() {
    for (alias, redirect) in [(true, false), (false, true)] {
        let server = OllamaFixture::start(alias, redirect).await;
        let mut config = config();
        config.llm.local.base_url = server.url.clone();
        let memory = MemorySystem::from_parts(
            KnowledgeGraph::in_memory().unwrap(),
            Some(&config),
            std::path::Path::new(":memory:"),
        );
        chat(&memory);
        assert!(memory.ingestor.ingest(date()).await.unwrap().skipped);
        assert_eq!(
            server.requests.lock().unwrap().as_slice(),
            &[("/api/tags".into(), Value::Null)]
        );
        assert_eq!(memory.ingestor.watermark().unwrap(), 0);
    }
}

#[test]
fn private_ollama_client_rejects_nonlocal_urls_and_cloud_models() {
    for url in [
        "https://ollama.com",
        "http://192.0.2.1:11434",
        "http://localhost.example.invalid",
        "http://user@localhost:11434",
        "http://localhost:11434?remote=true",
    ] {
        assert!(
            OllamaClient::local_only(url.into(), "test".into()).is_err(),
            "{url}"
        );
    }
    assert!(
        OllamaClient::local_only("http://localhost:11434".into(), "model:cloud".into()).is_err()
    );
    for url in [
        "http://localhost:11434",
        "http://127.0.0.1:11434",
        "http://[::1]:11434",
    ] {
        assert!(OllamaClient::local_only(url.into(), "test".into()).is_ok());
    }
}

#[tokio::test]
async fn deleting_a_source_during_inference_cannot_resurrect_its_memory() {
    struct DeleteDuringExtraction(Arc<Mutex<Connection>>, String);
    #[async_trait::async_trait]
    impl Completer for DeleteDuringExtraction {
        async fn complete(&self, _: &str) -> anyhow::Result<String> {
            let mut db = self.0.lock().unwrap();
            let tx = db.transaction()?;
            tx.execute("DELETE FROM web_chat_turns WHERE conversation_id=?1", [&self.1])?;
            tx.execute("DELETE FROM messages WHERE conversation_id=?1", [&self.1])?;
            tx.execute("DELETE FROM conversations WHERE id=?1", [&self.1])?;
            tx.commit()?;
            Ok(r#"{"entities":[{"name":"Deleted evidence","description":"Must not be committed"}]}"#.into())
        }
    }
    let mut memory = MemorySystem::new();
    let id = chat(&memory);
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder { dim: 128 });
    memory.ingestor.models = Arc::new(Ok(Models {
        rag: memory.rag.clone(),
        embedder,
        completer: Arc::new(DeleteDuringExtraction(
            memory.graph.shared_conn(),
            id.clone(),
        )),
        probe: None,
        extraction_model: "delete-fixture".into(),
    }));
    let result = memory.ingestor.conversation(&id).await.unwrap();
    assert_eq!(
        (result.conversations, result.watermark, result.failed),
        (0, 0, 0)
    );
    assert_eq!(table_count(&memory, "entities"), 0);
    assert_eq!(table_count(&memory, "rag_chunks"), 0);
}

#[tokio::test]
async fn disabling_auto_index_leaves_manual_and_nightly_catchup_available() {
    let mut memory = MemorySystem::new();
    memory.ingestor.config.auto_index = false;
    let id = chat(&memory);
    memory.index_saved_conversation(&id).await;
    assert_eq!(memory.ingestor.watermark().unwrap(), 0);
    assert_eq!(
        memory.ingestor.ingest(date()).await.unwrap().conversations,
        1
    );
}
