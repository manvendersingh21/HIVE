use std::sync::{Arc, Mutex};
use rusqlite::Connection;
use serde_json::json;
use hive_core::delegation::DelegationPlan;
use hive_core::delegation::store::RunStore;

#[test]
fn test_relay_budget_unbounded_repro() {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    let store = RunStore::new(conn.clone()).unwrap();

    let plan: DelegationPlan = serde_json::from_value(json!({
        "summary": "relay budget test",
        "assignments": [
            {
                "key": "a",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/test-a",
                "objective": "test a",
                "dependencies": [],
                "acceptance_criteria": ["test"]
            },
            {
                "key": "b",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/test-b",
                "objective": "test b",
                "dependencies": [],
                "acceptance_criteria": ["test"]
            }
        ]
    })).unwrap();

    let runs = store.create("task-f4", "chat", &plan).unwrap();
    let sender = &runs[0];
    let receiver = &runs[1];

    let now = chrono::Utc::now().timestamp();
    // Simulate 9 peer budget entries that were recorded outside the 60s window (>60s ago)
    {
        let db = conn.lock().unwrap();
        for i in 0..9 {
            db.execute(
                "INSERT INTO delegated_relay_budget (message_id, source, task_id, attempted_at) VALUES (?, ?, ?, ?)",
                rusqlite::params![format!("old-msg-{i}"), sender.id, sender.task_id, now - 120 + i],
            ).unwrap();
        }
        let count: i64 = db.query_row(
            "SELECT count(*) FROM delegated_relay_budget",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 9, "precondition: 9 old budget rows exist");
    }

    // Stage a new peer message to trigger budget check on delivery path
    store.message(
        "new-msg",
        &sender.id,
        &receiver.id,
        &json!({"id": "new-msg", "source": sender.id, "kind": "answer", "text": "evidence"}),
    ).unwrap();

    // Trigger delivery path which checks peer budget
    let _delivery = store.next_delivery(&receiver.id).unwrap();

    // Verify whether old rows outside 60s window were pruned
    let old_count: i64 = conn.lock().unwrap().query_row(
        "SELECT count(*) FROM delegated_relay_budget WHERE attempted_at <= ?",
        [now - 60],
        |r| r.get(0),
    ).unwrap();

    assert_eq!(
        old_count, 0,
        "delegated_relay_budget rows outside the 60s window must be pruned (count never drops from 9 to 0 on unmodified code)"
    );
}
