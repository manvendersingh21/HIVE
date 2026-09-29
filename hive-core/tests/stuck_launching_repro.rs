use std::sync::{Arc, Mutex};
use rusqlite::Connection;
use serde_json::json;
use hive_core::delegation::DelegationPlan;
use hive_core::delegation::store::RunStore;

#[test]
fn test_stuck_launching_recovery_via_retry_setup() {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    let store = RunStore::new(conn.clone()).unwrap();

    let plan: DelegationPlan = serde_json::from_value(json!({
        "summary": "stuck launching test",
        "assignments": [
            {
                "key": "stuck-run",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/stuck-run",
                "objective": "test recovery",
                "dependencies": [],
                "acceptance_criteria": ["criteria"]
            }
        ]
    })).unwrap();

    let runs = store.create("task-f2", "chat", &plan).unwrap();
    let run_id = &runs[0].id;
    assert_eq!(store.get(run_id).unwrap().state, "queued");

    // Claim the run into 'launching' with a runner
    assert!(store.claim(run_id, "runner.py").unwrap());
    assert_eq!(store.get(run_id).unwrap().state, "launching");
    assert!(store.get(run_id).unwrap().runner_path.is_some());

    // On unmodified code, a run claimed into 'launching' has no recovery path:
    // retry_setup fails because state != ('needs-setup', 'disconnected') and runner_path is not null.
    // After the fix, retry_setup is eligible for runs in 'launching' (or failed), resetting state to 'queued'.
    store.retry_setup(run_id).expect(
        "run claimed into 'launching' with no confirmed tmux session must have a recovery path via retry_setup"
    );

    let recovered = store.get(run_id).unwrap();
    assert_eq!(recovered.state, "queued", "state must be reset to queued");
    assert!(recovered.runner_path.is_none(), "runner_path must be reset to None");
}

#[test]
fn test_stuck_launching_marked_failed_can_retry_setup() {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    let store = RunStore::new(conn.clone()).unwrap();

    let plan: DelegationPlan = serde_json::from_value(json!({
        "summary": "stuck launching fail test",
        "assignments": [
            {
                "key": "stuck-run",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/stuck-run",
                "objective": "test recovery",
                "dependencies": [],
                "acceptance_criteria": ["criteria"]
            }
        ]
    })).unwrap();

    let runs = store.create("task-f2-b", "chat", &plan).unwrap();
    let run_id = &runs[0].id;

    assert!(store.claim(run_id, "runner.py").unwrap());
    // Simulate being marked failed with clear reason after bounded timeout
    store.state(run_id, "failed", "Launch timed out: agent tmux session never appeared").unwrap();
    assert_eq!(store.get(run_id).unwrap().state, "failed");
    assert_eq!(store.get(run_id).unwrap().metadata["reason"], "Launch timed out: agent tmux session never appeared");

    // Must be eligible for retry_setup
    store.retry_setup(run_id).unwrap();
    let recovered = store.get(run_id).unwrap();
    assert_eq!(recovered.state, "queued");
    assert!(recovered.runner_path.is_none());
}
