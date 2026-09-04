//! The Phase 3 exit test: a full bilateral lifecycle between the **reference**
//! implementation (`hacp::v2`) and an **independently implemented peer** that
//! imports neither HIVE nor this crate — `interop/peer-python/peer.py`, built
//! from the normative spec, the committed canonical schemas, and the golden
//! transcripts alone (ADR-0001 §3, spec §14 "Independence").
//!
//! The transport is the file edge (§12.1): the reference writes envelopes to
//! `a-out/`, the peer writes to `b-out/`, each side records its view of the
//! exchange, and the two views must agree frame-for-frame under canonical
//! comparison. The peer independently recomputes the freeze digest (§7.5) and
//! enforces the evidence-over-signals rule on the verdict it receives (§9.4);
//! the reference performs a *real* verification of the peer's artifact before
//! accepting it.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Dir, Transcript};
use hacp::v2::contract::{
    Contract, ContractLimits, ContractState, Relationship, Task, Verdict as ContractVerdict,
};
use hacp::v2::envelope::{agent_urn, kinds, Envelope, PROTOCOL};
use hacp::v2::session::Session;
use hacp::v2::verification::{Check, Verification};
use serde_json::{json, Value};

const VOLATILE: &[&str] = &["message_id", "timestamp"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(1).unwrap().to_path_buf()
}

fn edge_dir() -> PathBuf {
    std::env::temp_dir().join(format!("hacp-interop-{}", std::process::id()))
}

/// Wait until a file matching `prefix` appears in `dir`, then parse it.
fn await_frame(dir: &Path, already_seen: &mut Vec<String>) -> Option<Envelope> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json") && !already_seen.contains(n))
            .collect();
        names.sort();
        if let Some(name) = names.first() {
            let raw = std::fs::read_to_string(dir.join(name)).expect("readable frame");
            already_seen.push(name.clone());
            let envelope: Envelope = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("peer frame {name} is not an envelope: {e}"));
            envelope
                .validate()
                .unwrap_or_else(|e| panic!("peer frame {name} is not valid: {e}"));
            return Some(envelope);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn write_frame(dir: &Path, counter: &mut u64, envelope: &Envelope) {
    *counter += 1;
    let name = format!("{:03}-{}.json", *counter, envelope.kind);
    let raw = serde_json::to_string(envelope).expect("envelope serializes");
    std::fs::write(dir.join(name), raw).expect("write frame");
}

