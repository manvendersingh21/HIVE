//! Peer scheduling regressions use the coordinator store and real runner journal
//! with disposable local state; no live agent, credentials or SSH is required.
use super::*;

fn fixture() -> (RunStore, Vec<Run>) {
    let graph = hive_core::memory::graph::KnowledgeGraph::in_memory().unwrap();
    let store = RunStore::new(graph.shared_conn()).unwrap();
    let plan = serde_json::from_value(json!({"summary":"implement and verify","assignments":[
        {"key":"implementer","device":"local","agent":"codex","model":null,
         "workspace":"~/hive-workspaces/implementer","objective":"implement",
         "dependencies":[],"acceptance_criteria":["verified"]},
        {"key":"verifier","device":"local","agent":"codex","model":null,
         "workspace":"~/hive-workspaces/verifier","objective":"verify",
         "dependencies":["implementer"],"acceptance_criteria":["verified"]}
    ]}))
    .unwrap();
    let runs = store.create("task", "chat", &plan).unwrap();
    (store, runs)
}

fn snapshot(state: &str) -> Value {
    json!({"metadata":{"state":state},"events":[],"approvals":[]})
}

fn sync(store: &RunStore, run: &Run, snapshot: &mut Value) -> anyhow::Result<()> {
    sync_peer_snapshot(store, run, &store.list()?.0, snapshot)
}

fn question(to: &Run) -> Value {
    let mut snapshot = snapshot("waiting-for-peer");
    snapshot["events"] = json!([{
        "id":"question", "seq":1, "kind":"peer",
        "payload":{"to":to.id,"kind":"question","text":"Which interface should I implement?"}
    }]);
    snapshot
}

fn ready(store: &RunStore, run: &Run) -> Result<bool, String> {
    dependency_ready(
        &store.list().unwrap().0,
        run,
        &store.pending_messages(&run.id).unwrap(),
    )
}

#[test]
fn working_dependency_keeps_verifier_queued_even_with_a_message() {
    let (store, runs) = fixture();
    let mut snapshot = question(&runs[1]);
    snapshot["metadata"]["state"] = json!("working");
    sync(&store, &runs[0], &mut snapshot).unwrap();
    assert_eq!(ready(&store, &runs[1]), Ok(false));
    assert_eq!(store.get(&runs[1].id).unwrap().state, "queued");
}

#[test]
fn waiting_dependency_with_a_message_to_us_releases_verifier() {
    let (store, runs) = fixture();
    sync(&store, &runs[0], &mut question(&runs[1])).unwrap();
    assert_eq!(ready(&store, &runs[1]), Ok(true));
    // The scheduling exception never consumes the message; launch comes first.
    assert_eq!(store.pending_messages(&runs[1].id).unwrap().len(), 1);
    let delivery = store.next_delivery(&runs[1].id).unwrap().unwrap();
    store.message_delivered(&delivery).unwrap();
    assert_eq!(ready(&store, &runs[1]), Ok(false));
}

#[test]
fn waiting_dependency_with_a_message_to_someone_else_keeps_verifier_queued() {
    let (store, runs) = fixture();
    let mut third = runs[0].assignment.clone();
    third.key = "other-verifier".into();
    let plan = delegation::DelegationPlan {
        summary: "three peers".into(),
        assignments: vec![
            runs[0].assignment.clone(),
            runs[1].assignment.clone(),
            third,
        ],
        containers: vec![],
    };
    let runs = store.create("three-peer-task", "chat", &plan).unwrap();
    let other = &runs[2];
    sync(&store, &runs[0], &mut question(other)).unwrap();
    assert!(store.pending_messages(&runs[1].id).unwrap().is_empty());
    assert_eq!(ready(&store, &runs[1]), Ok(false));
    assert_eq!(store.get(&runs[1].id).unwrap().state, "queued");
    assert_eq!(ready(&store, other), Ok(true));
}

#[test]
fn waiting_dependency_without_a_message_or_with_user_message_stays_queued() {
    let (store, runs) = fixture();
    sync(&store, &runs[0], &mut snapshot("waiting-for-peer")).unwrap();
    assert_eq!(ready(&store, &runs[1]), Ok(false));
    store
        .message(
            "user-message",
            "user",
            &runs[1].id,
            &json!({"id":"user-message","source":"user","text":"start"}),
        )
        .unwrap();
    assert_eq!(ready(&store, &runs[1]), Ok(false));
}

