//! HACP/1.1 conformance vectors.
//!
//! These exercise the properties `spec/HACP.md` §15 says an implementation must have.
//! They are deliberately written against **JSON**, not against Rust constructors, so
//! that they document the wire format rather than this crate's API: an implementation in
//! another language can port them by copying the literals.
//!
//! Passing these makes an implementation *a* reference implementation. It does not make
//! it the only correct one.

use hacp::contract::*;
use hacp::envelope::*;
use hacp::evolution::*;
use hacp::report::*;
use hacp::state::*;
use hacp::topology::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// §5 — Envelope and forward compatibility
// ---------------------------------------------------------------------------

/// The single most important property in the protocol: a message from a future minor
/// version, with a kind and body fields this build has never heard of, must survive
/// round-tripping. §5 forbids rejecting it.
#[test]
fn unknown_kinds_and_body_fields_round_trip() {
    let raw = json!({
        "protocol": "HACP/1.9",
        "message_id": "m-test-1",
        "run_id": "run-test",
        "from": "urn:hacp:agent:a-3f0c",
        "to": "urn:hacp:coordinator:c1",
        "kind": "future.unknown.kind",
        "in_reply_to": "m-old",
        "timestamp": "2026-09-04T10:00:00Z",
        "body": {"known": 1, "deeply": {"unknown": [1, 2, 3]}}
    });

    let env: Envelope = serde_json::from_value(raw).expect("a future minor must parse");
    assert_eq!(env.kind.as_str(), "future.unknown.kind");
    assert!(!env.kind.is_registered(), "not in the v1.1 registry");
    assert!(env.protocol_ok(), "same major must be accepted");
    assert!(env.validate().is_ok(), "an unknown kind is not a shape error");
    assert_eq!(env.body["deeply"]["unknown"][2], json!(3));

    let back = serde_json::to_value(&env).unwrap();
    assert_eq!(back["kind"], json!("future.unknown.kind"));
    assert_eq!(back["body"]["deeply"]["unknown"], json!([1, 2, 3]));
}

#[test]
fn version_gate_accepts_same_major_only() {
    assert!(urn::protocol_supported("HACP/1.0"), "1.0 messages remain valid in 1.1");
    assert!(urn::protocol_supported(PROTOCOL));
    assert!(urn::protocol_supported("HACP/1.42"));
    assert!(!urn::protocol_supported("HACP/2.0"));
    assert!(!urn::protocol_supported("HTTP/1.1"));
    assert!(!urn::protocol_supported("HACP/x.y"));
    assert!(!urn::protocol_supported("HACP/1"), "a minor is required");
    assert!(!urn::protocol_supported("HACP/1.x"), "a malformed minor is not a future minor");
}

#[test]
fn major_mismatch_is_rejected_with_the_supported_version() {
    let mut env = Envelope::new("run-1", urn::agent("a", "3f0c"), urn::coordinator("c"), kinds::HELLO, json!({}));
    env.protocol = "HACP/2.0".into();
    match env.validate() {
        Err(EnvelopeError::ProtocolMismatch { found, expected }) => {
            assert_eq!(found, "HACP/2.0");
            assert_eq!(expected, PROTOCOL);
        }
        other => panic!("expected a protocol mismatch, got {other:?}"),
    }
}

#[test]
fn kinds_requiring_a_causal_link_are_enforced() {
    for kind in kinds::REQUIRES_IN_REPLY_TO {
        let env = Envelope::new("run-1", urn::arbiter("x"), urn::agent("a", "3f0c"), *kind, json!({}));
        assert!(
            matches!(env.validate(), Err(EnvelopeError::MissingInReplyTo(_))),
            "{kind} must require in_reply_to"
        );
        assert!(env.with_in_reply_to("m-prev").validate().is_ok());
    }
}

// ---------------------------------------------------------------------------
// §3 — Vendor-neutral naming
// ---------------------------------------------------------------------------

