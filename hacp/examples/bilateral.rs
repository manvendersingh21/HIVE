//! No Hive, model account, network, or async runtime is needed.
//! Run: cargo run -p hacp --example bilateral
use hacp::v2::{
    canon, Artifact, Check, Contract, ContractLimits, ContractState, Envelope, Relationship,
    Session, Submission, Task, Verdict, Verification,
};
use serde_json::json;
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let requester = "urn:hacp:agent:requester";
    let worker = "urn:hacp:agent:worker";
    let mut session = Session::open("s-example", requester, worker)?;
    let open = Envelope::new(
        &session.session_id,
        requester,
        worker,
        "session.open",
        json!({"prospective": true}),
    );
    open.validate()?;
    println!("{}", canon::canonical_json(&serde_json::to_value(&open)?)?);
    // A real adapter transports this JSON, authenticates the remote identity,
    // validates the session/from/to tuple and deduplicates before dispatch.
    session.accept(worker)?;

    let mut contract = Contract::propose(
        &session,
        "c-example",
        Task {
            task_id: "t-example".into(),
            summary: "produce exactly ready plus a newline".into(),
            owner: worker.into(),
        },
        Relationship::Collaboration,
        vec![],
        ContractLimits {
            max_rounds: 3,
            max_amendments: 2,
        },
    )?;
    let terms = json!({"outputs": ["result.txt"], "acceptance": ["bytes equal ready\n"]});
    contract.agree(requester, &terms)?;
    contract.agree(worker, &terms)?;
    let revision = contract.freeze(terms)?;

    // An actual adapter would ask its chosen CLI to do the work. Here both
    // peers run deterministically in-process, with an in-memory artifact store.
    let produced = b"ready\n";
    let artifact = Artifact::new(
        &format!("urn:hacp:artifact:{}", uuid::Uuid::new_v4()),
        "text/plain",
        &format!("{:x}", Sha256::digest(produced)),
        produced.len() as u64,
        worker,
        &contract.task.task_id,
        &contract.contract_id,
        &revision,
        "memory:result.txt",
    )?;
    contract.submit(
        worker,
        Submission {
            against_revision: revision.clone(),
            artifacts: vec![artifact.artifact_id.clone()],
            evidence: vec![],
            claim: "result is available in the artifact store".into(),
        },
    )?;

    // The verifier fetches the bytes and measures them independently of the
    // producer's claim. A digest matches bytes, not the truth of a claim.
    let fetched: &[u8] = produced;
    let checks = vec![
        Check {
            name: "digest".into(),
            passed: format!("{:x}", Sha256::digest(fetched)) == artifact.digest,
            detail: "SHA-256 recomputed from fetched bytes".into(),
        },
        Check {
            name: "size".into(),
            passed: fetched.len() as u64 == artifact.size,
            detail: "byte count compared with manifest".into(),
        },
        Check {
            name: "exact-content".into(),
            passed: fetched == b"ready\n",
            detail: "compared with the frozen acceptance criterion".into(),
        },
    ];
    let verdict = if checks.iter().all(|c| c.passed) {
        Verdict::Accept
    } else {
        Verdict::Reject
    };
    let verification = Verification::decide(
        "v-000000000001",
        requester,
        &contract.contract_id,
        &revision,
        vec![artifact.artifact_id],
        vec![],
        checks,
        verdict,
        vec![],
        vec![],
    )?;
    contract.apply_verification(&verification)?;
    assert_eq!(contract.state, ContractState::Settled);
    session.close(requester, "verified and settled")?;
    println!("Settled: exact content, size and digest verified; session closed.");
    Ok(())
}
