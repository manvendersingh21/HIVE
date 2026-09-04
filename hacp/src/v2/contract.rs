//! The bilateral contract engine — `spec/HACP-2.0-draft.md` §7.
//!
//! Every contract binds exactly two participants (§7, ADR-0001 §2); the type
//! says so. The state machine below is §7.3 made executable, including the two
//! amendments that make 2.0 what it is: **EXECUTE is a state entered implicitly
//! on freeze** — `freeze()` lands the contract in `Executing` with no event in
//! between — and **`NoAgreement` is a valid terminal** for exhausted
//! negotiation and exhausted amendments alike.
//!
//! Naming note, mapped to the §7.3 diagram: `Agreed` is the pre-freeze ACCEPTED
//! box (both participants have accepted the terms); `Settled` is the
//! post-verification ACCEPTED box (the verdict). Two different facts deserve
//! two different names even where the diagram reused one.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canon;
use super::session::Session;

/// Who the contract binds and how (§7.2): the machine is identical; the
/// difference is the authority and escalation semantics attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Relationship {
    /// Peers under no authority relationship.
    Collaboration,
    /// Parent to child, carrying a CapabilityGrant and an escalation path (§8).
    Delegation,
}

/// A unit of work with an owner and a completion claim (§7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    /// Deployment-unique task identifier.
    pub task_id: String,
    /// One honest sentence about what "done" means.
    pub summary: String,
    /// The agent accountable for the work.
    pub owner: String,
}

/// Negotiation bounds (§7.4), carried from 1.1 because silence must not consent:
/// reaching a bound without agreement is `NoAgreement`, never an implicit freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContractLimits {
    /// Maximum PROPOSE/COUNTER rounds before negotiation expires.
    pub max_rounds: u64,
    /// Maximum amendments after the first freeze.
    pub max_amendments: u64,
}

/// The contract lifecycle (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ContractState {
    /// Proposed; awaiting response.
    Proposed,
    /// Counterproposal in flight; the bounded loop (§7.4).
    Countered,
    /// Both participants accepted; freeze may happen (the §7.3 ACCEPTED box).
    Agreed,
    /// A frozen revision exists and work may proceed. Entered implicitly on
    /// freeze (§7.5) and re-entered on rework; this is EXECUTE.
    Executing,
    /// An amendment is being negotiated against the frozen revision (§7.6).
    Amending,
    /// A submission was delivered and awaits a verdict (§7.5).
    Verifying,
    /// Accepted after verification — the §7.3 post-verify ACCEPTED box. Terminal.
    Settled,
    /// Terminal: refused after verification, or failed post-freeze exit (§7.7).
    Rejected,
    /// Terminal: a participant withdrew, pre-freeze only (§7.7).
    Withdrawn,
    /// Terminal: bounds exhausted without agreement, in negotiation or
    /// amendment (§7.4, §7.6). A valid outcome, not a protocol failure.
    NoAgreement,
}

/// One immutable frozen revision of the contract's interface (§7.5–§7.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Revision {
    /// 1 for the initial freeze, N+1 for each accepted amendment.
    pub number: u64,
    /// The interface as negotiated: inputs, outputs, dependencies, acceptance
    /// criteria, budget. Free-form JSON; canonicalizable by construction.
    pub content: Value,
    /// SHA-256 over the canonical form of `{contract_id, revision, content}` —
    /// so two revisions of one contract can never collide by content reuse,
    /// and one revision of two contracts never collides either.
    pub digest: String,
}

/// A submission under a frozen revision (§7.5): artifacts by reference,
/// evidence by reference, and the completion claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Submission {
    /// The revision digest this submission answers. Must match the current
    /// frozen revision — a submission against a stale revision is refused.
    pub against_revision: String,
    /// Artifact identifiers (§9.1), never embedded content.
    pub artifacts: Vec<String>,
    /// Evidence identifiers (§9.2).
    pub evidence: Vec<String>,
    /// The performing participant's claim of completion.
    pub claim: String,
}

