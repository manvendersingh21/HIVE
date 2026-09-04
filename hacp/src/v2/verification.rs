//! Verification — `spec/HACP-2.0-draft.md` §9.3–§9.4.
//!
//! A verification is a verdict record: who verified, what submission, against
//! which frozen revision, which checks ran, and the verdict with reasons. Two
//! rules are enforced by construction rather than by review:
//!
//! * **Evidence over signals** (§9.4, Phase S finding 2): an Accept verdict
//!   requires subject artifacts and at least one passing check. A process exit
//!   code, an exit file, or a self-report is a signal; a verdict built on
//!   signals alone is refused here, mechanically.
//! * **Attestation composes, authority does not** (§9.3, ADR-0001 §5): a
//!   supervisor's verification may reference children's verification records
//!   in `attests`; the reference graph is acyclic or refused.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::artifact::is_digest_shape;
use super::contract::Verdict;

/// One mechanical or judgment check a verifier ran (§9.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Check {
    /// What was checked, e.g. `file-has-one-line`.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// What was observed — the reason a machine did not decide alone.
    pub detail: String,
}

/// A verdict record (§9.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Verification {
    /// `v-` + ≥12 lowercase hex.
    pub verification_id: String,
    /// The agent that verified.
    pub verifier: String,
    pub contract_id: String,
    /// The frozen revision digest this verification answers.
    pub against_revision: String,
    /// Subject artifact ids (§9.1).
    pub artifacts: Vec<String>,
    /// Evidence ids consumed (§9.2).
    pub evidence: Vec<String>,
    /// The checks that were run.
    pub checks: Vec<Check>,
    pub verdict: Verdict,
    /// Reasons for anything not decided by a passing check alone.
    pub reasons: Vec<String>,
    /// Child verification records this one attests (§9.3): recursive
    /// attestation. Authority is never implied by the reference.
    pub attests: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VerificationError {
    #[error("verification_id {found:?} must be \"v-\" followed by at least 12 lowercase hex characters")]
    BadId { found: String },
    #[error("verifier: {0}")]
    BadVerifier(String),
    #[error("against_revision must be a 64-hex digest, found {found:?}")]
    BadRevision { found: String },
    #[error("cannot accept on signals alone (§9.4): a verdict of Accept requires subject artifacts and at least one passing check — measured: a blocked stock CLI exited 0 having produced nothing")]
    SignalsAreNotEvidence,
    #[error("attestation cycle detected involving {0:?}")]
    Cycle(String),
    #[error("unknown verification {0:?} in attestation closure")]
    UnknownVerification(String),
}

impl Verification {
    /// Build a verdict record over a submission. The §9.4 rule lives here:
    /// Accept without artifacts or without a passing check is refused — the
    /// error message cites the measurement that made it law.
    pub fn decide(
        verification_id: &str,
        verifier: &str,
        contract_id: &str,
        against_revision: &str,
        artifacts: Vec<String>,
        evidence: Vec<String>,
        checks: Vec<Check>,
        verdict: Verdict,
        reasons: Vec<String>,
        attests: Vec<String>,
    ) -> Result<Self, VerificationError> {
        let record = Self {
            verification_id: verification_id.to_string(),
            verifier: verifier.to_string(),
            contract_id: contract_id.to_string(),
            against_revision: against_revision.to_string(),
            artifacts,
            evidence,
            checks,
            verdict,
            reasons,
            attests,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<(), VerificationError> {
        let Some(hex) = self.verification_id.strip_prefix("v-") else {
            return Err(VerificationError::BadId {
                found: self.verification_id.clone(),
            });
        };
        if hex.len() < 12 || !hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(VerificationError::BadId {
                found: self.verification_id.clone(),
            });
        }
        crate::v2::envelope::agent_urn::parse(&self.verifier)
            .map_err(VerificationError::BadVerifier)?;
        if !is_digest_shape(&self.against_revision) {
            return Err(VerificationError::BadRevision {
                found: self.against_revision.clone(),
            });
        }
        if matches!(self.verdict, Verdict::Accept)
            && (self.artifacts.is_empty() || !self.checks.iter().any(|c| c.passed))
        {
            return Err(VerificationError::SignalsAreNotEvidence);
        }
        Ok(())
    }
}

/// The transitive attestation closure of a verification: the child records it
/// stands on (§9.3). Cycles are refused; diamonds visit once.
pub fn attestation_closure<'a>(
    records: &'a [Verification],
    id: &str,
) -> Result<Vec<&'a Verification>, VerificationError> {
    let mut colors: std::collections::BTreeMap<String, bool> = Default::default();
    let mut order: Vec<&Verification> = Vec::new();
    let start = records
        .iter()
        .find(|v| v.verification_id == id)
        .ok_or_else(|| VerificationError::UnknownVerification(id.to_string()))?;
    for child in &start.attests {
        visit(records, child, &mut colors, &mut order)?;
    }
    Ok(order)
}