#[test]
fn actor_urns_parse_and_carry_no_vendor_identity() {
    let a = urn::agent("a", "3f0c");
    assert_eq!(a, "urn:hacp:agent:a-3f0c");
    assert_eq!(urn::parse_agent(&a), Some(("a".into(), "3f0c".into())));

    // A role id may itself contain a hyphen; the run-short suffix is the unambiguous
    // part, so the split is on the last one.
    let multi = urn::agent("job-store", "8a41");
    assert_eq!(urn::parse_agent(&multi), Some(("job-store".into(), "8a41".into())));

    assert!(urn::is_agent(&a));
    assert!(urn::is_coordinator(&urn::coordinator("c1")));
    assert!(urn::is_arbiter(&urn::arbiter("c1")));
    assert!(urn::is_broadcast(urn::ALL));
    assert!(!urn::is_agent(&urn::coordinator("c1")));
}

#[test]
fn malformed_urns_are_rejected() {
    assert_eq!(urn::parse_agent("urn:hacp:agent:-3f0c"), None, "empty role");
    assert_eq!(urn::parse_agent("urn:hacp:agent:a-"), None, "empty run-short");
    assert_eq!(urn::parse_agent("urn:hacp:agent:noseparator"), None);
    assert_eq!(urn::parse_agent("urn:hacp:coordinator:x"), None);
    assert!(!urn::is_neutral("agent-7"), "a bare name is not a HACP actor");
    assert!(!urn::is_neutral("urn:hacp:agent:"), "no role or run");
}

#[test]
fn broadcast_is_addressable_but_not_a_sender() {
    let mut env = Envelope::new("run-1", urn::coordinator("c"), urn::ALL, kinds::RUN_STARTED, json!({}));
    assert!(env.validate().is_ok());
    assert!(env.is_broadcast());

    env.from = urn::ALL.into();
    assert!(
        matches!(env.validate(), Err(EnvelopeError::BroadcastNotAllowed("from"))),
        "nothing may claim to be everyone"
    );
}

// ---------------------------------------------------------------------------
// §6 — Peer traffic is private to its endpoints
// ---------------------------------------------------------------------------

#[test]
fn peer_messages_reach_only_their_endpoints_and_the_arbiter() {
    let (a, b, c) = (urn::agent("a", "1"), urn::agent("b", "1"), urn::agent("c", "1"));
    let env = Envelope::new("run-1", &a, &b, kinds::PEER_QUESTION, json!({"about": "x", "text": "?"}));

    assert!(env.kind.is_peer());
    assert!(env.deliverable_to(&b), "the addressee receives it");
    assert!(env.deliverable_to(&a), "the sender sees its own traffic");
    assert!(env.deliverable_to(&urn::arbiter("x")), "the arbiter observes for audit");
    assert!(!env.deliverable_to(&c), "an uninvolved worker must not see peer traffic");
}

#[test]
fn a_broadcast_peer_message_still_reaches_only_endpoints() {
    // Addressing peer traffic to everyone is a contradiction. §6's privacy rule wins:
    // an implementation that resolved it the other way would leak every negotiation.
    let a = urn::agent("a", "1");
    let env = Envelope::new("run-1", &a, urn::ALL, kinds::PEER_ANSWER, json!({"text": "..."}));
    assert!(!env.deliverable_to(&urn::agent("c", "1")));
}

#[test]
fn non_peer_broadcasts_reach_everyone() {
    let env = Envelope::new("run-1", urn::coordinator("c"), urn::ALL, kinds::CONTRACT_FROZEN, json!({}));
    assert!(env.deliverable_to(&urn::agent("a", "1")));
    assert!(env.deliverable_to(&urn::agent("z", "1")));
}

// ---------------------------------------------------------------------------
// §4 — Topology is derived from the agent count
// ---------------------------------------------------------------------------

#[test]
fn topology_follows_the_agent_count() {
    assert_eq!(Topology::for_agent_count(1), Topology::Solo);
    assert_eq!(Topology::for_agent_count(2), Topology::Peer);
    assert_eq!(Topology::for_agent_count(3), Topology::Federated);
    assert_eq!(Topology::for_agent_count(9), Topology::Federated);

    assert!(!Topology::Peer.requires_arbiter(), "two agents may negotiate alone");
    assert!(Topology::Federated.requires_arbiter(), "three or more cannot");
    assert!(!Topology::Solo.allows_peer_traffic());
    assert!(Topology::Peer.allows_peer_traffic());
}

