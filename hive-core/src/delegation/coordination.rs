//! Mechanical coordination rules for delegated assignments.
//!
//! Protocol state is represented by the pinned HACP v2 types. This module only
//! adapts peer messages and measured attestation results to that state machine.

use crate::runtime::attest::{self, Corroboration};
use hacp::v2::{canon, Contract, ContractLimits, Relationship, Session, Task, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgreementRecord {
    pub contract: Contract,
    pub proposed_terms: Value,
    pub proposed_digest: String,
    pub proposer: String,
    pub rejected_changes: Vec<Value>,
}

impl AgreementRecord {
    pub fn propose(task_id: &str, proposer: &str, peer: &str, text: &str) -> anyhow::Result<Self> {
        let proposer = agent_urn(proposer);
        let peer = agent_urn(peer);
        let mut session = Session::open(
            &format!("agreement:{task_id}:{}", canon::digest_canonical(text)),
            &proposer,
            &peer,
        )?;
        session.accept(&peer)?;
        let terms = json!({"agreement": text});
        let digest = canon::digest_of(&terms)?;
        let mut contract = Contract::propose(
            &session,
            &format!("contract:{task_id}:{}", &digest[..16]),
            Task {
                task_id: task_id.into(),
                summary: text.into(),
                owner: proposer.clone(),
            },
            Relationship::Collaboration,
            None,
            None,
            None,
            None,
            vec![],
            ContractLimits {
                max_rounds: 3,
                max_amendments: 16,
                max_rework: 2,
            },
        )?;
        contract.agree(&proposer, &terms)?;
        Ok(Self {
            contract,
            proposed_terms: terms,
            proposed_digest: digest,
            proposer,
            rejected_changes: vec![],
        })
    }

    /// Record the counterparty's reference. Only the exact canonical digest freezes.
    pub fn agree(&mut self, by: &str, digest: &str) -> anyhow::Result<String> {
        let by = agent_urn(by);
        if digest != self.proposed_digest {
            self.rejected_changes
                .push(json!({"by":by,"digest":digest,"reason":"one-sided change"}));
            anyhow::bail!("agreement digest does not match the bilateral proposal");
        }
        self.contract.agree(&by, &self.proposed_terms)?;
        Ok(self.contract.freeze(self.proposed_terms.clone())?)
    }

    /// Begin an amendment. It remains pending until both parties accept the same digest.
    pub fn propose_amendment(&mut self, by: &str, text: &str) -> anyhow::Result<String> {
        let by = agent_urn(by);
        let terms = json!({"agreement": text});
        self.contract.propose_amendment(&by)?;
        self.contract
            .decide_amendment(&by, true, Some(terms.clone()))?;
        self.proposed_terms = terms;
        self.proposed_digest = canon::digest_of(&self.proposed_terms)?;
        self.proposer = by;
        Ok(self.proposed_digest.clone())
    }

    pub fn agree_amendment(&mut self, by: &str, digest: &str) -> anyhow::Result<String> {
        let by = agent_urn(by);
        if digest != self.proposed_digest {
            self.rejected_changes
                .push(json!({"by":by,"digest":digest,"reason":"one-sided amendment"}));
            anyhow::bail!("amendment digest does not match the bilateral proposal");
        }
        self.contract
            .decide_amendment(&by, true, Some(self.proposed_terms.clone()))?
            .ok_or_else(|| anyhow::anyhow!("amendment still needs both parties"))
    }
}

fn agent_urn(id: &str) -> String {
    if id.starts_with("urn:hacp:agent:") {
        id.into()
    } else {
        format!("urn:hacp:agent:{id}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionVerdict {
    Accept,
    Rework,
    NoAgreement,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionRecord {
    pub verdict: CompletionVerdict,
    pub rework_rounds: u8,
    pub evidence: Vec<String>,
    pub followup: Option<String>,
}

impl Default for CompletionRecord {
    fn default() -> Self {
        Self {
            verdict: CompletionVerdict::Rework,
            rework_rounds: 0,
            evidence: vec![],
            followup: None,
        }
    }
}

impl CompletionRecord {
    /// Apply independently measured evidence through the existing attestation gate.
    pub fn assess(&mut self, evidence: impl Into<String>, corroboration: &Corroboration) {
        let evidence = evidence.into();
        self.evidence.push(evidence.clone());
        match attest::gate(&Verdict::Accept, corroboration) {
            Ok(()) => {
                self.verdict = CompletionVerdict::Accept;
                self.followup = None;
            }
            Err(error) if self.rework_rounds < 2 => {
                self.rework_rounds += 1;
                self.verdict = CompletionVerdict::Rework;
                self.followup = Some(format!(
                    "Acceptance checks failed: {error}. Evidence: {evidence}"
                ));
            }
            Err(_) => {
                self.verdict = CompletionVerdict::NoAgreement;
                self.followup = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreement_freezes_and_amendment_needs_both_sides() {
        let mut record = AgreementRecord::propose("task", "a", "b", "API v1").unwrap();
        let first = record.agree("b", &record.proposed_digest.clone()).unwrap();
        assert_eq!(record.contract.state, hacp::v2::ContractState::Executing);
        let amendment = record.propose_amendment("a", "API v2").unwrap();
        assert_eq!(record.contract.state, hacp::v2::ContractState::Amending);
        assert!(record.agree_amendment("b", "different").is_err());
        assert_eq!(record.rejected_changes.len(), 1);
        let second = record.agree_amendment("b", &amendment).unwrap();
        assert_ne!(first, second);
        assert_eq!(record.contract.revisions.len(), 2);
    }

    #[test]
    fn third_failed_attestation_is_no_agreement() {
        let failed = Corroboration {
            contradicted: vec!["missing output".into()],
            ..Default::default()
        };
        let mut record = CompletionRecord::default();
        record.assess("failure one", &failed);
        assert_eq!(record.verdict, CompletionVerdict::Rework);
        assert!(record.followup.as_deref().unwrap().contains("failure one"));
        record.assess("failure two", &failed);
        assert_eq!(record.verdict, CompletionVerdict::Rework);
        record.assess("failure three", &failed);
        assert_eq!(record.verdict, CompletionVerdict::NoAgreement);
        assert_eq!(record.rework_rounds, 2);
    }
}
