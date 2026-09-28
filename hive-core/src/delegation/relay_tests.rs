use super::*;
use crate::memory::graph::KnowledgeGraph;

fn fixture(budget: relay::Budget) -> (RunStore, Vec<Run>) {
    let graph = KnowledgeGraph::in_memory().unwrap();
    let mut store = RunStore::new(graph.shared_conn()).unwrap();
    store.1 = budget;
    let plan: DelegationPlan = serde_json::from_value(json!({"summary":"relay","assignments":
        (["a","b","c","d"].map(|key| json!({"key":key,"device":"local","agent":"codex","model":null,"workspace":"/tmp/test","objective":"test","dependencies":[],"acceptance_criteria":["test"]})))})).unwrap();
    let runs = store.create("task", "chat", &plan).unwrap();
    (store, runs)
}
fn stage(store: &RunStore, from: &Run, to: &Run, id: &str) {
    store
        .message(
            id,
            &from.id,
            &to.id,
            &json!({"id":id,"source":from.id,"kind":"answer","text":"evidence"}),
        )
        .unwrap();
}
fn exec(store: &RunStore, sql: &str) {
    store.0.lock().unwrap().execute_batch(sql).unwrap();
}

#[test]
fn identities_are_unique_persistent_and_secret_never_serializes() {
    let (store, runs) = fixture(relay::Budget::default());
    assert_ne!(runs[0].identity.public_key, runs[1].identity.public_key);
    let seed: Vec<u8> = store
        .0
        .lock()
        .unwrap()
        .query_row(
            "SELECT seed FROM delegated_relay_keys WHERE run_id=?",
            [&runs[0].id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seed.len(), 32);
    let seed_hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    let public = serde_json::to_string(&runs).unwrap();
    let assignment = serde_json::to_string(&runs[0].assignment).unwrap();
    let audit = store.audit(&runs[0].id).unwrap().to_string();
    for output in [public, assignment, audit, format!("{:?}", runs[0])] {
        assert!(!output.contains(&seed_hex));
        assert!(!output.contains(&serde_json::to_string(&seed).unwrap()));
        assert!(!output.contains("seed"));
        assert!(!output.contains("secret"));
    }
    let reopened = RunStore::new(store.0.clone()).unwrap();
    assert_eq!(
        reopened.get(&runs[0].id).unwrap().identity.public_key,
        runs[0].identity.public_key
    );
    store.state(&runs[0].id, "needs-setup", "").unwrap();
    let mut assignment = runs[0].assignment.clone();
    assignment.device = "replacement-device".into();
    let replacement = store.replace(&runs[0].id, &assignment).unwrap();
    assert_ne!(replacement.identity.public_key, runs[0].identity.public_key);
}

#[test]
fn payload_signature_routing_and_kind_tampering_never_reach_inbox() {
    for mutation in [
        "UPDATE delegated_messages SET payload=json_set(payload,'$.text','forged')",
        "UPDATE delegated_messages SET payload=json_set(payload,'$.kind','agreement')",
        "UPDATE delegated_messages SET payload=json_set(payload,'$.source','user')",
        "UPDATE delegated_messages SET payload=json_set(payload,'$.extra','injected')",
        "UPDATE delegated_messages SET payload='not json'",
        "UPDATE delegated_relay_envelopes SET signature=lower(hex(zeroblob(64)))",
        "UPDATE delegated_relay_envelopes SET envelope=json_set(envelope,'$.task_id','other')",
        "UPDATE delegated_relay_envelopes SET envelope=json_set(envelope,'$.seq',99)",
        "UPDATE delegated_relay_envelopes SET envelope='{}'",
        "DELETE FROM delegated_relay_envelopes",
    ] {
        let (store,runs) = fixture(relay::Budget::default());
        stage(&store,&runs[0],&runs[1],"message");
        exec(&store,mutation);
        assert!(store.next_delivery(&runs[1].id).unwrap().is_none(),"{mutation}");
        assert!(store.next_delivery(&runs[1].id).unwrap().is_none());
        for run in &runs[..2] {
            let run = store.get(&run.id).unwrap();
            assert_eq!(run.relay["incidents"].as_array().unwrap().len(),1);
            assert_eq!(run.relay["incidents"][0]["kind"],"integrity");
        }
        assert_eq!(store.audit(&runs[0].id).unwrap()["chain_valid"],true);
        let delivered: i64 = store.0.lock().unwrap().query_row("SELECT delivered FROM delegated_messages",[],|r|r.get(0)).unwrap();
        assert_eq!(delivered,0);
    }
}

#[test]
fn exact_replay_is_idempotent_changed_replay_is_audited_and_sequence_replay_rejected() {
    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[0], &runs[1], "one");
    let delivery = store.next_delivery(&runs[1].id).unwrap().unwrap();
    store.message_delivered(&delivery).unwrap();
    stage(&store, &runs[0], &runs[1], "one");
    assert!(store.next_delivery(&runs[1].id).unwrap().is_none());
    assert!(store
        .message(
            "one",
            &runs[0].id,
            &runs[1].id,
            &json!({"id":"one","text":"changed"})
        )
        .is_err());
    assert_eq!(
        store.get(&runs[1].id).unwrap().relay["incidents"][0]["kind"],
        "integrity"
    );
    // Simulate a replay of a previously signed sequence with the DB delivery
    // flag reset. The persistent pair cursor must independently reject it.
    exec(&store, "UPDATE delegated_messages SET delivered=0");
    assert!(store.next_delivery(&runs[1].id).unwrap().is_none());
    stage(&store, &runs[0], &runs[1], "two");
    let delivery = store.next_delivery(&runs[1].id).unwrap().unwrap();
    assert_eq!(delivery.id, "two");
    store.message_delivered(&delivery).unwrap();
}

