use hive_core::{
    delegation::{self, store::RunStore, workspace_gc},
    memory::graph::{Entity, KnowledgeGraph},
};
use serde_json::json;
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

fn plan() -> delegation::DelegationPlan {
    serde_json::from_value(json!({"summary":"build", "assignments":[{
        "key":"build", "device":"test", "agent":"codex", "model":null,
        "workspace":"~/hive-workspaces/test", "objective":"cargo build --workspace",
        "dependencies":[], "acceptance_criteria":["build passes"]
    }]}))
    .unwrap()
}

#[test]
fn disk_placement_rejects_low_disk_accepts_boundary_and_healthy() {
    let graph = KnowledgeGraph::in_memory().unwrap();
    let mut command_only = plan().assignments[0].clone();
    command_only.objective = "implement feature".into();
    command_only.acceptance_criteria = vec!["verified".into()];
    command_only.acceptance_checks = serde_json::from_value(json!([
        {"kind":"command", "argv":["cargo","build"], "cwd":".", "timeout_seconds":60}
    ]))
    .unwrap();
    for free in [9.9, 10.0, 50.0] {
        graph
            .upsert_entity(&Entity::new(
                "machine",
                "test",
                json!({"disk_free_gb":free}),
            ))
            .unwrap();
        assert_eq!(
            delegation::placement::validate_disk(&command_only, &graph).is_ok(),
            free >= 10.0
        );
        let result = delegation::placement::validate_disk(&plan().assignments[0], &graph);
        if free < 10.0 {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("test") && error.contains("9.9 GiB"));
        } else {
            result.unwrap();
        }
    }
}

#[test]
fn terminal_grace_and_live_states_survive_sync() {
    let store = RunStore::new(Arc::new(Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    )))
    .unwrap();
    let id = store.create("task", "chat", &plan()).unwrap()[0].id.clone();
    for state in ["completed", "failed", "superseded"] {
        let mut run = store.get(&id).unwrap();
        run.state = state.into();
        run.metadata = json!({"terminal_since":100});
        assert!(!workspace_gc::eligible(
            &run,
            &[run.clone()],
            100 + workspace_gc::GRACE_SECONDS - 1
        ));
        assert!(workspace_gc::eligible(
            &run,
            &[run.clone()],
            100 + workspace_gc::GRACE_SECONDS
        ));
        for live in [
            "working",
            "paused-quota",
            "waiting-for-peer",
            "queued",
            "disconnected",
            "verifying",
        ] {
            let mut other = run.clone();
            other.state = live.into();
            assert!(!workspace_gc::eligible(&other, &[other.clone()], 99999));
            assert!(!workspace_gc::eligible(&run, &[run.clone(), other], 99999));
        }
    }
    store.state(&id, "failed", "test").unwrap();
    let since = store.get(&id).unwrap().metadata["terminal_since"].clone();
    assert!(since.is_i64());
    store
        .sync(
            &id,
            &json!({"metadata":{"state":"failed", "terminal_since":1},"events":[]}),
        )
        .unwrap();
    assert_eq!(store.get(&id).unwrap().metadata["terminal_since"], since);
    store
        .record_workspace_gc(&id, since.as_i64().unwrap(), &json!({"freed_bytes":42}))
        .unwrap();
    store
        .sync(&id, &json!({"metadata":{"state":"failed"},"events":[]}))
        .unwrap();
    assert_eq!(
        store.get(&id).unwrap().metadata["workspace_gc"]["freed_bytes"],
        42
    );
    store.state(&id, "working", "resume").unwrap();
    assert!(store.get(&id).unwrap().metadata["terminal_since"].is_null());
    assert!(store.get(&id).unwrap().metadata["workspace_gc"].is_null());
}

#[test]
fn gc_only_removes_caches_under_temp_home() {
    let home = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("hive-gc-{}", uuid::Uuid::new_v4()));
    let ws = home.join("hive-workspaces/test");
    for dir in [
        "repo/target",
        "repo/frontend/node_modules",
        "repo/frontend/.next",
        "repo/playwright-report",
        "repo/test-results",
        "repo/src",
        "repo/.git/target",
    ] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
        std::fs::write(ws.join(dir).join("keep"), "data").unwrap();
    }
    std::fs::create_dir_all(home.join("outside/target")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(home.join("outside"), ws.join("linked")).unwrap();
    let invoke = |workspace: &str, live: Vec<&str>| {
        let mut child = Command::new("python3")
            .args(["-c", workspace_gc::SCRIPT])
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"workspace":workspace,"live":live})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    };
    assert!(!invoke("~/outside", vec![]).status.success());
    assert!(!invoke("~/hive-workspaces/../outside", vec![])
        .status
        .success());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(home.join("outside"), home.join("hive-workspaces/alias"))
            .unwrap();
        assert!(!invoke("~/hive-workspaces/alias", vec![]).status.success());
    }
    assert!(
        !invoke("~/hive-workspaces/test", vec!["~/hive-workspaces/test"])
            .status
            .success()
    );
    assert!(ws.join("repo/target/keep").exists());
    let out = invoke("~/hive-workspaces/test", vec![]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["removed"].as_array().unwrap().len(), 5);
    assert!(result["freed_bytes"].as_u64().unwrap() > 0);
    assert!(ws.join("repo/src/keep").exists());
    assert!(ws.join("repo/.git/target/keep").exists());
    assert!(home.join("outside/target").exists());
    assert!(!ws.join("repo/target").exists());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn remote_journal_blocks_stale_terminal_snapshots_and_pending_followups() {
    let home = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("hive-journal-gc-{}", uuid::Uuid::new_v4()));
    let id = uuid::Uuid::new_v4().to_string();
    let journal = home.join(".hive/runs").join(&id);
    let cache = home.join("hive-workspaces/test/repo/target");
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let db = rusqlite::Connection::open(journal.join("journal.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE inbox(state TEXT);
        INSERT INTO metadata VALUES ('state','\"working\"');",
    )
    .unwrap();
    let invoke = || {
        let mut child = Command::new("python3")
            .args(["-c", workspace_gc::SCRIPT])
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"workspace":"~/hive-workspaces/test",
            "live":[],"journal":true,"run_id":id,"superseded":false})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    };
    for state in ["working", "paused-quota", "waiting-for-peer"] {
        db.execute(
            "UPDATE metadata SET value=? WHERE key='state'",
            [json!(state).to_string()],
        )
        .unwrap();
        assert!(!invoke().status.success());
        assert!(cache.exists());
    }
    db.execute("UPDATE metadata SET value='\"completed\"'", [])
        .unwrap();
    db.execute("INSERT INTO inbox VALUES ('queued')", [])
        .unwrap();
    assert!(!invoke().status.success());
    assert!(cache.exists());
    // Failed runners exit their turn loop; a stranded delivering inbox entry
    // must not retain their caches forever.
    db.execute("UPDATE metadata SET value='\"failed\"'", [])
        .unwrap();
    db.execute("UPDATE inbox SET state='delivering'", [])
        .unwrap();
    let result = invoke();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!cache.exists());
    drop(db);
    std::fs::remove_dir_all(home).unwrap();
}