/// The verdict a verifier reaches (§9.3); the contract engine applies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Accept: the contract settles.
    Accept,
    /// Rework: back to `Executing` with a scope note.
    Rework { scope: String },
    /// Reject: terminal failure.
    Reject,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ContractError {
    #[error("{who:?} is not a party to contract {}", .contract)]
    NotAParty { who: String, contract: String },
    #[error("cannot {action} from state {state:?}")]
    IllegalTransition { action: &'static str, state: ContractState },
    #[error("negotiation bound reached: {rounds} of {max} rounds — NoAgreement (§7.4)")]
    RoundsExhausted { rounds: u64, max: u64 },
    #[error("amendment bound reached: {amendments} of {max} — NoAgreement (§7.6)")]
    AmendmentsExhausted { amendments: u64, max: u64 },
    #[error("a delegation must declare its escalation path (§8.3)")]
    DelegationNeedsEscalationPath,
    #[error("limits must allow at least one round and one amendment")]
    BadLimits,
    #[error("submission answers revision {found:?} but the frozen revision is {expected:?}")]
    StaleRevision { found: String, expected: String },
    #[error("both participants must agree before freeze; missing {missing:?}")]
    NotBothAgreed { missing: String },
    #[error("terms are not canonicalizable (§5.1): {0}")]
    NotCanonical(String),
}

/// The bilateral contract (§7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Contract {
    pub contract_id: String,
    pub task: Task,
    pub relationship: Relationship,
    /// Exactly two participants (§7), matching the session the contract formed in.
    pub participants: [String; 2],
    /// The declared escalation path (§8.3): the parent chain. Mandatory for
    /// delegations, optional for collaborations.
    pub escalation_path: Vec<String>,
    pub limits: ContractLimits,
    pub state: ContractState,
    /// Rounds spent in the current negotiation loop.
    pub rounds: u64,
    /// Amendments accepted since the first freeze.
    pub amendments: u64,
    /// Participants recorded as having accepted the current terms.
    agreed_by: BTreeSet<String>,
    /// Every frozen revision, in order; history is never rewritten (§7.6).
    pub revisions: Vec<Revision>,
    /// The submission awaiting a verdict, if any.
    pub pending_submission: Option<Submission>,
    /// The rework scope when a verdict sent the contract back.
    pub rework_scope: Option<String>,
}

impl Contract {
    /// Propose a contract in a session (§7). The participants are the session's
    /// two participants — a contract between anyone else is not this session's
    /// contract, and the engine will not build it.
    pub fn propose(
        session: &Session,
        contract_id: &str,
        task: Task,
        relationship: Relationship,
        escalation_path: Vec<String>,
        limits: ContractLimits,
    ) -> Result<Self, ContractError> {
        if limits.max_rounds < 1 || limits.max_amendments < 1 {
            return Err(ContractError::BadLimits);
        }
        if relationship == Relationship::Delegation && escalation_path.is_empty() {
            return Err(ContractError::DelegationNeedsEscalationPath);
        }
        if !session.participants.contains(&task.owner) {
            return Err(ContractError::NotAParty {
                who: task.owner.clone(),
                contract: contract_id.to_string(),
            });
        }
        Ok(Self {
            contract_id: contract_id.to_string(),
            task,
            relationship,
            participants: session.participants.clone(),
            escalation_path,
            limits,
            state: ContractState::Proposed,
            rounds: 0,
            amendments: 0,
            agreed_by: BTreeSet::new(),
            revisions: Vec::new(),
            pending_submission: None,
            rework_scope: None,
        })
    }

    fn authorize(&self, who: &str) -> Result<(), ContractError> {
        if self.participants.contains(&who.to_string()) {
            Ok(())
        } else {
            Err(ContractError::NotAParty {
                who: who.to_string(),
                contract: self.contract_id.clone(),
            })
        }
    }

