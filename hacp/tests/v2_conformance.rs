//! W4 conformance vectors: the kernel and contract engine exercised as a wired
//! bilateral lifecycle. Every protocol event is a real [`Envelope`], validated
//! and recorded, and the whole exchange must match the committed golden
//! transcript `bilateral-lifecycle` — the same golden the independent peer is
//! held to in the Phase 3 exit test.

mod common;

use common::{assert_matches_golden, maybe_record_golden, Dir, Transcript};
use hacp::v2::contract::{
    Contract, ContractLimits, ContractState, Relationship, Submission, Task, Verdict,
};
use hacp::v2::envelope::{agent_urn, kinds, Envelope};
use hacp::v2::session::{Session, SessionState};
use serde_json::{json, Value};

const VOLATILE: &[&str] = &["message_id", "timestamp"];

fn urn(name: &str) -> String {
    agent_urn::mint(name).unwrap()
}

/// Record one protocol event as a validated envelope, after the session engine
/// has confirmed the author is a participant (§6.2).
fn send(
    transcript: &mut Transcript,
    session: &Session,
    from: &str,
    to: &str,
    kind: &str,
    body: Value,
) {
    session
        .authorize_author(from)
        .expect("only participants author (§6.2)");
    let envelope = Envelope::new(&session.session_id, from, to, kind, body);
    envelope.validate().expect("every recorded frame is a valid envelope");
    transcript.record(Dir::AToB, &envelope);
}

fn send_from_b(
    transcript: &mut Transcript,
    session: &Session,
    from: &str,
    to: &str,
    kind: &str,
    body: Value,
) {
    session
        .authorize_author(from)
        .expect("only participants author (§6.2)");
    let envelope = Envelope::new(&session.session_id, from, to, kind, body);
    envelope.validate().expect("every recorded frame is a valid envelope");
    transcript.record(Dir::BToA, &envelope);
}

/// Drive the full bilateral lifecycle through the engines, recording every
/// protocol event. Returns the transcript and the frozen revision digest.
fn drive_lifecycle() -> (Transcript, String) {
    let a = urn("parent-1");
    let b = urn("child-1");

    let mut transcript = Transcript::new();
    let mut session = Session::open("s-000000000001", &a, &b).unwrap();

    // Handshake (§6): open, accept, both sides declare features.
    send(&mut transcript, &session, &a, &b, kinds::SESSION_OPEN, json!({"prospective": true}));
    session.accept(&b).unwrap();
    session
        .declare_features(&a, &["supervision", "delegation", "artifact-digest", "observer-events"])
        .unwrap();
    session
        .declare_features(&b, &["delegation", "artifact-digest", "observer-events"])
        .unwrap();
    send_from_b(&mut transcript, &session, &b, &a, kinds::SESSION_FEATURES, json!({"features": ["delegation", "artifact-digest", "observer-events"]}));
    send(&mut transcript, &session, &a, &b, kinds::SESSION_FEATURES, json!({"features": ["supervision", "delegation", "artifact-digest", "observer-events"]}));
    assert_eq!(session.state, SessionState::Active);

    // Contract (§7): propose, both accept, freeze — landing in Executing with
    // no event between freeze and work, because EXECUTE is a state (§7.5).
    let mut contract = Contract::propose(
        &session,
        "c-000000000001",
        Task {
            task_id: "t-000000000001".into(),
            summary: "produce thing.txt with one line".into(),
            owner: b.clone(),
        },
        Relationship::Delegation,
        vec![urn("root-1"), a.clone()],
        ContractLimits { max_rounds: 4, max_amendments: 2 },
    )
    .unwrap();
    let terms = json!({
        "inputs": [],
        "outputs": [{"name": "thing.txt", "media_type": "text/plain", "one_line": true}],
        "acceptance": ["file has exactly one line", "line is non-empty"],
        "budget": {"max_hours": 1}
    });
    send(&mut transcript, &session, &a, &b, kinds::CONTRACT_PROPOSED, json!({"contract_id": contract.contract_id, "task_id": contract.task.task_id, "terms": terms}));
    contract.agree(&b, &terms).unwrap();
    send_from_b(&mut transcript, &session, &b, &a, kinds::CONTRACT_ACCEPTED, json!({"contract_id": contract.contract_id, "accepted": true}));
    contract.agree(&a, &terms).unwrap();
    send(&mut transcript, &session, &a, &b, kinds::CONTRACT_ACCEPTED, json!({"contract_id": contract.contract_id, "accepted": true}));
    let digest = contract.freeze(terms).unwrap();
    assert_eq!(contract.state, ContractState::Executing);
    send(&mut transcript, &session, &a, &b, kinds::CONTRACT_FROZEN, json!({"contract_id": contract.contract_id, "revision": 1, "digest": digest}));

    // An observer subscribed to lifecycle events sees the freeze and the
    // verdict, and nothing else (§6.2).
    session
        .grant_observer(&urn("auditor-1"), &[kinds::CONTRACT_FROZEN, kinds::VERIFICATION_DELIVERED])
        .unwrap();
    assert!(session.observer_receives(&urn("auditor-1"), kinds::CONTRACT_FROZEN));
    assert!(!session.observer_receives(&urn("auditor-1"), kinds::CONTRACT_PROPOSED));

    // Submission and verdict (§7.5, §9.3).
    let artifact = "urn:hacp:artifact:9f0d0e6a-0000-4000-8000-000000000001";
    contract
        .submit(
            &b,
            Submission {
                against_revision: digest.clone(),
                artifacts: vec![artifact.into()],
                evidence: vec![],
                claim: "thing.txt written with one non-empty line".into(),
            },
        )
        .unwrap();
    send_from_b(&mut transcript, &session, &b, &a, kinds::SUBMISSION_DELIVERED, json!({"contract_id": contract.contract_id, "against_revision": digest, "artifacts": [artifact], "claim": "thing.txt written with one non-empty line"}));
    contract.decide(Verdict::Accept).unwrap();
    send(&mut transcript, &session, &a, &b, kinds::VERIFICATION_DELIVERED, json!({"contract_id": contract.contract_id, "verdict": "accept"}));

    (transcript, digest)
}