#[test]
fn failed_dependency_is_not_released_by_its_pending_message() {
    let (store, runs) = fixture();
    sync(&store, &runs[0], &mut question(&runs[1])).unwrap();
    store
        .state(&runs[0].id, "failed", "native turn failed")
        .unwrap();
    assert_eq!(
        ready(&store, &runs[1]),
        Err("dependency implementer failed and can never complete".into())
    );
}

#[test]
fn a_peer_message_cannot_bypass_other_prerequisites_or_hide_failure() {
    let (store, mut runs) = fixture();
    sync(&store, &runs[0], &mut question(&runs[1])).unwrap();
    runs = store.list().unwrap().0;
    let mut prerequisite = runs[0].clone();
    prerequisite.id = "another-prerequisite".into();
    prerequisite.assignment.key = "another-prerequisite".into();
    prerequisite.state = "working".into();
    runs[1]
        .assignment
        .dependencies
        .push(prerequisite.assignment.key.clone());
    runs.push(prerequisite);
    let messages = store.pending_messages(&runs[1].id).unwrap();
    assert_eq!(dependency_ready(&runs, &runs[1], &messages), Ok(false));
    runs[2].state = "failed".into();
    assert_eq!(
        dependency_ready(&runs, &runs[1], &messages),
        Err("dependency another-prerequisite failed and can never complete".into())
    );
    // A still-working prerequisite before a failed one must not hide the failure.
    runs[0].state = "working".into();
    assert!(dependency_ready(&runs, &runs[1], &messages).is_err());
    runs[0].state = "waiting-for-peer".into();
    runs[2].state = "completed".into();
    assert_eq!(dependency_ready(&runs, &runs[1], &messages), Ok(true));
}

#[test]
fn messages_from_superseded_or_other_task_dependencies_do_not_release_verifier() {
    let (store, mut runs) = fixture();
    sync(&store, &runs[0], &mut question(&runs[1])).unwrap();
    runs = store.list().unwrap().0;
    let messages = store.pending_messages(&runs[1].id).unwrap();
    runs[0].state = "superseded".into();
    assert_eq!(dependency_ready(&runs, &runs[1], &messages), Ok(false));
    runs[0].state = "waiting-for-peer".into();
    runs[0].task_id = "another-task".into();
    assert_eq!(dependency_ready(&runs, &runs[1], &messages), Ok(false));
}

#[test]
fn waiting_reason_survives_multiple_syncs_and_clears_when_peer_launches() {
    let (store, runs) = fixture();
    sync(&store, &runs[0], &mut question(&runs[1])).unwrap();
    for _ in 0..3 {
        let waiting = store.get(&runs[0].id).unwrap();
        assert_eq!(waiting.state, "waiting-for-peer");
        assert_eq!(
            waiting.metadata["reason"],
            "waiting for verifier, which is queued behind this run"
        );
        assert_eq!(waiting.cursor, 1);
        // The question only appears in the first page, never in these syncs.
        sync(&store, &waiting, &mut snapshot("waiting-for-peer")).unwrap();
    }
    assert_eq!(store.events(&runs[0].id, 0).unwrap().len(), 1);
    assert!(store.claim(&runs[1].id, "runner.py").unwrap());
    sync(&store, &runs[0], &mut snapshot("waiting-for-peer")).unwrap();
    assert!(store.get(&runs[0].id).unwrap().metadata["reason"].is_null());
}

#[test]
fn question_to_dependent_verifier_launches_and_is_deliverable() {
    let (store, runs) = fixture();
    let (implementer, verifier) = (&runs[0], &runs[1]);
    store.state(&implementer.id, "working", "").unwrap();
    assert_eq!(ready(&store, verifier), Ok(false));

    // BUG8: one question reverses the wait direction. Importing it must release
    // the dependent verifier without requiring a second question or completion.
    sync(&store, implementer, &mut question(verifier)).unwrap();
    assert_eq!(ready(&store, verifier), Ok(true));
    assert!(store.claim(&verifier.id, "runner.py").unwrap());
    assert_eq!(store.get(&verifier.id).unwrap().state, "launching");
    assert!(!store.claim(&verifier.id, "runner.py").unwrap());
    let pending = store.pending_messages(&verifier.id).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["source"], implementer.id);
    assert_eq!(pending[0]["kind"], "question");

    // Exercise the real runner's launch receipt and inbox. Only tmux execution
    // is mocked: no provider-backed conversation or production journal starts.
    let script = r#"