    /// Counter the current terms (§7.3–§7.4). Each counter consumes a round;
    /// exhausting `max_rounds` lands in `NoAgreement` — the valid terminal.
    pub fn counter(&mut self, by: &str) -> Result<(), ContractError> {
        self.authorize(by)?;
        if !matches!(self.state, ContractState::Proposed | ContractState::Countered) {
            return Err(ContractError::IllegalTransition {
                action: "counter",
                state: self.state,
            });
        }
        self.rounds += 1;
        // A counter resets consensus: terms changed under the acceptors' feet.
        self.agreed_by.clear();
        if self.rounds >= self.limits.max_rounds {
            self.state = ContractState::NoAgreement;
            return Err(ContractError::RoundsExhausted {
                rounds: self.rounds,
                max: self.limits.max_rounds,
            });
        }
        self.state = ContractState::Countered;
        Ok(())
    }

    /// Accept the current terms (§7.3). The contract reaches `Agreed` only when
    /// both participants have accepted — unilateral acceptance is not agreement.
    pub fn agree(&mut self, by: &str, terms: &Value) -> Result<(), ContractError> {
        self.authorize(by)?;
        if !matches!(
            self.state,
            ContractState::Proposed | ContractState::Countered | ContractState::Agreed
        ) {
            return Err(ContractError::IllegalTransition {
                action: "agree",
                state: self.state,
            });
        }
        // Terms must canonicalize; a digest will be taken of them at freeze.
        canon::canonical_json(terms)
            .map_err(|e| ContractError::NotCanonical(e.to_string()))?;
        self.agreed_by.insert(by.to_string());
        if self.agreed_by.len() == 2 {
            self.state = ContractState::Agreed;
        } else {
            self.state = ContractState::Proposed;
        }
        Ok(())
    }

    /// Freeze the agreed terms as revision N (§7.5). Landing state is
    /// `Executing`: EXECUTE is entered implicitly, with no event in between —
    /// that is amendment 4, executable.
    pub fn freeze(&mut self, terms: Value) -> Result<String, ContractError> {
        if self.state != ContractState::Agreed {
            return Err(ContractError::IllegalTransition {
                action: "freeze",
                state: self.state,
            });
        }
        let missing: String = self
            .participants
            .iter()
            .find(|p| !self.agreed_by.contains(*p))
            .cloned()
            .unwrap_or_default();
        if !missing.is_empty() {
            return Err(ContractError::NotBothAgreed { missing });
        }
        let number = self.revisions.len() as u64 + 1;
        let digest = revision_digest(&self.contract_id, number, &terms)?;
        self.revisions.push(Revision {
            number,
            content: terms,
            digest: digest.clone(),
        });
        self.agreed_by.clear();
        self.state = ContractState::Executing;
        Ok(digest)
    }

    /// The digest of the current frozen revision, if one exists.
    pub fn frozen_digest(&self) -> Option<&str> {
        self.revisions.last().map(|r| r.digest.as_str())
    }

    /// Withdraw pre-freeze only (§7.7). A post-freeze exit is failure under the
    /// contract, not withdrawal, and travels by verdict or escalation.
    pub fn withdraw(&mut self, by: &str) -> Result<(), ContractError> {
        self.authorize(by)?;
        if !matches!(
            self.state,
            ContractState::Proposed | ContractState::Countered | ContractState::Agreed
        ) {
            return Err(ContractError::IllegalTransition {
                action: "withdraw",
                state: self.state,
            });
        }
        self.state = ContractState::Withdrawn;
        Ok(())
    }

    /// Expire negotiation explicitly (deadline, abandonment): terminal
    /// `NoAgreement` from any pre-freeze state (§7.4).
    pub fn expire_negotiation(&mut self) -> Result<(), ContractError> {
        if !matches!(
            self.state,
            ContractState::Proposed | ContractState::Countered | ContractState::Agreed
        ) {
            return Err(ContractError::IllegalTransition {
                action: "expire negotiation",
                state: self.state,
            });
        }
        self.state = ContractState::NoAgreement;
        Ok(())
    }

