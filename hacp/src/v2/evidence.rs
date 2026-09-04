//! Evidence — `spec/HACP-2.0-draft.md` §9.2.
//!
//! Evidence attests to process or provenance: what was run, what a log shows,
//! what a test printed. It is produced by the performing side and consumed by
//! verification. Evidence never *is* truth; it is the input to a verdict —
//! which is why its shape is minimal and honest: a kind, a subject, a digest,
//! and a statement of what it claims.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::artifact::is_digest_shape;

/// What kind of attestation an evidence object carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A transcript of commands executed.
    CommandTranscript,
    /// An excerpt from a retained log.
    LogExcerpt,
    /// Output of a test or check run.
    TestOutput,
    /// A cryptographic signature over the subject.
    Signature,
    /// Anything else; the string says what, honestly unlabeled by the registry.
    Other(String),
}

/// What the evidence is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSubject {
    /// An artifact id (§9.1).
    Artifact(String),
    /// A free-form claim under the contract.
    Claim(String),
}

/// One attestation (§9.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    /// `e-` + ≥12 lowercase hex.
    pub evidence_id: String,
    pub kind: EvidenceKind,
    pub subject: EvidenceSubject,
    /// SHA-256 of the evidence content, 64 lowercase hex.
    pub digest: String,
    /// The agent that produced it.
    pub produced_by: String,
    /// One honest sentence about what this evidence shows.
    pub statement: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EvidenceError {
    #[error("evidence_id {found:?} must be \"e-\" followed by at least 12 lowercase hex characters")]
    BadId { found: String },
    #[error("digest must be 64 lowercase hex characters, found {found:?}")]
    BadDigest { found: String },
    #[error("produced_by: {0}")]
    BadProducer(String),
    #[error("subject must not be empty")]
    BadSubject,
    #[error("statement must be a non-empty string")]
    BadStatement,
    #[error("Other kind must be a non-empty string")]
    BadOther,
}

impl Evidence {
    pub fn validate(&self) -> Result<(), EvidenceError> {
        let Some(hex) = self.evidence_id.strip_prefix("e-") else {
            return Err(EvidenceError::BadId {
                found: self.evidence_id.clone(),
            });
        };
        if hex.len() < 12 || !hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(EvidenceError::BadId {
                found: self.evidence_id.clone(),
            });
        }
        if let EvidenceKind::Other(label) = &self.kind {
            if label.is_empty() {
                return Err(EvidenceError::BadOther);
            }
        }
        let subject = match &self.subject {
            EvidenceSubject::Artifact(s) | EvidenceSubject::Claim(s) => s,
        };
        if subject.is_empty() {
            return Err(EvidenceError::BadSubject);
        }
        if !is_digest_shape(&self.digest) {
            return Err(EvidenceError::BadDigest {
                found: self.digest.clone(),
            });
        }
        crate::v2::envelope::agent_urn::parse(&self.produced_by)
            .map_err(EvidenceError::BadProducer)?;
        if self.statement.is_empty() {
            return Err(EvidenceError::BadStatement);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> Evidence {
        Evidence {
            evidence_id: "e-0000000000000001".into(),
            kind: EvidenceKind::TestOutput,
            subject: EvidenceSubject::Artifact(
                "urn:hacp:artifact:9f0d0e6a-1111-4222-8333-444444444444".into(),
            ),
            digest: "c".repeat(64),
            produced_by: "urn:hacp:agent:child-1".into(),
            statement: "the acceptance check printed OK for thing.txt".into(),
        }
    }

    #[test]
    fn shapes_are_enforced() {
        evidence().validate().unwrap();
        let mut e = evidence();
        e.evidence_id = "v-0000000000000001".into();
        assert!(matches!(e.validate(), Err(EvidenceError::BadId { .. })));
        let mut e = evidence();
        e.digest = "TBD".into();
        assert!(matches!(e.validate(), Err(EvidenceError::BadDigest { .. })));
        let mut e = evidence();
        e.produced_by = "claude".into();
        assert!(matches!(e.validate(), Err(EvidenceError::BadProducer(_))));
        let mut e = evidence();
        e.subject = EvidenceSubject::Claim(String::new());
        assert!(matches!(e.validate(), Err(EvidenceError::BadSubject { .. })));
        let mut e = evidence();
        e.kind = EvidenceKind::Other(String::new());
        assert!(matches!(e.validate(), Err(EvidenceError::BadOther)));
    }
}