#[test]
fn out_of_order_signed_message_is_rejected_and_pair_sequences_are_independent() {
    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[0], &runs[1], "two");
    stage(&store, &runs[0], &runs[2], "other-pair");
    exec(
        &store,
        "UPDATE delegated_relay_pairs SET delivered_seq=2 WHERE staged_seq=2",
    );
    assert!(store.next_delivery(&runs[1].id).unwrap().is_none());
    assert_eq!(
        store.get(&runs[0].id).unwrap().relay["incidents"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store.next_delivery(&runs[2].id).unwrap().unwrap().id,
        "other-pair"
    );
}

#[test]
fn run_budget_holds_without_dropping_and_releases_at_sixty_seconds() {
    let (store, runs) = fixture(relay::Budget {
        per_run: 1,
        per_task: 30,
    });
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[0], &runs[2], "two");
    let delivery = store.next_delivery_at(&runs[1].id, 1000).unwrap().unwrap();
    store.message_delivered(&delivery).unwrap();
    assert!(store.next_delivery_at(&runs[2].id, 1059).unwrap().is_none());
    assert!(store.get(&runs[0].id).unwrap().relay["held"][0]["reason"]
        .as_str()
        .unwrap()
        .starts_with("Per-run"));
    assert_eq!(store.pending_messages(&runs[2].id).unwrap().len(), 1);
    // Holds and counters survive re-opening; no dependency on in-memory state.
    let mut reopened = RunStore::new(store.0.clone()).unwrap();
    reopened.1 = store.1;
    assert!(reopened
        .next_delivery_at(&runs[2].id, 1059)
        .unwrap()
        .is_none());
    let delivery = reopened
        .next_delivery_at(&runs[2].id, 1060)
        .unwrap()
        .unwrap();
    reopened.message_delivered(&delivery).unwrap();
    assert!(reopened.get(&runs[0].id).unwrap().relay["held"]
        .as_array()
        .unwrap()
        .is_empty());
    let events: Vec<String> = reopened.audit(&runs[0].id).unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["record"]["event"].as_str().unwrap().into())
        .collect();
    assert_eq!(events.iter().filter(|e| e.as_str() == "hold").count(), 1);
    for event in ["stage", "verify", "deliver", "hold"] {
        assert!(events.iter().any(|e| e == event));
    }
}

#[test]
fn task_budget_covers_multiple_sources_but_not_other_tasks_or_human_control() {
    let (store, runs) = fixture(relay::Budget {
        per_run: 10,
        per_task: 1,
    });
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[2], &runs[3], "two");
    let delivery = store.next_delivery_at(&runs[1].id, 1000).unwrap().unwrap();
    store.message_delivered(&delivery).unwrap();
    assert!(store.next_delivery_at(&runs[3].id, 1001).unwrap().is_none());
    assert!(store.get(&runs[2].id).unwrap().relay["held"][0]["reason"]
        .as_str()
        .unwrap()
        .starts_with("Per-task"));
    store
        .message(
            "human",
            "user",
            &runs[3].id,
            &json!({"id":"human","text":"stop"}),
        )
        .unwrap();
    assert_eq!(
        store
            .next_delivery_at(&runs[3].id, 1002)
            .unwrap()
            .unwrap()
            .id,
        "human"
    );
    let plan = DelegationPlan {
        summary: "other".into(),
        assignments: vec![runs[0].assignment.clone(), runs[1].assignment.clone()],
        containers: vec![],
    };
    let others = store.create("other-task", "chat", &plan).unwrap();
    stage(&store, &others[0], &others[1], "other");
    assert_eq!(
        store
            .next_delivery_at(&others[1].id, 1003)
            .unwrap()
            .unwrap()
            .id,
        "other"
    );
    assert_eq!(
        store
            .next_delivery_at(&runs[3].id, 1060)
            .unwrap()
            .unwrap()
            .id,
        "two"
    );
}

