use std::sync::{Arc, Mutex};
use rusqlite::{params, Connection};
use serde_json::json;
use hive_core::delegation::DelegationPlan;
use hive_core::delegation::store::RunStore;

#[test]
fn test_stuck_launching_recovery_via_retry_launching() {
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

    // retry_setup refuses launching runs with runner_path
    assert!(store.retry_setup(run_id).is_err());

    // Within timeout: retry_launching is refused
    assert!(store.retry_launching(run_id, 30).is_err());

    // Backdate claimed_at past timeout
    conn.lock().unwrap().execute(
        "UPDATE delegated_runs SET metadata=json_set(metadata,'$.claimed_at',?) WHERE id=?",
        params![chrono::Utc::now().timestamp() - 60, run_id],
    ).unwrap();

    // After timeout, retry_launching succeeds, resetting state to queued and runner_path to None
    store.retry_launching(run_id, 30).expect(
        "run claimed into 'launching' past timeout must have a recovery path via retry_launching"
    );

    let recovered = store.get(run_id).unwrap();
    assert_eq!(recovered.state, "queued", "state must be reset to queued");
    assert!(recovered.runner_path.is_none(), "runner_path must be reset to None");
}

#[test]
fn test_failed_and_installed_runs_cannot_retry_setup() {
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

    // Failed runs must NOT be eligible for retry_setup (must use replace flow)
    assert!(store.retry_setup(run_id).is_err());

    // A run whose runner was installed (runner_path IS NOT NULL) must NOT be silently reset
    store.state(run_id, "needs-setup", "setup required").unwrap();
    assert!(store.get(run_id).unwrap().runner_path.is_some());
    assert!(store.retry_setup(run_id).is_err());
}

#[test]
fn test_needs_setup_without_runner_can_retry_setup() {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    let store = RunStore::new(conn.clone()).unwrap();

    let plan: DelegationPlan = serde_json::from_value(json!({
        "summary": "needs-setup test",
        "assignments": [
            {
                "key": "setup-run",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/setup-run",
                "objective": "test recovery",
                "dependencies": [],
                "acceptance_criteria": ["criteria"]
            }
        ]
    })).unwrap();

    let runs = store.create("task-f2-c", "chat", &plan).unwrap();
    let run_id = &runs[0].id;

    store.state(run_id, "needs-setup", "login required").unwrap();
    assert_eq!(store.get(run_id).unwrap().state, "needs-setup");
    assert!(store.get(run_id).unwrap().runner_path.is_none());

    // Needs-setup run without runner installed can retry setup
    store.retry_setup(run_id).unwrap();
    assert_eq!(store.get(run_id).unwrap().state, "queued");
}
