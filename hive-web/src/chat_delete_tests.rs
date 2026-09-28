//! Router tests for `DELETE /api/chats/{id}`: they drive the real router,
//! auth layer included, over an in-memory database shared by the chat store,
//! the delegation store and the relay audit chain.
use super::*;
use axum::{
    body::Body,
    http::{header, Request},
};
use hive_core::delegation::store::{Run, RunStore};
use hive_core::watchdog::incidents::IncidentStore;
use serde_json::json;
use tower::ServiceExt;

struct Fixture {
    app: Router,
    cookie: String,
    handle: chat::AgentHandle,
    runs: RunStore,
}

async fn fixture() -> Fixture {
    let agent = MasterAgent::new(
        LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
        WorkerPool::new(vec![]),
        SkillRegistry::new(),
        MemorySystem::new(),
    );
    let handle = chat::AgentHandle::enabled(std::sync::Arc::new(agent), "test".into()).unwrap();
    let runs = delegation::store(&handle).unwrap();
    let state = AppState {
        auth: auth::Auth::new("test-password".into()),
        agent: handle.clone(),
        workers: workers::WorkerIngest::from_env(),
        incidents: incidents::IncidentReview::new(IncidentStore::in_memory().unwrap()),
    };
    let app = app_router(state, std::env::temp_dir().to_str().unwrap());
    let login = app
        .clone()
        .oneshot(
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("password=test-password"))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    Fixture {
        app,
        cookie,
        handle,
        runs,
    }
}

impl Fixture {
    async fn send(&self, method: &str, path: &str) -> (StatusCode, String) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::COOKIE, &self.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// A saved chat with one finished turn, a RAG memory row, and two
    /// delegated runs of one task (so peer messages can be relayed).
    fn chat_with_history(&self, run_state: &str) -> (String, Vec<Run>) {
        let store = self.handle.history.as_ref().unwrap();
        let chat = store.create(None).unwrap().id;
        let turn = uuid::Uuid::new_v4().to_string();
        store.begin(&chat, &turn, "Build the thing").unwrap();
        store.finish(&turn, "completed", "Dispatched", None).unwrap();
        self.sql(&format!(
            "INSERT INTO rag_chunks(project_id,conversation_id,chunk_index,text,embedding,dim) VALUES ('web','{chat}',0,'memory',x'00',1);
             INSERT INTO rag_indexed(conversation_id,fingerprint) VALUES ('{chat}','fp');"
        ));
        let plan = serde_json::from_value(json!({"summary":"implement and verify","assignments":[
            {"key":"implementer","device":"local","agent":"codex","model":null,
             "workspace":"~/hive-workspaces/implementer","objective":"implement",
             "dependencies":[],"acceptance_criteria":["verified"]},
            {"key":"verifier","device":"local","agent":"codex","model":null,
             "workspace":"~/hive-workspaces/verifier","objective":"verify",
             "dependencies":["implementer"],"acceptance_criteria":["verified"]}
        ]}))
        .unwrap();
        let runs = self.runs.create(&turn, &chat, &plan).unwrap();
        for run in &runs {
            self.runs
                .sync(
                    &run.id,
                    &json!({"metadata":{"state":"working"},"approvals":[],"events":[
                        {"id":"e1","seq":1,"kind":"state","payload":{"state":"working"}}
                    ]}),
                )
                .unwrap();
            self.runs.state(&run.id, run_state, "test").unwrap();
        }
        (chat, runs)
    }

    fn sql(&self, batch: &str) {
        let conn = self.handle.agent.as_ref().unwrap().memory.graph.shared_conn();
        conn.lock().unwrap().execute_batch(batch).unwrap();
    }