#[test]
fn concurrent_claims_do_not_overspend_task_budget() {
    let (store, runs) = fixture(relay::Budget {
        per_run: 10,
        per_task: 1,
    });
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[2], &runs[3], "two");
    let workers: Vec<_> = [runs[1].id.clone(), runs[3].id.clone()]
        .into_iter()
        .map(|id| {
            let store = store.clone();
            std::thread::spawn(move || store.next_delivery_at(&id, 1000).unwrap().is_some())
        })
        .collect();
    let admitted = workers
        .into_iter()
        .filter_map(|t| t.join().unwrap().then_some(()))
        .count();
    assert_eq!(admitted, 1);
}

#[test]
fn lease_prevents_overtaking_and_crash_retry_is_bounded_and_verified_again() {
    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[0], &runs[1], "two");
    let first = store.next_delivery_at(&runs[1].id, 1000).unwrap().unwrap();
    assert!(store.next_delivery_at(&runs[1].id, 1001).unwrap().is_none());
    let retry = store.next_delivery_at(&runs[1].id, 1300).unwrap().unwrap();
    assert_eq!(retry.id, "one");
    assert_ne!(retry.token, first.token);
    assert!(store.message_delivered(&first).is_err());
    store.message_delivered(&retry).unwrap();
    let second = store.next_delivery_at(&runs[1].id, 1301).unwrap().unwrap();
    assert_eq!(second.id, "two");
    store.delivery_failed(&second).unwrap();
    exec(&store,"UPDATE delegated_messages SET payload=json_set(payload,'$.text','tampered') WHERE id='two'");
    assert!(store.next_delivery_at(&runs[1].id, 1302).unwrap().is_none());
}

#[test]
fn append_only_audit_detects_modified_missing_middle_tail_and_all_rows() {
    for mutation in [
        "UPDATE delegated_relay_audit SET record='{}' WHERE position=2",
        "DELETE FROM delegated_relay_audit WHERE position=2",
        "DELETE FROM delegated_relay_audit WHERE position=(SELECT max(position) FROM delegated_relay_audit)",
        "DELETE FROM delegated_relay_audit",
    ] {
        let (store,runs)=fixture(relay::Budget::default());
        stage(&store,&runs[0],&runs[1],"one");
        let delivery=store.next_delivery(&runs[1].id).unwrap().unwrap();
        store.message_delivered(&delivery).unwrap();
        assert_eq!(store.audit(&runs[0].id).unwrap()["chain_valid"],true);
        assert!(store.0.lock().unwrap().execute_batch(mutation).is_err());
        exec(&store,"DROP TRIGGER relay_audit_no_update; DROP TRIGGER relay_audit_no_delete;");
        exec(&store,mutation);
        assert_eq!(store.audit_full(&runs[0].id).unwrap()["chain_valid"],false,"{mutation}");
        // A failed full verification is sticky for subsequent cheap polls.
        assert_eq!(store.audit(&runs[0].id).unwrap()["chain_valid"],false,"{mutation}");
    }
}

#[test]
fn migration_does_not_resign_unsigned_messages_and_sync_cannot_erase_incidents() {
    let (store, runs) = fixture(relay::Budget::default());
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO delegated_messages(id,source,destination,payload) VALUES('legacy',?,?,?)",
            params![
                runs[0].id,
                runs[1].id,
                json!({"id":"legacy","text":"old"}).to_string()
            ],
        )
        .unwrap();
    let reopened = RunStore::new(store.0.clone()).unwrap();
    assert!(reopened.next_delivery(&runs[1].id).unwrap().is_none());
    for run in &runs[..2] {
        reopened.sync(&run.id,&json!({"metadata":{"state":"working","relay":{"incidents":[]}},"events":[],"approvals":[]})).unwrap();
        assert_eq!(
            reopened.get(&run.id).unwrap().relay["incidents"][0]["kind"],
            "integrity"
        );
    }
}