#[test]
fn an_independent_peer_interoperates_over_the_file_edge() {
    let edge = edge_dir();
    let _ = std::fs::remove_dir_all(&edge);
    for d in ["a-out", "b-out", "workspace"] {
        std::fs::create_dir_all(edge.join(d)).expect("edge dirs");
    }

    // The independent peer: Python stdlib, spec + schemas only.
    let mut peer = std::process::Command::new("python3")
        .arg(repo_root().join("interop/peer-python/peer.py"))
        .arg("--edge-dir").arg(&edge)
        .arg("--schemas").arg(repo_root().join("hacp/spec/schemas"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 available");
    let peer_started = Instant::now();

    let a = agent_urn::mint("parent-1").unwrap();
    let b = agent_urn::mint("child-1").unwrap();
    let session_id = "s-000000000003".to_string();
    let a_out = edge.join("a-out");
    let b_out = edge.join("b-out");

    let mut reference = Transcript::new();
    let mut sent = 0u64;
    let mut seen_from_peer: Vec<String> = Vec::new();

    let mut send = |reference: &mut Transcript, session: &Session, from: &str, to: &str, kind: &str, body: Value| {
        session.authorize_author(from).expect("reference authors as a participant");
        let envelope = Envelope::new(&session_id, from, to, kind, body);
        envelope.validate().unwrap();
        write_frame(&a_out, &mut sent, &envelope);
        reference.record(Dir::AToB, &envelope);
    };

    // §6: handshake.
    let mut session = Session::open(&session_id, &a, &b).unwrap();
    send(&mut reference, &session, &a, &b, kinds::SESSION_OPEN, json!({"prospective": true}));
    let peer_features = await_frame(&b_out, &mut seen_from_peer).expect("peer declares features");
    assert_eq!(peer_features.kind.as_str(), kinds::SESSION_FEATURES);
    session.accept(&b).unwrap();
    session.declare_features(&a, &["supervision", "delegation", "artifact-digest", "observer-events"]).unwrap();
    session.declare_features(&b, &["delegation", "artifact-digest", "observer-events"]).unwrap();
    reference.record(Dir::BToA, &peer_features);
    send(&mut reference, &session, &a, &b, kinds::SESSION_FEATURES,
         json!({"features": ["supervision", "delegation", "artifact-digest", "observer-events"]}));

    // §7: propose, both accept, freeze.
    let mut contract = Contract::propose(
        &session,
        "c-000000000003",
        Task {
            task_id: "t-000000000003".into(),
            summary: "produce thing.txt with one line".into(),
            owner: b.clone(),
        },
        Relationship::Delegation,
        vec![agent_urn::mint("root-1").unwrap(), a.clone()],
        ContractLimits { max_rounds: 4, max_amendments: 2 },
    )
    .unwrap();
    let terms = json!({
        "inputs": [],
        "outputs": [{"name": "thing.txt", "media_type": "text/plain", "one_line": true}],
        "acceptance": ["file has exactly one line", "line is non-empty"],
        "budget": {"max_hours": 1}
    });
    send(&mut reference, &session, &a, &b, kinds::CONTRACT_PROPOSED,
         json!({"contract_id": contract.contract_id, "task_id": contract.task.task_id, "terms": terms}));
    let peer_accepted = await_frame(&b_out, &mut seen_from_peer).expect("peer accepts");
    assert_eq!(peer_accepted.kind.as_str(), kinds::CONTRACT_ACCEPTED);
    contract.agree(&b, &terms).unwrap();
    reference.record(Dir::BToA, &peer_accepted);
    contract.agree(&a, &terms).unwrap();
    send(&mut reference, &session, &a, &b, kinds::CONTRACT_ACCEPTED,
         json!({"contract_id": contract.contract_id, "accepted": true}));
    let digest = contract.freeze(terms.clone()).unwrap();
    assert_eq!(contract.state, ContractState::Executing);
    send(&mut reference, &session, &a, &b, kinds::CONTRACT_FROZEN,
         json!({"contract_id": contract.contract_id, "revision": 1, "digest": digest}));

    // The peer recomputes the digest itself, executes (silently — EXECUTE is a
    // state), and submits an artifact manifest.
    let submission_env = await_frame(&b_out, &mut seen_from_peer).expect("peer submits");
    assert_eq!(submission_env.kind.as_str(), kinds::SUBMISSION_DELIVERED);
    reference.record(Dir::BToA, &submission_env);
    let body = &submission_env.body;
    assert_eq!(body["against_revision"].as_str(), Some(digest.as_str()),
        "the submission answers the frozen revision");
    let info = &body["artifacts_info"][0];
    let artifact_id = info["artifact_id"].as_str().unwrap().to_string();
    let claimed_digest = info["digest"].as_str().unwrap().to_string();

    // REAL verification (§9.3): open the artifact, check the digest and the
    // acceptance criteria, then decide.
    let artifact_bytes = std::fs::read(edge.join(info["location"].as_str().unwrap()))
        .expect("the artifact exists where its manifest says");
    let actual_digest = hacp::v2::canon::digest_canonical(&String::from_utf8_lossy(&artifact_bytes));
    let one_line = artifact_bytes.iter().filter(|b| **b == b'\n').count() == 1
        && !artifact_bytes.is_empty();
    let size_ok = info["size"].as_u64() == Some(artifact_bytes.len() as u64);
    let checks = vec![
        Check { name: "digest-matches".into(), passed: actual_digest == claimed_digest, detail: "sha256 of the artifact bytes".into() },
        Check { name: "file-has-one-line".into(), passed: one_line, detail: "newline count".into() },
        Check { name: "size-matches".into(), passed: size_ok, detail: "manifest size".into() },
    ];
    contract.submit(
        &b,
        hacp::v2::contract::Submission {
            against_revision: digest.clone(),
            artifacts: vec![artifact_id.clone()],
            evidence: vec![],
            claim: body["claim"].as_str().unwrap().to_string(),
        },
    )
    .unwrap();
    let verdict = if checks.iter().all(|c| c.passed) {
        ContractVerdict::Accept
    } else {
        ContractVerdict::Reject
    };
    let verification = Verification::decide(
        "v-0000000000000003",
        &a,
        "c-000000000003",
        &digest,
        vec![artifact_id.clone()],
        vec![],
        checks.clone(),
        verdict.clone(),
        vec![],
        vec![],
    )
    .expect("an accept with artifacts and passing checks is lawful (§9.4)");
    send(&mut reference, &session, &a, &b, kinds::VERIFICATION_DELIVERED,
         json!({"contract_id": contract.contract_id, "verdict": "accept", "artifacts": [artifact_id],
                "checks": checks, "reasons": []}));
    contract.decide(verdict).unwrap();
    assert_eq!(contract.state, ContractState::Settled);

    // The peer finishes, having enforced §9.4 on what it received.
    let output = peer.wait_timeout(Duration::from_secs(30)).expect("peer exits");
    assert_eq!(
        output.code(),
        Some(0),
        "the independent peer completed the lifecycle"
    );
    assert!(peer_started.elapsed() < Duration::from_secs(120));

    // §14 Independence, mechanically: the peer's view and the reference's view
    // are the same exchange, frame for frame, under canonical comparison.
    let peer_view = Transcript::load(&edge.join("peer-transcript.jsonl")).expect("peer transcript");
    reference
        .compare(&peer_view, VOLATILE)
        .expect("both sides saw the same exchange");

    // And the peer's independently computed digest agrees with the reference's.
    let peer_digest = std::fs::read_to_string(edge.join("peer-digest.txt")).unwrap();
    assert_eq!(peer_digest, digest, "§7.5 preimage agreement across implementations");

    // The reference's own frames still speak the protocol the spec defines.
    let rendered = reference.render(VOLATILE).unwrap();
    assert!(rendered.contains(PROTOCOL));
    assert!(!rendered.contains("execute"), "EXECUTE never appeared on the wire (§7.5)");

    let _ = std::fs::remove_dir_all(&edge);
}

/// Wait-helper with a timeout for `std::process::Child`.
trait WaitTimeout {
    fn wait_timeout(&mut self, timeout: Duration) -> Option<std::process::ExitStatus>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = self.kill();
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return None,
            }
        }
    }
}