// ---------------------------------------------------------------------------
// §7 — Formation
// ---------------------------------------------------------------------------

fn decomposition_vector() -> serde_json::Value {
    json!({
        "decomposition_id": "d-1",
        "goal": "Accept a job from a CLI, store it, process it asynchronously, report status.",
        "analysis": "Two components sharing one interface.",
        "components": [
            {"component_id": "ingest", "description": "Accept and persist a job.",
             "required_capabilities": ["file-write", "shell"],
             "produces": ["job-store"], "consumes": []},
            {"component_id": "worker", "description": "Process jobs and report status.",
             "required_capabilities": ["file-write", "shell"],
             "produces": ["runner"], "consumes": ["job-store"]}
        ],
        "roles": [
            {"role_id": "a", "components": ["ingest"], "required_capabilities": ["file-write", "shell"]},
            {"role_id": "b", "components": ["worker"], "required_capabilities": ["file-write", "shell"]}
        ],
        "agent_count": 2,
        "topology": "peer",
        "rationale": "Two components with a single shared interface: two agents suffice, and with only two parties a direct peer negotiation needs no arbiter."
    })
}

#[test]
fn a_well_formed_decomposition_validates() {
    let d: TaskDecomposition = serde_json::from_value(decomposition_vector()).unwrap();
    d.validate().expect("the vector must be conformant");
    assert_eq!(d.agent_count, 2);
    assert_eq!(d.required_capabilities(), vec!["file-write", "shell"]);
}

#[test]
fn agent_count_must_match_the_roles_it_claims() {
    let mut v = decomposition_vector();
    v["agent_count"] = json!(3);
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    assert!(matches!(d.validate(), Err(DecompositionError::CountMismatch { .. })));
}

#[test]
fn topology_must_be_the_one_the_count_requires() {
    // Claiming `federated` for two agents would smuggle in a mandatory arbiter; claiming
    // `peer` for four would remove a required one. Both are rejected.
    let mut v = decomposition_vector();
    v["topology"] = json!("federated");
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    assert!(matches!(d.validate(), Err(DecompositionError::TopologyMismatch { .. })));
}

#[test]
fn every_component_belongs_to_exactly_one_role() {
    let mut v = decomposition_vector();
    v["roles"][0]["components"] = json!(["ingest", "worker"]);
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    assert!(matches!(
        d.validate(),
        Err(DecompositionError::MultiplyAssignedComponent { .. })
    ));

    // Add a component no role claims, rather than emptying a role. Emptying one makes
    // the input idle-role AND unassigned-component at once, and then whichever check
    // runs first decides what the test appears to be about.
    let mut v = decomposition_vector();
    v["components"].as_array_mut().unwrap().push(json!({
        "component_id": "reporter", "description": "Report status.",
        "required_capabilities": ["shell"], "produces": ["status"], "consumes": []
    }));
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    match d.validate() {
        Err(DecompositionError::UnassignedComponent(c)) => assert_eq!(c, "reporter"),
        other => panic!("expected UnassignedComponent, got {other:?}"),
    }
}

#[test]
fn a_consumed_artifact_must_be_produced_by_something() {
    let mut v = decomposition_vector();
    v["components"][1]["consumes"] = json!(["nonexistent"]);
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    assert!(matches!(d.validate(), Err(DecompositionError::UnproducedArtifact { .. })));
}

#[test]
fn the_rationale_is_required() {
    // §7 requires the decomposition to say *why* this many agents. A count with no
    // reasoning is exactly the hardcoded team size the section exists to forbid.
    let mut v = decomposition_vector();
    v["rationale"] = json!("   ");
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    assert!(matches!(d.validate(), Err(DecompositionError::MissingRationale)));
}

// ---------------------------------------------------------------------------
// §8 — Manifests are provenance, not a gate
// ---------------------------------------------------------------------------