/// Appends `count` synthetic audit rows in one transaction (fast seeding).
fn seed_chain(store: &RunStore, run: &Run, count: usize) {
    let mut db = store.0.lock().unwrap();
    let tx = db.transaction().unwrap();
    let envelope = relay::Envelope {
        id: "seed".into(),
        task_id: run.task_id.clone(),
        source: run.id.clone(),
        destination: "elsewhere".into(),
        kind: "message".into(),
        text_digest: String::new(),
        seq: 1,
        staged_at: String::new(),
    };
    for _ in 0..count {
        relay::audit(&tx, "stage", &envelope, json!({}), 1_000).unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn incremental_audit_reads_only_new_rows_of_a_large_chain() {
    let (store, runs) = fixture(relay::Budget::default());
    seed_chain(&store, &runs[0], 20_000);
    let full = store.audit_full(&runs[0].id).unwrap();
    assert_eq!(full["chain_valid"], true);
    assert_eq!(full["verification"], "full");
    assert_eq!(full["verified_rows"], 20_000);
    assert_eq!(full["total_entries"], 20_000);
    assert_eq!(full["entries"].as_array().unwrap().len(), 200);
    assert_eq!(full["entries"][199]["record"]["position"], 20_000);
    // Default call: nothing new since the checkpoint, so nothing is re-hashed.
    let cached = store.audit(&runs[0].id).unwrap();
    assert_eq!(cached["verification"], "incremental");
    assert_eq!(cached["verified_rows"], 0);
    assert_eq!(cached["chain_valid"], true);
    // One new message adds exactly one stage row; only that row is read.
    stage(&store, &runs[0], &runs[1], "fresh");
    let next = store.audit(&runs[1].id).unwrap();
    assert_eq!(next["verified_rows"], 1);
    assert_eq!(next["verified_through"], 20_001);
    assert_eq!(next["chain_valid"], true);
    // Only the requested run's entries are returned.
    assert_eq!(next["total_entries"], 1);
    assert_eq!(next["entries"][0]["record"]["message_id"], "fresh");
}

#[test]
fn full_audit_detects_old_rows_and_incremental_detects_new_rows() {
    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[0], &runs[1], "two");
    assert_eq!(store.audit(&runs[0].id).unwrap()["chain_valid"], true);
    exec(&store, "DROP TRIGGER relay_audit_no_update; DROP TRIGGER relay_audit_no_delete;");
    // Tampering with a row the checkpoint already covers is found by a full pass.
    exec(&store, "UPDATE delegated_relay_audit SET record=json_set(record,'$.seq',9) WHERE position=1");
    assert_eq!(store.audit_full(&runs[0].id).unwrap()["chain_valid"], false);

    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    stage(&store, &runs[0], &runs[1], "two");
    assert_eq!(store.audit(&runs[0].id).unwrap()["chain_valid"], true);
    exec(&store, "DROP TRIGGER relay_audit_no_update; DROP TRIGGER relay_audit_no_delete;");
    exec(&store, "DELETE FROM delegated_relay_audit WHERE position=1");
    assert_eq!(store.audit_full(&runs[0].id).unwrap()["chain_valid"], false);

    // A row appended after the checkpoint is verified by the default call.
    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    assert_eq!(store.audit(&runs[0].id).unwrap()["chain_valid"], true);
    stage(&store, &runs[0], &runs[1], "two");
    exec(&store, "DROP TRIGGER relay_audit_no_update;");
    exec(&store, "UPDATE delegated_relay_audit SET record=json_set(record,'$.seq',9) WHERE position=2");
    let report = store.audit(&runs[0].id).unwrap();
    assert_eq!(report["verification"], "incremental");
    assert_eq!(report["verified_rows"], 1);
    assert_eq!(report["chain_valid"], false);
}

#[test]
fn repeated_conflicting_message_records_one_incident_and_one_reject() {
    let (store, runs) = fixture(relay::Budget::default());
    stage(&store, &runs[0], &runs[1], "one");
    for _ in 0..5 {
        let error = store
            .message("one", &runs[0].id, &runs[1].id, &json!({"id":"one","text":"changed"}))
            .unwrap_err();
        assert!(error.is::<MessageRejected>());
    }
    let incidents: i64 = store.0.lock().unwrap().query_row(
        "SELECT count(*) FROM delegated_relay_incidents WHERE message_id='one'", [], |r| r.get(0)).unwrap();
    assert_eq!(incidents, 1);
    let rejects: i64 = store.0.lock().unwrap().query_row(
        "SELECT count(*) FROM delegated_relay_audit WHERE json_extract(record,'$.event')='reject'", [], |r| r.get(0)).unwrap();
    assert_eq!(rejects, 1);
    assert_eq!(store.audit_full(&runs[0].id).unwrap()["chain_valid"], true);
}