    /// Deliver a submission (§7.5). Must answer the current frozen revision;
    /// a stale revision digest is refused, because it would be a submission
    /// against terms that no longer exist.
    pub fn submit(&mut self, by: &str, submission: Submission) -> Result<(), ContractError> {
        self.authorize(by)?;
        if self.state != ContractState::Executing {
            return Err(ContractError::IllegalTransition {
                action: "submit",
                state: self.state,
            });
        }
        let expected = self.frozen_digest().unwrap_or_default().to_string();
        if submission.against_revision != expected {
            return Err(ContractError::StaleRevision {
                found: submission.against_revision,
                expected,
            });
        }
        self.pending_submission = Some(submission);
        self.state = ContractState::Verifying;
        Ok(())
    }

    /// Apply a verdict (§7.3, §9.3). The verifier's record is the verifier's
    /// business (§9); the engine takes the outcome.
    pub fn decide(&mut self, verdict: Verdict) -> Result<(), ContractError> {
        if self.state != ContractState::Verifying {
            return Err(ContractError::IllegalTransition {
                action: "apply a verdict",
                state: self.state,
            });
        }
        match verdict {
            Verdict::Accept => {
                self.pending_submission = None;
                self.rework_scope = None;
                self.state = ContractState::Settled;
            }
            Verdict::Rework { scope } => {
                self.rework_scope = Some(scope);
                self.pending_submission = None;
                self.state = ContractState::Executing;
            }
            Verdict::Reject => {
                self.pending_submission = None;
                self.rework_scope = None;
                self.state = ContractState::Rejected;
            }
        }
        Ok(())
    }

    /// Propose an amendment (§7.6): `Executing → Amending`, negotiated against
    /// the frozen revision.
    pub fn propose_amendment(&mut self, by: &str) -> Result<(), ContractError> {
        self.authorize(by)?;
        if self.state != ContractState::Executing {
            return Err(ContractError::IllegalTransition {
                action: "propose an amendment",
                state: self.state,
            });
        }
        self.state = ContractState::Amending;
        self.agreed_by.clear();
        self.rounds = 0;
        Ok(())
    }

    /// Decide an amendment (§7.6). Both accepting freezes revision N+1 and
    /// returns its digest; a refusal returns to `Executing` with the contract
    /// unchanged — a declined amendment is not a dispute. Round exhaustion is
    /// terminal `NoAgreement`.
    pub fn decide_amendment(&mut self, by: &str, accept: bool, terms: Option<Value>) -> Result<Option<String>, ContractError> {
        self.authorize(by)?;
        if self.state != ContractState::Amending {
            return Err(ContractError::IllegalTransition {
                action: "decide an amendment",
                state: self.state,
            });
        }
        if !accept {
            self.state = ContractState::Executing;
            return Ok(None);
        }
        self.agreed_by.insert(by.to_string());
        if self.agreed_by.len() < 2 {
            return Ok(None); // awaiting the other participant
        }
        self.amendments += 1;
        if self.amendments > self.limits.max_amendments {
            self.state = ContractState::NoAgreement;
            return Err(ContractError::AmendmentsExhausted {
                amendments: self.amendments,
                max: self.limits.max_amendments,
            });
        }
        let terms = terms.ok_or(ContractError::IllegalTransition {
            action: "accept an amendment without terms",
            state: self.state,
        })?;
        let number = self.revisions.len() as u64 + 1;
        let digest = revision_digest(&self.contract_id, number, &terms)?;
        self.revisions.push(Revision {
            number,
            content: terms,
            digest: digest.clone(),
        });
        self.agreed_by.clear();
        self.state = ContractState::Executing;
        Ok(Some(digest))
    }
}

