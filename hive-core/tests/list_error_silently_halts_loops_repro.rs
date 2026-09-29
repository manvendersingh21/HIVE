use std::sync::{Arc, Mutex};
use rusqlite::Connection;
use serde_json::json;
use hive_core::delegation::DelegationPlan;
use hive_core::delegation::store::RunStore;

#[test]
fn test_list_error_silently_halts_loops_repro() {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    let store = RunStore::new(conn.clone()).unwrap();

    let plan: DelegationPlan = serde_json::from_value(json!({
        "summary": "corrupt row test",
        "assignments": [
            {
                "key": "valid-run",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/valid-run",
                "objective": "objective 1",
                "dependencies": [],
                "acceptance_criteria": ["criteria 1"]
            },
            {
                "key": "corrupt-run",
                "device": "local",
                "agent": "codex",
                "model": null,
                "workspace": "/tmp/corrupt-run",
                "objective": "objective 2",
                "dependencies": [],
                "acceptance_criteria": ["criteria 2"]
            }
        ]
    })).unwrap();

    let runs_created = store.create("task-f3", "chat", &plan).unwrap();
    assert_eq!(runs_created.len(), 2);
    let valid_id = &runs_created[0].id;
    let corrupt_id = &runs_created[1].id;

    // Corrupt the second row in delegated_runs (e.g. invalid JSON in assignment)
    conn.lock()
        .unwrap()
        .execute(
            "UPDATE delegated_runs SET assignment='{not valid json}' WHERE id=?",
            [corrupt_id],
        )
        .unwrap();

    // On unmodified code, store.list() fails completely with Err, which causes
    // background loops to silently drop the error and halt processing all runs.
    // After the fix, one corrupt row is skipped with a logged reason and the valid run is returned.
    let (list_result, error_count) = store.list().expect(
        "one corrupted row must not fail the whole list; bad row should be skipped or quarantined instead of halting loops"
    );

    assert_eq!(error_count, 1, "one unreadable row must surface in error count");
    assert_eq!(list_result.len(), 1, "the valid row must be returned");
    assert_eq!(&list_result[0].id, valid_id, "returned run must be the uncorrupted run");
}