#[test]
fn a_minimal_manifest_is_conformant() {
    // Admission must not be gated on manifest contents (§8), so the emptiest possible
    // manifest has to parse and be usable.
    let m: CapabilityManifest = serde_json::from_value(json!({"agent": "urn:hacp:agent:a-1"})).unwrap();
    assert!(m.capabilities.is_empty());
    assert_eq!(m.declared_by, "adapter-default");
    assert!(!m.writes_own_report(), "absent report-json means expect a synthesized report");

    let m: CapabilityManifest = serde_json::from_value(json!({
        "agent": "urn:hacp:agent:a-1",
        "capabilities": ["file-write", "report-json"],
        "declared_by": "configured",
        "runtime_note": "provenance extras round-trip"
    }))
    .unwrap();
    assert!(m.writes_own_report());
    assert!(m.declares("file-write"));
    assert_eq!(m.extra["runtime_note"], json!("provenance extras round-trip"));
}

// ---------------------------------------------------------------------------
// §9 — Contract
// ---------------------------------------------------------------------------

fn contract_vector() -> serde_json::Value {
    json!({
        "contract_id": "c-1",
        "goal": "textool",
        "artifacts": [{
            "artifact_id": "textool-core",
            "produced_by": "urn:hacp:agent:a-3f0c",
            "path": "crates/textool-core",
            "format": "rust-crate",
            "interface_files": ["src/lib.rs"],
            "symbols": ["pub fn slugify"],
            "examples": [{"input": "Hello World", "output": "hello-world"}],
            "check": {"kind": "command", "command": "cargo build -p textool-core"}
        }],
        "dependencies": [{"consumer": "urn:hacp:agent:b-8a41", "consumes": "textool-core"}],
        "integration": {"command": "cargo test --workspace"},
        "workspace_rules": ["agents write only inside their own workspace"]
    })
}

#[test]
fn the_spec_example_contract_round_trips_and_validates() {
    let c: InterfaceContract = serde_json::from_value(contract_vector()).unwrap();
    c.validate().expect("the spec's own example must be conformant");

    assert_eq!(c.version, 1, "version defaults to 1");
    assert_eq!(c.artifact("textool-core").unwrap().format, ArtifactFormat::RustCrate);
    assert_eq!(c.produced_by("urn:hacp:agent:a-3f0c").len(), 1);
    assert_eq!(c.consumed_by("urn:hacp:agent:b-8a41").len(), 1);
    assert!(c.artifact("missing").is_none());

    let back: InterfaceContract = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(back.dependencies, c.dependencies);
}

#[test]
fn a_minimal_draft_parses_so_validation_is_what_rejects_it() {
    // A model-drafted contract may omit every optional collection. It must parse, so
    // that a bad draft is reported as a contract error rather than a parse failure the
    // drafting agent cannot act on.
    let c: InterfaceContract = serde_json::from_value(json!({
        "contract_id": "c-2",
        "goal": "g",
        "artifacts": [{"artifact_id": "a1", "produced_by": "urn:hacp:agent:a-1",
                       "path": "p", "format": "file"}]
    }))
    .unwrap();
    assert!(c.dependencies.is_empty() && c.integration.is_none());
    c.validate().expect("minimal but valid");
}

#[test]
fn json_artifacts_require_a_schema_and_others_forbid_one() {
    let mut v = contract_vector();
    v["artifacts"][0]["format"] = json!("json");
    let c: InterfaceContract = serde_json::from_value(v).unwrap();
    assert!(matches!(c.validate(), Err(ContractError::Artifact { .. })), "json needs a schema");

    let mut v = contract_vector();
    v["artifacts"][0]["schema"] = json!({"type": "object"});
    let c: InterfaceContract = serde_json::from_value(v).unwrap();
    assert!(matches!(c.validate(), Err(ContractError::Artifact { .. })), "non-json forbids one");
}

#[test]
fn duplicate_artifact_ids_and_dangling_dependencies_are_rejected() {
    let mut v = contract_vector();
    let dup = v["artifacts"][0].clone();
    v["artifacts"].as_array_mut().unwrap().push(dup);
    let c: InterfaceContract = serde_json::from_value(v).unwrap();
    assert!(matches!(c.validate(), Err(ContractError::DuplicateArtifact(_))));

    let mut v = contract_vector();
    v["dependencies"][0]["consumes"] = json!("nope");
    let c: InterfaceContract = serde_json::from_value(v).unwrap();
    assert!(matches!(c.validate(), Err(ContractError::UnknownDependency(_))));
}