    fn count(&self, sql: &str) -> i64 {
        let conn = self.handle.agent.as_ref().unwrap().memory.graph.shared_conn();
        let db = conn.lock().unwrap();
        db.query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

#[tokio::test]
async fn delete_removes_chat_messages_memory_and_finished_runs() {
    let f = fixture().await;
    let (chat, runs) = f.chat_with_history("completed");
    let (keep, _) = f.chat_with_history("failed");
    assert!(f.count(&format!("SELECT count(*) FROM delegated_events WHERE run_id='{}'", runs[0].id)) > 0);

    let (status, body) = f.send("DELETE", &format!("/api/chats/{chat}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(body.is_empty());

    let (status, _) = f.send("GET", &format!("/api/chats/{chat}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for sql in [
        format!("SELECT count(*) FROM conversations WHERE id='{chat}'"),
        format!("SELECT count(*) FROM messages WHERE conversation_id='{chat}'"),
        format!("SELECT count(*) FROM web_chat_turns WHERE conversation_id='{chat}'"),
        format!("SELECT count(*) FROM rag_chunks WHERE conversation_id='{chat}'"),
        format!("SELECT count(*) FROM rag_indexed WHERE conversation_id='{chat}'"),
        format!("SELECT count(*) FROM delegated_runs WHERE conversation_id='{chat}'"),
        format!(
            "SELECT count(*) FROM delegated_events WHERE run_id IN ('{}','{}')",
            runs[0].id, runs[1].id
        ),
    ] {
        assert_eq!(f.count(&sql), 0, "{sql}");
    }
    // Another conversation is untouched.
    let (status, _) = f.send("GET", &format!("/api/chats/{keep}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.count(&format!("SELECT count(*) FROM messages WHERE conversation_id='{keep}'")), 2);
    assert_eq!(f.count(&format!("SELECT count(*) FROM delegated_runs WHERE conversation_id='{keep}'")), 2);
    assert_eq!(f.count(&format!("SELECT count(*) FROM rag_chunks WHERE conversation_id='{keep}'")), 1);
    let (_, list) = f.send("GET", "/api/chats").await;
    assert!(!list.contains(&chat) && list.contains(&keep), "{list}");
}

#[tokio::test]
async fn delete_unknown_chat_is_404_and_requires_auth() {
    let f = fixture().await;
    let (status, body) = f.send("DELETE", "/api/chats/conv-does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "Chat not found");

    let anonymous = f
        .app
        .clone()
        .oneshot(
            Request::delete("/api/chats/conv-does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn delete_removes_only_that_chats_acceptance_evidence() {
    let f = fixture().await;
    let (chat, runs) = f.chat_with_history("completed");
    let (_, keep) = f.chat_with_history("completed");
    // Legacy assignments have no mechanical checks, so no passing measurement
    // can be manufactured. Exercise the bounded failure path before deletion.
    for run in [&runs[0], &keep[0]] {
        for turn in 1..=3 {
            f.runs.sync(&run.id, &json!({"metadata":{"state":"completed","acceptance_turn":turn},"events":[],"approvals":[]})).unwrap();
            f.runs.assess_completion(&run.id, turn, &[]).unwrap();
            if let Some(delivery) = f.runs.next_delivery(&run.id).unwrap() {
                f.runs.message_delivered(&delivery).unwrap();
            }
        }
        assert_eq!(f.runs.get(&run.id).unwrap().state, "no_agreement");
    }
    assert_eq!(f.count("SELECT count(*) FROM delegated_completions"), 2);
    let (status, body) = f.send("DELETE", &format!("/api/chats/{chat}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(f.count("SELECT count(*) FROM delegated_completions"), 1);
    assert!(f.runs.get(&keep[0].id).unwrap().completion.is_some());
    assert!(f.runs.verify_audit_chain().unwrap());
}

#[tokio::test]
async fn delete_is_refused_with_409_while_any_run_is_live() {
    for live in chat::LIVE_RUN_STATES {
        let f = fixture().await;
        let (chat, runs) = f.chat_with_history("completed");
        f.runs.state(&runs[1].id, live, "test").unwrap();
        let (status, body) = f.send("DELETE", &format!("/api/chats/{chat}")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{live}");
        assert!(body.contains("1 delegated run in progress"), "{live}: {body}");
        // Nothing was deleted.
        let (status, _) = f.send("GET", &format!("/api/chats/{chat}")).await;
        assert_eq!(status, StatusCode::OK, "{live}");
        assert_eq!(f.count(&format!("SELECT count(*) FROM delegated_runs WHERE conversation_id='{chat}'")), 2);
        assert_eq!(f.count(&format!("SELECT count(*) FROM rag_chunks WHERE conversation_id='{chat}'")), 1);
    }
}

#[tokio::test]
async fn delete_is_refused_with_409_while_a_chat_request_is_active() {
    let f = fixture().await;
    let store = f.handle.history.as_ref().unwrap();
    let chat = store.create(None).unwrap().id;
    store
        .begin(&chat, &uuid::Uuid::new_v4().to_string(), "Still planning")
        .unwrap();
    let (status, body) = f.send("DELETE", &format!("/api/chats/{chat}")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("request running"), "{body}");
    assert_eq!(f.count(&format!("SELECT count(*) FROM messages WHERE conversation_id='{chat}'")), 2);
}

#[tokio::test]
async fn relay_audit_is_preserved_and_the_chain_stays_valid_after_delete() {
    let f = fixture().await;
    let (chat, runs) = f.chat_with_history("completed");
    f.runs
        .message("m-1", &runs[0].id, &runs[1].id, &json!({"id":"m-1","text":"interface agreed"}))
        .unwrap();
    f.runs
        .message("m-2", "user", &runs[1].id, &json!({"id":"m-2","text":"ship it"}))
        .unwrap();
    let audit_rows = f.count("SELECT count(*) FROM delegated_relay_audit");
    assert!(audit_rows > 0);
    let head = f.count("SELECT position FROM delegated_relay_head");
    assert!(f.runs.verify_audit_chain().unwrap());

    let (status, body) = f.send("DELETE", &format!("/api/chats/{chat}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert_eq!(f.count("SELECT count(*) FROM delegated_relay_audit"), audit_rows);
    assert_eq!(f.count("SELECT position FROM delegated_relay_head"), head);
    // Rows naming the deleted run are still there as evidence.
    assert!(f.count(&format!(
        "SELECT count(*) FROM delegated_relay_audit WHERE json_extract(record,'$.destination')='{}'",
        runs[1].id
    )) > 0);
    // A full re-verification from genesis still holds.
    assert!(f.runs.verify_audit_chain().unwrap());
    // And the chain keeps growing correctly afterwards.
    let (other, others) = f.chat_with_history("completed");
    f.runs
        .message("m-3", &others[0].id, &others[1].id, &json!({"id":"m-3","text":"next"}))
        .unwrap();
    assert!(f.count("SELECT count(*) FROM delegated_relay_audit") > audit_rows);
    assert!(f.runs.verify_audit_chain().unwrap());
    assert_eq!(f.send("DELETE", &format!("/api/chats/{other}")).await.0, StatusCode::NO_CONTENT);
    assert!(f.runs.verify_audit_chain().unwrap());
}