/// Revision digest (§7.5): SHA-256 over the canonical form of the revision
/// envelope — contract identity and revision number included, so history is
/// tamper-evident by construction.
fn revision_digest(contract_id: &str, number: u64, terms: &Value) -> Result<String, ContractError> {
    canon::digest_of(&serde_json::json!({
        "contract_id": contract_id,
        "revision": number,
        "content": terms,
    }))
    .map_err(|e| ContractError::NotCanonical(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::envelope::agent_urn;
    use crate::v2::session::Session;

    fn urn(name: &str) -> String {
        agent_urn::mint(name).unwrap()
    }

    fn setup() -> (Session, Contract) {
        let mut session = Session::open("s-1", &urn("parent"), &urn("child")).unwrap();
        session.accept(&urn("child")).unwrap();
        let contract = Contract::propose(
            &session,
            "c-1",
            Task {
                task_id: "t-1".into(),
                summary: "write the thing".into(),
                owner: urn("child"),
            },
            Relationship::Delegation,
            vec![urn("root"), urn("parent")],
            ContractLimits {
                max_rounds: 2,
                max_amendments: 1,
            },
        )
        .unwrap();
        (session, contract)
    }

    fn agreed_terms() -> Value {
        serde_json::json!({"outputs": [{"name": "thing.txt"}], "budget": {"max_hours": 2}})
    }

    fn drive_to_executing(contract: &mut Contract) -> String {
        contract.agree(&urn("child"), &agreed_terms()).unwrap();
        contract.agree(&urn("parent"), &agreed_terms()).unwrap();
        assert_eq!(contract.state, ContractState::Agreed);
        let digest = contract.freeze(agreed_terms()).unwrap();
        assert_eq!(contract.state, ContractState::Executing, "EXECUTE is implicit on freeze");
        digest
    }

    #[test]
    fn a_delegation_without_an_escalation_path_is_refused() {
        let (session, _) = &setup();
        assert!(matches!(
            Contract::propose(
                session,
                "c-2",
                Task {
                    task_id: "t".into(),
                    summary: "s".into(),
                    owner: urn("child"),
                },
                Relationship::Delegation,
                vec![],
                ContractLimits { max_rounds: 1, max_amendments: 1 },
            ),
            Err(ContractError::DelegationNeedsEscalationPath)
        ));
    }

    #[test]
    fn both_participants_must_agree_before_freeze() {
        let (_, mut c) = setup();
        c.agree(&urn("child"), &agreed_terms()).unwrap();
        // One acceptance is not agreement: the contract is still Proposed, and
        // freeze from Proposed is an illegal transition.
        assert!(matches!(
            c.freeze(agreed_terms()),
            Err(ContractError::IllegalTransition { action: "freeze", .. })
        ));
        c.agree(&urn("parent"), &agreed_terms()).unwrap();
        assert_eq!(c.state, ContractState::Agreed);
    }

    #[test]
    fn a_counter_resets_consensus_and_exhaustion_is_no_agreement() {
        let (_, mut c) = setup();
        c.agree(&urn("child"), &agreed_terms()).unwrap();
        // Parent counters: whatever was agreed is void (§7.4).
        c.counter(&urn("parent")).unwrap();
        assert_eq!(c.state, ContractState::Countered);
        assert!(c.agreed_by.is_empty());
        // Second counter exhausts max_rounds = 2.
        let err = c.counter(&urn("child")).unwrap_err();
        assert!(matches!(err, ContractError::RoundsExhausted { .. }));
        assert_eq!(c.state, ContractState::NoAgreement, "a valid terminal (§7.4)");
    }

    #[test]
    fn freeze_records_an_immutable_revision_and_rework_returns_to_executing() {
        let (_, mut c) = setup();
        let digest = drive_to_executing(&mut c);
        assert_eq!(c.frozen_digest(), Some(digest.as_str()));
        c.submit(
            &urn("child"),
            Submission {
                against_revision: digest.clone(),
                artifacts: vec!["urn:hacp:artifact:x".into()],
                evidence: vec![],
                claim: "done".into(),
            },
        )
        .unwrap();
        assert_eq!(c.state, ContractState::Verifying);
        c.decide(Verdict::Rework {
            scope: "trailing newline".into(),
        })
        .unwrap();
        assert_eq!(c.state, ContractState::Executing);
        assert_eq!(c.rework_scope.as_deref(), Some("trailing newline"));
    }

    #[test]
    fn a_submission_against_a_stale_revision_is_refused() {
        let (_, mut c) = setup();
        let digest = drive_to_executing(&mut c);
        // An amendment lands revision 2; the submission still answers rev 1.
        c.propose_amendment(&urn("parent")).unwrap();
        c.decide_amendment(&urn("parent"), true, Some(json_amended())).unwrap();
        c.decide_amendment(&urn("child"), true, Some(json_amended())).unwrap();
        assert!(matches!(
            c.submit(
                &urn("child"),
                Submission {
                    against_revision: digest,
                    artifacts: vec![],
                    evidence: vec![],
                    claim: "done".into(),
                },
            ),
            Err(ContractError::StaleRevision { .. })
        ));
    }

    fn json_amended() -> Value {
        serde_json::json!({"outputs": [{"name": "thing.txt"}, {"name": "other.txt"}], "budget": {"max_hours": 3}})
    }

    #[test]
    fn amendments_create_new_revisions_and_preserve_history() {
        let (_, mut c) = setup();
        let first = drive_to_executing(&mut c);
        c.propose_amendment(&urn("child")).unwrap();
        let second = c
            .decide_amendment(&urn("child"), true, Some(json_amended()))
            .unwrap();
        assert!(second.is_none(), "one participant alone does not amend");
        let second = c
            .decide_amendment(&urn("parent"), true, Some(json_amended()))
            .unwrap()
            .expect("both accepted");
        assert_ne!(first, second, "revision digests must differ");
        assert_eq!(c.revisions.len(), 2, "history is never rewritten");
        assert_eq!(c.revisions[0].digest, first);
        assert_eq!(c.state, ContractState::Executing);
    }

    #[test]
    fn amendment_refusal_returns_to_executing_unchanged() {
        let (_, mut c) = setup();
        let digest = drive_to_executing(&mut c);
        c.propose_amendment(&urn("parent")).unwrap();
        c.decide_amendment(&urn("child"), false, None).unwrap();
        assert_eq!(c.state, ContractState::Executing);
        assert_eq!(c.frozen_digest(), Some(digest.as_str()));
        assert_eq!(c.revisions.len(), 1);
    }

    #[test]
    fn amendment_bounds_end_in_no_agreement() {
        let (_, mut c) = setup();
        drive_to_executing(&mut c);
        // max_amendments = 1: the second accepted amendment exhausts the bound.
        for _ in 0..2 {
            c.propose_amendment(&urn("parent")).unwrap();
            c.decide_amendment(&urn("parent"), true, Some(json_amended())).unwrap();
            match c.decide_amendment(&urn("child"), true, Some(json_amended())) {
                Ok(_) | Err(_) => {}
            }
        }
        assert_eq!(c.state, ContractState::NoAgreement);
    }

    #[test]
    fn withdrawal_is_pre_freeze_only() {
        let (_, mut c) = setup();
        c.withdraw(&urn("child")).unwrap();
        assert_eq!(c.state, ContractState::Withdrawn);
        let (_, mut c) = setup();
        drive_to_executing(&mut c);
        assert!(matches!(
            c.withdraw(&urn("child")),
            Err(ContractError::IllegalTransition { action: "withdraw", .. })
        ));
    }

    #[test]
    fn verdicts_are_terminal_or_looped() {
        let (_, mut c) = setup();
        let digest = drive_to_executing(&mut c);
        c.submit(
            &urn("child"),
            Submission {
                against_revision: digest,
                artifacts: vec!["urn:hacp:artifact:x".into()],
                evidence: vec![],
                claim: "done".into(),
            },
        )
        .unwrap();
        c.decide(Verdict::Accept).unwrap();
        assert_eq!(c.state, ContractState::Settled);
        assert!(matches!(
            c.decide(Verdict::Accept),
            Err(ContractError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn outsiders_cannot_touch_a_contract() {
        let (_, mut c) = setup();
        assert!(matches!(
            c.agree(&urn("stranger"), &agreed_terms()),
            Err(ContractError::NotAParty { .. })
        ));
    }
}