#[test]
fn a_dependency_cycle_is_rejected() {
    // A produces x and consumes y; B produces y and consumes x. Nothing can be built
    // first. 1.0 permitted this by omission.
    let c: InterfaceContract = serde_json::from_value(json!({
        "contract_id": "c-cycle",
        "goal": "g",
        "artifacts": [
            {"artifact_id": "x", "produced_by": "urn:hacp:agent:a-1", "path": "x", "format": "file"},
            {"artifact_id": "y", "produced_by": "urn:hacp:agent:b-1", "path": "y", "format": "file"}
        ],
        "dependencies": [
            {"consumer": "urn:hacp:agent:a-1", "consumes": "y"},
            {"consumer": "urn:hacp:agent:b-1", "consumes": "x"}
        ]
    }))
    .unwrap();
    assert!(matches!(c.validate(), Err(ContractError::DependencyCycle(_))));
}

#[test]
fn examples_must_be_strings_in_v1() {
    let mut v = contract_vector();
    v["artifacts"][0]["examples"] = json!([{"input": 42, "output": "x"}]);
    let c: InterfaceContract = serde_json::from_value(v).unwrap();
    assert!(matches!(c.validate(), Err(ContractError::Artifact { .. })));
}

// ---------------------------------------------------------------------------
// §9 — Canonical interface digest
// ---------------------------------------------------------------------------

#[test]
fn the_interface_digest_is_order_sensitive_and_content_sensitive() {
    let spec: ArtifactSpec = serde_json::from_value(json!({
        "artifact_id": "a", "produced_by": "urn:hacp:agent:a-1", "path": "p",
        "format": "file", "interface_files": ["one.txt", "two.txt"]
    }))
    .unwrap();

    let read = |p: &str| -> Result<Vec<u8>, std::convert::Infallible> {
        Ok(match p {
            "one.txt" => b"alpha".to_vec(),
            "two.txt" => b"beta".to_vec(),
            _ => Vec::new(),
        })
    };
    let digest = spec.interface_digest(read).unwrap();
    assert!(digest.starts_with("sha256:"));

    // Deterministic across calls: the freeze check depends on it entirely.
    assert_eq!(digest, spec.interface_digest(read).unwrap());

    // Listed order is part of the definition, not an implementation detail.
    let mut swapped = spec.clone();
    swapped.interface_files = vec!["two.txt".into(), "one.txt".into()];
    assert_ne!(digest, swapped.interface_digest(read).unwrap());

    // Any content change moves the digest — this is the whole freeze mechanism.
    let changed = |p: &str| -> Result<Vec<u8>, std::convert::Infallible> {
        Ok(if p == "one.txt" { b"ALPHA".to_vec() } else { b"beta".to_vec() })
    };
    assert_ne!(digest, spec.interface_digest(changed).unwrap());
}