import contextlib, io, json, sys, tempfile
from pathlib import Path
from unittest.mock import patch
import runner
value = json.load(sys.stdin)
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    assignment = value['assignment']
    assignment['workspace'] = str(root / 'hive-workspaces' / 'verifier')
    with patch.object(Path, 'home', return_value=root), patch.object(runner, 'BASE', root / 'runs'), patch.object(runner.subprocess, 'run') as tmux:
        for _ in range(2):
            with patch.object(sys, 'argv', ['runner.py', 'launch', '--run-id', assignment['id']]), patch.object(sys, 'stdin', io.StringIO(json.dumps(assignment))), contextlib.redirect_stdout(io.StringIO()):
                runner.main()
        assert tmux.call_count == 1, tmux.call_args_list
        journal = runner.Journal(runner.BASE / assignment['id'])
        journal.quiet = True
        journal.enqueue(value['message'])
        journal.enqueue(value['message'])
        inbox = journal.db.execute('SELECT payload, state FROM inbox').fetchall()
        assert len(inbox) == 1
        assert json.loads(inbox[0]['payload']) == value['message']
        assert inbox[0]['state'] == 'queued'
        journal.db.close()
"#;
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("python3")
        .args(["-c", script])
        .current_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../hive-core/src/delegation/runner"
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = json!({"assignment":delegation::remote_assignment(verifier, &runs, None),"message":pending[0]});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let delivery = store.next_delivery(&verifier.id).unwrap().unwrap();
    store.message_delivered(&delivery).unwrap();
    assert!(store.pending_messages(&verifier.id).unwrap().is_empty());
}

fn paused_with(to: &Run, kind: &str, text: &str) -> Value {
    let mut snapshot = snapshot("paused-quota");
    snapshot["metadata"]["quota"] = json!({"agent":"codex","resets_at":1_790_003_600,"message":"You've hit your usage limit."});
    snapshot["events"] = json!([{
        "id":"handoff", "seq":1, "kind":"peer",
        "payload":{"to":to.id,"kind":kind,"text":text}
    }]);
    snapshot
}

#[test]
fn paused_quota_dependency_keeps_dependents_queued_and_is_not_failed() {
    let (store, runs) = fixture();
    sync(&store, &runs[0], &mut snapshot("paused-quota")).unwrap();
    assert_eq!(store.get(&runs[0].id).unwrap().state, "paused-quota");
    assert_eq!(ready(&store, &runs[1]), Ok(false));
    // A progress message that hands nothing over does not release it either.
    sync(&store, &runs[0], &mut paused_with(&runs[1], "deployment", "Paused on quota; I will push a branch later")).unwrap();
    assert_eq!(ready(&store, &runs[1]), Ok(false));
    assert_eq!(store.get(&runs[1].id).unwrap().state, "queued");
}

#[test]
fn paused_quota_dependency_that_handed_off_a_branch_or_commit_releases_dependents() {
    for text in [
        "Implementation is on branch fix/usage-limit-pause, ready to verify",
        "Committed 3f9c2ab0 with the parser; verify it",
    ] {
        let (store, runs) = fixture();
        sync(&store, &runs[0], &mut paused_with(&runs[1], "deployment", text)).unwrap();
        assert_eq!(ready(&store, &runs[1]), Ok(true), "{text}");
    }
}

#[test]
fn a_handoff_from_a_paused_run_cannot_bypass_another_prerequisite() {
    let (store, mut runs) = fixture();
    sync(&store, &runs[0], &mut paused_with(&runs[1], "deployment", "branch: fix/x")).unwrap();
    runs = store.list().unwrap().0;
    let mut other = runs[0].clone();
    other.id = "other".into();
    other.assignment.key = "other".into();
    other.state = "paused-quota".into();
    runs[1].assignment.dependencies.push("other".into());
    runs.push(other);
    let messages = store.pending_messages(&runs[1].id).unwrap();
    assert_eq!(dependency_ready(&runs, &runs[1], &messages), Ok(false));
}

#[test]
fn launching_without_session_has_recovery_path() {
    let (store, runs) = fixture();
    let run = &runs[0];
    assert!(LIVE_STATES.contains(&"launching"));
    assert!(store.claim(&run.id, "runner.py").unwrap());
    assert_eq!(store.get(&run.id).unwrap().state, "launching");
    // retry_setup directly on RunStore is refused for launching runs
    assert!(store.retry_setup(&run.id).is_err());
    // retry_launching within timeout is refused
    assert!(store.retry_launching(&run.id, 30).is_err());
    // Past timeout (tested here with 0 timeout), retry_launching resets state to queued and clears runner_path
    store.retry_launching(&run.id, 0).unwrap();
    let recovered = store.get(&run.id).unwrap();
    assert_eq!(recovered.state, "queued");
    assert!(recovered.runner_path.is_none());
}