#[test]
fn the_bilateral_lifecycle_matches_its_golden_transcript() {
    let (transcript, digest) = drive_lifecycle();
    assert_eq!(digest.len(), 64);
    maybe_record_golden("bilateral-lifecycle", &transcript);
    assert_matches_golden("bilateral-lifecycle", &transcript, VOLATILE);
}

#[test]
fn every_frame_is_sane_wire_traffic() {
    let (transcript, _) = drive_lifecycle();
    let rendered = transcript.render(VOLATILE).unwrap();
    // All frames speak 2.0 and stay inside one session.
    assert_eq!(
        rendered.matches("\"protocol\":\"HACP/2.0\"").count(),
        transcript.len()
    );
    assert!(rendered.contains("\"session_id\":\"s-000000000001\""));
    // The wire never saw an execute kind (§7.5): EXECUTE is a state.
    assert!(!rendered.contains("execute"), "in: {rendered}");
    // The freeze carried the revision digest, mutually known (§7.5).
    assert!(rendered.contains("\"digest\":\""));
    // Only the two participants authored.
    for line in rendered.lines() {
        let frame: Value = serde_json::from_str(line).unwrap();
        let from = frame["envelope"]["from"].as_str().unwrap();
        assert!(
            from == "urn:hacp:agent:parent-1" || from == "urn:hacp:agent:child-1",
            "stranger authored: {from}"
        );
    }
}

#[test]
fn a_rejected_negotiation_is_also_a_golden_lifecycle() {
    // The other terminal: bounded negotiation exhausted, NO_AGREEMENT recorded
    // with the transcript as evidence (§7.4).
    let a = urn("peer-x");
    let b = urn("peer-y");
    let mut session = Session::open("s-000000000002", &a, &b).unwrap();
    session.accept(&b).unwrap();
    let mut contract = Contract::propose(
        &session,
        "c-000000000002",
        Task {
            task_id: "t-2".into(),
            summary: "agree on anything".into(),
            owner: a.clone(),
        },
        Relationship::Collaboration,
        vec![],
        ContractLimits { max_rounds: 2, max_amendments: 1 },
    )
    .unwrap();
    let mut transcript = Transcript::new();
    let terms = json!({"outputs": []});
    send(&mut transcript, &session, &a, &b, kinds::SESSION_OPEN, json!({"prospective": true}));
    send(&mut transcript, &session, &a, &b, kinds::CONTRACT_PROPOSED, json!({"contract_id": contract.contract_id, "terms": terms}));
    // Round 1: an ordinary counter.
    contract.counter(&b).unwrap();
    send_from_b(&mut transcript, &session, &b, &a, kinds::CONTRACT_COUNTERED, json!({"contract_id": contract.contract_id, "round": 1}));
    // Round 2: the bound — the counter returns the exhaustion error and the
    // state lands in NoAgreement. A valid terminal, on the wire.
    let exhausted = contract.counter(&a).unwrap_err();
    assert!(exhausted.to_string().contains("NoAgreement"), "got: {exhausted}");
    assert_eq!(contract.state, ContractState::NoAgreement);
    send(&mut transcript, &session, &a, &b, kinds::CONTRACT_NO_AGREEMENT, json!({"contract_id": contract.contract_id, "rounds": 2}));
    maybe_record_golden("bilateral-no-agreement", &transcript);
    assert_matches_golden("bilateral-no-agreement", &transcript, VOLATILE);
}