#[test]
fn an_artifact_with_no_frozen_interface_has_a_constant_digest() {
    let spec: ArtifactSpec = serde_json::from_value(json!({
        "artifact_id": "a", "produced_by": "urn:hacp:agent:a-1", "path": "p", "format": "file"
    }))
    .unwrap();
    let read = |_: &str| -> Result<Vec<u8>, std::convert::Infallible> { Ok(Vec::new()) };
    // sha256 of the empty input. Declaring no frozen interface means nothing can drift.
    assert_eq!(
        spec.interface_digest(read).unwrap(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

// ---------------------------------------------------------------------------
// §9.1 / §9.2 — Amendment and evolution
// ---------------------------------------------------------------------------

#[test]
fn strictly_additive_amendments_are_distinguishable() {
    let additive: ContractAmendment = serde_json::from_value(json!({
        "target_version": 1, "rationale": "the consumer needs a status query",
        "additions": [{"artifact_id": "status", "produced_by": "urn:hacp:agent:a-1",
                       "path": "s", "format": "file"}]
    }))
    .unwrap();
    assert!(additive.is_strictly_additive(), "auto-acceptance is allowed here");

    let mutating: ContractAmendment = serde_json::from_value(json!({
        "target_version": 1, "rationale": "rename the entry point",
        "changes": [{"artifact_id": "textool-core", "change": "rename slugify", "reason": "clarity"}]
    }))
    .unwrap();
    assert!(!mutating.is_strictly_additive(), "a mutation must be adjudicated");
}

#[test]
fn consumers_of_a_changed_artifact_are_exactly_the_impact_audience() {
    // §9.2: interface.impacted goes to the consumers of a changed artifact and to no one
    // else. Getting this audience wrong either leaks negotiation or silently strands a
    // consumer on a stale interface.
    let c: InterfaceContract = serde_json::from_value(json!({
        "contract_id": "c-3", "goal": "g",
        "artifacts": [
            {"artifact_id": "api", "produced_by": "urn:hacp:agent:a-1", "path": "api", "format": "file"},
            {"artifact_id": "ui", "produced_by": "urn:hacp:agent:b-1", "path": "ui", "format": "file"}
        ],
        "dependencies": [
            {"consumer": "urn:hacp:agent:b-1", "consumes": "api"},
            {"consumer": "urn:hacp:agent:c-1", "consumes": "api"}
        ]
    }))
    .unwrap();
    c.validate().unwrap();

    let mut consumers = c.consumers_of("api");
    consumers.sort_unstable();
    assert_eq!(consumers, vec!["urn:hacp:agent:b-1", "urn:hacp:agent:c-1"]);
    assert!(c.consumers_of("ui").is_empty(), "nothing consumes ui, so nobody is told");
}

#[test]
fn evolution_bodies_round_trip() {
    let req: ChangeRequest = serde_json::from_value(json!({
        "artifact_id": "api", "change": "add a status field", "reason": "the consumer cannot poll without it",
        "breaking": true
    }))
    .unwrap();
    assert!(req.breaking, "advisory only — the arbiter decides");

    let impacted: InterfaceImpacted = serde_json::from_value(json!({
        "artifact_id": "api", "was_digest": "sha256:aa", "now_digest": "sha256:bb",
        "what_changed": "added `status` to the job record",
        "action_required": "read the new field; existing calls are unaffected"
    }))
    .unwrap();
    assert_ne!(impacted.was_digest, impacted.now_digest);
}

// ---------------------------------------------------------------------------
// §10 / §11 — Reports and verdicts
// ---------------------------------------------------------------------------

#[test]
fn a_report_round_trips_and_a_synthesized_one_says_so() {
    let r: CompletionReport = serde_json::from_value(json!({
        "report_id": "r-1", "agent": "urn:hacp:agent:a-3f0c", "outcome": "success",
        "summary": "done", "contract_status": "satisfied",
        "artifacts": [{"artifact_id": "t", "path": "p", "sha256": "sha256:ab", "exists": true}],
        "diffstat": {"files_changed": 2, "insertions": 10, "deletions": 1},
        "tests": {"command": "cargo test", "passed": 3, "failed": 0, "output": "ok"},
        "evidence": {"log_path": "agents/a/agent.log", "session": "run-x-a"},
        "duration_secs": 42, "source": "agent"
    }))
    .unwrap();
    assert_eq!(r.outcome, Outcome::Success);
    assert!(r.is_self_reported());

    let f = CompletionReport::fallback("urn:hacp:agent:b-8a41");
    assert_eq!(f.source, ReportSource::AdapterSynthesized);
    assert_eq!(f.contract_status, ContractStatus::NotReported);
    assert_eq!(f.outcome, Outcome::Blocked, "an adapter must not guess success");
}

#[test]
fn a_verdict_derives_its_own_pass_flag() {
    // C2 applies to the verdict too: `passed` is computed from the checks, never taken
    // as a separate claim that could contradict them.
    let checks = vec![
        CheckResult::pass(check::name(check::EXISTENCE, "api"), "crates/api exists"),
        CheckResult::fail(check::name(check::BUILD_PROBE, "api"), "exit 101\n...tail..."),
    ];
    let v = VerificationResult::new("urn:hacp:agent:a-1", "r-1", checks);
    assert!(!v.passed);
    assert_eq!(v.failed_checks().len(), 1);
    assert_eq!(v.failed_checks()[0].name, "build-probe:api");

    let ok = VerificationResult::new("urn:hacp:agent:a-1", "r-2", vec![CheckResult::pass("x", "")]);
    assert!(ok.passed);
}

#[test]
fn a_rework_request_carries_the_failed_checks_verbatim() {
    // A worker cannot repair what it is only told "failed" (§11.1).
    let v = VerificationResult::new(
        "urn:hacp:agent:a-1",
        "r-1",
        vec![CheckResult::fail("symbols:api", "`submit_job` not found in crates/api")],
    );
    let req = ReworkRequested {
        report_id: v.report_id.clone(),
        failed_checks: v.failed_checks(),
        round: 1,
        rounds_remaining: 0,
    };
    let back: ReworkRequested = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert!(back.failed_checks[0].evidence.contains("submit_job"));
}

#[test]
fn a_run_summary_carries_opaque_evidence_refs() {
    // 1.0 typed this as the reference implementation's own session struct. It is now
    // opaque, which is what lets a non-Rust implementation emit a conformant summary.
    let s: RunSummary = serde_json::from_value(json!({
        "run_id": "run-1", "goal": "g", "final_state": "completed",
        "agents": [{"agent": "urn:hacp:agent:a-3f0c", "outcome": "success",
                    "verdict_passed": true, "checks_passed": 7, "checks_total": 7,
                    "report_source": "agent", "rework_rounds": 1}],
        "integration": {"name": "integration", "passed": true, "evidence": "ok"},
        "evidence": [{"kind": "session", "locator": "tmux:run-1-a"},
                     {"kind": "log", "locator": "agents/a/agent.log"}],
        "duration_secs": 300
    }))
    .unwrap();
    assert_eq!(s.final_state, RunState::Completed);
    assert_eq!(s.evidence.len(), 2);
    assert_eq!(s.agents[0].rework_rounds, 1);
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[test]
fn the_happy_path_is_legal_end_to_end() {
    use RunState::*;
    let path = [
        Formation, Planning, Drafted, Amending, Drafted, Frozen, Working, Reporting,
        Verifying, Integrating, Completed,
    ];
    for w in path.windows(2) {
        assert!(w[0].can_transition_to(w[1]), "{} -> {} must be legal", w[0], w[1]);
    }
}

#[test]
fn post_freeze_amendment_and_rework_are_legal() {
    use RunState::*;
    // §9.2: the controlled door out of freeze, and back.
    assert!(Working.can_transition_to(Amending));
    assert!(Frozen.can_transition_to(Amending));
    assert!(Amending.can_transition_to(Frozen));
    // §11.1: a failed verdict is repairable.
    assert!(Verifying.can_transition_to(Reworking));
    assert!(Reworking.can_transition_to(Verifying));
}

#[test]
fn every_non_terminal_state_can_fail_and_no_terminal_state_can_move() {
    for s in RunState::ALL {
        if s.is_terminal() {
            assert!(!s.can_transition_to(RunState::Failed), "{s} is terminal");
            assert!(s.successors().is_empty());
        } else {
            for end in [RunState::Failed, RunState::Aborted, RunState::TimedOut] {
                assert!(s.can_transition_to(end), "{s} must be able to reach {end}");
            }
        }
    }
}

#[test]
fn completion_is_reachable_only_through_integration() {
    // "Completed" is a claim that verification happened. Reaching it from anywhere else
    // would let a run declare success without ever running the contract.
    for s in RunState::ALL {
        if *s == RunState::Integrating {
            assert!(s.can_transition_to(RunState::Completed));
        } else {
            assert!(!s.can_transition_to(RunState::Completed), "{s} must not complete directly");
        }
    }
}

#[test]
fn the_post_freeze_states_are_exactly_the_ones_a_digest_check_applies_to() {
    use RunState::*;
    for s in [Frozen, Working, Reporting, Verifying, Reworking, Integrating] {
        assert!(s.is_post_freeze(), "{s} is under a frozen contract");
    }
    for s in [Formation, Planning, Drafted, Amending, Completed, Failed] {
        assert!(!s.is_post_freeze(), "{s} is not");
    }
}

#[test]
fn run_limits_bound_every_loop_in_the_protocol() {
    let l = RunLimits::default();
    assert!(l.max_rounds > 0 && l.max_amendments > 0 && l.heartbeat_secs > 0);
    // Zero rework rounds is conformant: it means "fail honestly on first failure".
    let strict = RunLimits { max_rework_rounds: 0, ..l };
    assert_eq!(strict.max_rework_rounds, 0);
}

// ── Vectors added after the first bindings were written ─────────────────────
//
// Each of these pins a defect that five parallel implementations found and the
// original 39 vectors missed. They are written against literals precisely
// because the bugs were invisible to tests that asserted through the types.

#[test]
fn contract_status_is_hyphenated_on_the_wire() {
    // §10 spells these `not-started` / `not-reported`, and Display agrees, but the
    // derive said snake_case — so the only form that actually crosses the wire was
    // the one nothing else used. Asserting through the type cannot see this; a
    // round-trip through the wrong spelling round-trips perfectly.
    assert_eq!(
        serde_json::to_value(ContractStatus::NotReported).unwrap(),
        json!("not-reported")
    );
    assert_eq!(
        serde_json::to_value(ContractStatus::NotStarted).unwrap(),
        json!("not-started")
    );
    let parsed: ContractStatus = serde_json::from_value(json!("not-reported")).unwrap();
    assert_eq!(parsed, ContractStatus::NotReported);

    // Display and the wire form are the same string, so neither can drift alone.
    assert_eq!(ContractStatus::NotStarted.to_string(), "not-started");
    assert_eq!(
        serde_json::to_value(ContractStatus::NotStarted).unwrap(),
        json!(ContractStatus::NotStarted.to_string())
    );
}

#[test]
fn every_outcome_and_status_variant_serializes_to_its_display_form() {
    // The bug above was one variant's spelling, not a rule anyone stated. Pin the
    // rule instead, so a variant added later cannot reintroduce it.
    for s in [
        ContractStatus::Satisfied,
        ContractStatus::Deviated,
        ContractStatus::Partial,
        ContractStatus::NotStarted,
        ContractStatus::NotReported,
    ] {
        assert_eq!(serde_json::to_value(s).unwrap(), json!(s.to_string()), "{s}");
    }
    for o in [
        Outcome::Success,
        Outcome::Partial,
        Outcome::Failure,
        Outcome::Blocked,
    ] {
        assert_eq!(serde_json::to_value(o).unwrap(), json!(o.to_string()), "{o}");
    }
}

#[test]
fn a_team_of_nobody_is_not_a_decomposition() {
    // Zero roles satisfied every other rule vacuously: count 0 == roles 0, and the
    // topology required for 0 agents is Solo. The emptiest possible answer to the
    // run's central question validated cleanly.
    let mut v = decomposition_vector();
    v["components"] = json!([]);
    v["roles"] = json!([]);
    v["agent_count"] = json!(0);
    v["topology"] = json!("solo");
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    assert!(matches!(
        d.validate(),
        Err(DecompositionError::EmptyTeam)
    ));
}

#[test]
fn a_role_with_nothing_to_do_is_rejected() {
    // agent_count is the one number §7 exists to keep honest, so a role holding no
    // component inflates exactly what the section protects. The component walk only
    // goes components->roles and can never see a role nothing points back at.
    let mut v = decomposition_vector();
    let idle = json!({
        "role_id": "idle",
        "title": "Idle",
        "responsibility": "none",
        "components": [],
        "required_capabilities": []
    });
    v["roles"].as_array_mut().unwrap().push(idle);
    v["agent_count"] = json!(v["roles"].as_array().unwrap().len());
    v["topology"] = json!("federated");
    let d: TaskDecomposition = serde_json::from_value(v).unwrap();
    match d.validate() {
        Err(DecompositionError::IdleRole(r)) => assert_eq!(r, "idle"),
        other => panic!("expected IdleRole, got {other:?}"),
    }
}