fn visit<'a>(
    records: &'a [Verification],
    id: &str,
    colors: &mut std::collections::BTreeMap<String, bool>,
    order: &mut Vec<&'a Verification>,
) -> Result<(), VerificationError> {
    match colors.get(id) {
        Some(true) => return Ok(()),      // diamond
        Some(false) => return Err(VerificationError::Cycle(id.to_string())),
        None => {}
    }
    colors.insert(id.to_string(), false);
    let record = records
        .iter()
        .find(|v| v.verification_id == id)
        .ok_or_else(|| VerificationError::UnknownVerification(id.to_string()))?;
    for child in &record.attests {
        visit(records, child, colors, order)?;
    }
    colors.insert(id.to_string(), true);
    order.push(record);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept_inputs() -> (Vec<String>, Vec<Check>) {
        let artifacts = vec!["urn:hacp:artifact:9f0d0e6a-1111-4222-8333-444444444444".into()];
        let checks = vec![Check {
            name: "file-has-one-line".into(),
            passed: true,
            detail: "wc -l printed 1".into(),
        }];
        (artifacts, checks)
    }

    fn record(verdict: Verdict, artifacts: Vec<String>, checks: Vec<Check>) -> Result<Verification, VerificationError> {
        let reasons = match &verdict {
            Verdict::Accept => vec![],
            _ => vec!["reasons accompany refusals".to_string()],
        };
        Verification::decide(
            "v-0000000000000001",
            "urn:hacp:agent:parent-1",
            "c-1",
            &"d".repeat(64),
            artifacts,
            vec![],
            checks,
            verdict,
            reasons,
            vec![],
        )
    }

    #[test]
    fn an_accept_needs_artifacts_and_a_passing_check() {
        let (artifacts, checks) = accept_inputs();
        record(Verdict::Accept, artifacts.clone(), checks.clone()).unwrap();
        // No artifacts: signals alone (§9.4).
        assert!(matches!(
            record(Verdict::Accept, vec![], checks),
            Err(VerificationError::SignalsAreNotEvidence)
        ));
        // Checks all failed: nothing mechanical stood behind the accept.
        let (artifacts, mut checks) = accept_inputs();
        checks[0].passed = false;
        assert!(matches!(
            record(Verdict::Accept, artifacts, checks),
            Err(VerificationError::SignalsAreNotEvidence)
        ));
        // Rework and reject need no such support: refusal travels with reasons.
        let (artifacts, checks) = accept_inputs();
        record(Verdict::Rework { scope: "s".into() }, vec![], vec![]).unwrap();
        record(Verdict::Reject, vec![], vec![]).unwrap();
        let _ = (artifacts, checks);
    }

    #[test]
    fn attestation_composes_and_cycles_are_refused() {
        let (artifacts, checks) = accept_inputs();
        let child = record(Verdict::Accept, artifacts.clone(), checks).unwrap();
        let parent = Verification::decide(
            "v-0000000000000002",
            "urn:hacp:agent:root-1",
            "c-1",
            &"d".repeat(64),
            artifacts.clone(),
            vec![],
            vec![Check { name: "integration".into(), passed: true, detail: "children settled".into() }],
            Verdict::Accept,
            vec!["attests child verification".into()],
            vec![child.verification_id.clone()],
        )
        .unwrap();
        let records = [child, parent];
        let closure =
            attestation_closure(&records, "v-0000000000000002").unwrap();
        assert_eq!(closure.len(), 1, "the child stands under the parent");
        // A cycle: the child attests the parent.
        let mut cyclic = records[0].clone();
        cyclic.attests = vec!["v-0000000000000002".into()];
        let records = [cyclic, records[1].clone()];
        assert!(matches!(
            attestation_closure(&records, "v-0000000000000001"),
            Err(VerificationError::Cycle(_))
        ));
    }

    #[test]
    fn ids_revisions_and_verifiers_have_shapes() {
        let (artifacts, checks) = accept_inputs();
        let mut v = record(Verdict::Accept, artifacts, checks).unwrap();
        v.verification_id = "e-0000000000000001".into();
        assert!(matches!(v.validate(), Err(VerificationError::BadId { .. })));
        let (artifacts, checks) = accept_inputs();
        let mut v = record(Verdict::Accept, artifacts, checks).unwrap();
        v.against_revision = "latest".into();
        assert!(matches!(v.validate(), Err(VerificationError::BadRevision { .. })));
        let (artifacts, checks) = accept_inputs();
        let mut v = record(Verdict::Accept, artifacts, checks).unwrap();
        v.verifier = "codex".into();
        assert!(matches!(v.validate(), Err(VerificationError::BadVerifier(_))));
    }
}
