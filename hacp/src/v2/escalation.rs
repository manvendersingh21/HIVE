//! Escalation (`spec/HACP-2.0-draft.md` §11).
//!
//! The ladder is fixed and short:
//!
//! ```text
//! escalation.raised (same-parent dispute)
//!         │ mediated by the shared supervisor
//!         ▼ unresolved
//! escalation.referred (to the LCA — walks §8.3 chains)
//!         │ LCA rules structurally, or MAY invoke an Arbiter
//!         ▼ unresolved
//! escalation.no_agreement — valid terminal; all evidence retained
//! ```
//!
//! Two properties are load-bearing and tested below: there is no N at which
//! an arbiter becomes mandatory (ADR-0001 §6.1 — optionality is protocol
//! law, not a default), and `no_agreement` is a *valid* terminal, never a
//! failure state to be papered over. The Escalation object is the record of
//! the journey: parties, subject, path taken, ruling or its absence.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::grant::OrgChart;

/// What the dispute is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EscalationSubject {
    Contract { contract_id: String },
    Task { task_id: String },
    Artifact { artifact_id: String },
}

/// The LCA's structural rulings (§11): split, reassign, deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "ruling", rename_all = "snake_case")]
pub enum Ruling {
    Split,
    Reassign { to: String },
    Deadline { at: String },
}

/// Where the escalation currently stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EscalationStage {
    Raised,
    Referred,
    Resolved,
    NoAgreement,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EscalationError {
    #[error("escalation parties must share a direct supervisor to raise (§11); {0:?} and {1:?} do not")]
    NotSameParent(String, String),
    #[error("mediator {0:?} is not the shared supervisor")]
    NotMediator(String),
    #[error("arbiter {0:?} must differ from both parties")]
    ArbiterIsParty(String),
    #[error("referral requires an unresolved Raised stage")]
    NotRaised,
    #[error("resolution requires a Referred stage (or an arbiter invoked from it)")]
    NotReferred,
    #[error("a resolved or no-agreement escalation is closed")]
    Closed,
    #[error("no common supervisor for referral")]
    NoLca,
}

/// The journey record (§11). Transitions are the only mutations, and each
/// one is legality-checked against the ladder above.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Escalation {
    /// `esc-` + 12+ lowercase hex.
    pub escalation_id: String,
    /// The two disputing agents, in raise order.
    pub parties: [String; 2],
    pub subject: EscalationSubject,
    pub raised_at: String,
    pub stage: EscalationStage,
    /// Shared supervisor at raise time; the LCA after referral.
    pub mediator: Option<String>,
    /// Invoked at the LCA's discretion — optional at every N, never required.
    pub arbiter: Option<String>,
    /// The ruling, if any.
    pub ruling: Option<Ruling>,
}

impl Escalation {
    /// Stage one: a same-parent dispute, mediated by that shared supervisor.
    /// Parties in different subtrees start elsewhere (their supervisors
    /// escalate through their own chains, or the dispute is cross-branch and
    /// needs a §10 permit before it is a collaboration at all).
    pub fn raise(
        org: &OrgChart,
        escalation_id: &str,
        a: &str,
        b: &str,
        subject: EscalationSubject,
        at: &str,
    ) -> Result<(Self, String), EscalationError> {
        let shared = org
            .parent_of
            .get(a)
            .filter(|p| org.parent_of.get(b) == Some(p))
            .cloned();
        let mediator = shared.ok_or_else(|| EscalationError::NotSameParent(a.to_string(), b.to_string()))?;
        let esc = Escalation {
            escalation_id: escalation_id.to_string(),
            parties: [a.to_string(), b.to_string()],
            subject,
            raised_at: at.to_string(),
            stage: EscalationStage::Raised,
            mediator: Some(mediator.clone()),
            arbiter: None,
            ruling: None,
        };
        Ok((esc, mediator))
    }

    /// Stage two: the shared supervisor did not resolve it; the dispute walks
    /// the §8.3 chains to the lowest common supervisor. At the top of the
    /// org this is idempotent with stage one's mediator — which is legal:
    /// the LCA of siblings under the root *is* the shared supervisor.
    pub fn refer(&mut self, org: &OrgChart) -> Result<(), EscalationError> {
        if self.stage != EscalationStage::Raised {
            return Err(EscalationError::NotRaised);
        }
        let lca = org
            .lca(&self.parties[0], &self.parties[1])
            .ok_or(EscalationError::NoLca)?;
        self.mediator = Some(lca);
        self.stage = EscalationStage::Referred;
        Ok(())
    }

    /// The LCA MAY invoke an arbiter — optional at every N, never mandatory.
    /// The arbiter must be a third agent, not a party.
    pub fn invoke_arbiter(&mut self, arbiter: &str) -> Result<(), EscalationError> {
        if self.stage == EscalationStage::Resolved || self.stage == EscalationStage::NoAgreement {
            return Err(EscalationError::Closed);
        }
        if self.parties.contains(&arbiter.to_string()) {
            return Err(EscalationError::ArbiterIsParty(arbiter.to_string()));
        }
        self.arbiter = Some(arbiter.to_string());
        Ok(())
    }

    /// `escalation.resolved` carries the ruling (§11).
    pub fn resolve(&mut self, ruling: Ruling) -> Result<(), EscalationError> {
        if self.stage != EscalationStage::Referred {
            return Err(EscalationError::NotReferred);
        }
        self.ruling = Some(ruling);
        self.stage = EscalationStage::Resolved;
        Ok(())
    }

    /// The valid terminal: no agreement, all evidence retained — meaning
    /// exactly this object, with its full path taken, survives untouched.
    pub fn conclude_no_agreement(&mut self) -> Result<(), EscalationError> {
        if self.stage == EscalationStage::Resolved || self.stage == EscalationStage::NoAgreement {
            return Err(EscalationError::Closed);
        }
        self.stage = EscalationStage::NoAgreement;
        Ok(())
    }

    /// The wire kind matching the current stage (registry §5.3).
    pub fn stage_kind(&self) -> &'static str {
        match self.stage {
            EscalationStage::Raised => super::envelope::kinds::ESCALATION_RAISED,
            EscalationStage::Referred => super::envelope::kinds::ESCALATION_REFERRED,
            EscalationStage::Resolved => super::envelope::kinds::ESCALATION_RESOLVED,
            EscalationStage::NoAgreement => super::envelope::kinds::ESCALATION_NO_AGREEMENT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urn(n: &str) -> String {
        format!("urn:hacp:agent:{n}")
    }

    fn org() -> OrgChart {
        let mut org = OrgChart::default();
        // root -> p1 -> {c1, c2}; root -> p2 -> c3
        for (child, parent) in [("p1", "root"), ("p2", "root"), ("c1", "p1"), ("c2", "p1"), ("c3", "p2")] {
            org.parent_of.insert(urn(child), urn(parent));
        }
        org
    }

    const AT: &str = "2026-09-04T12:00:00Z";

    #[test]
    fn a_dispute_raises_under_the_shared_supervisor() {
        let (esc, mediator) = Escalation::raise(
            &org(),
            "esc-000000000001",
            &urn("c1"),
            &urn("c2"),
            EscalationSubject::Task { task_id: "t-1".into() },
            AT,
        )
        .unwrap();
        assert_eq!(mediator, urn("p1"));
        assert_eq!(esc.stage, EscalationStage::Raised);
        assert_eq!(esc.stage_kind(), "escalation.raised");
    }

    #[test]
    fn cousins_do_not_share_a_parent_and_cannot_raise_at_stage_one() {
        let err = Escalation::raise(
            &org(),
            "esc-000000000002",
            &urn("c1"),
            &urn("c3"),
            EscalationSubject::Task { task_id: "t-1".into() },
            AT,
        )
        .unwrap_err();
        assert_eq!(err, EscalationError::NotSameParent(urn("c1"), urn("c3")));
    }

    #[test]
    fn referral_walks_to_the_lca_and_the_lca_rules_structurally() {
        let (mut esc, _) = Escalation::raise(
            &org(),
            "esc-000000000003",
            &urn("c1"),
            &urn("c2"),
            EscalationSubject::Contract { contract_id: "c-1".into() },
            AT,
        )
        .unwrap();
        esc.refer(&org()).unwrap();
        assert_eq!(esc.mediator.as_deref(), Some(urn("p1").as_str()));
        assert_eq!(esc.stage_kind(), "escalation.referred");
        esc.resolve(Ruling::Reassign { to: urn("c3") }).unwrap();
        assert_eq!(esc.stage, EscalationStage::Resolved);
        assert_eq!(
            esc.ruling,
            Some(Ruling::Reassign { to: urn("c3") })
        );
        // Closed means closed.
        assert_eq!(esc.conclude_no_agreement(), Err(EscalationError::Closed));
    }

    #[test]
    fn an_arbiter_is_optional_at_every_n_and_never_mandatory() {
        let (mut esc, _) = Escalation::raise(
            &org(),
            "esc-000000000004",
            &urn("c1"),
            &urn("c2"),
            EscalationSubject::Task { task_id: "t-2".into() },
            AT,
        )
        .unwrap();
        esc.refer(&org()).unwrap();
        // Path A: no arbiter at all — the LCA resolves directly.
        esc.resolve(Ruling::Split).unwrap();
        assert!(esc.arbiter.is_none(), "no arbiter was ever required");

        // Path B: an arbiter may be invoked, but not a party.
        let (mut esc2, _) = Escalation::raise(
            &org(),
            "esc-000000000005",
            &urn("c1"),
            &urn("c2"),
            EscalationSubject::Task { task_id: "t-3".into() },
            AT,
        )
        .unwrap();
        esc2.refer(&org()).unwrap();
        assert_eq!(
            esc2.invoke_arbiter(&urn("c1")),
            Err(EscalationError::ArbiterIsParty(urn("c1")))
        );
        esc2.invoke_arbiter(&urn("arb-1")).unwrap();
        esc2.resolve(Ruling::Deadline { at: "2026-09-05T00:00:00Z".into() }).unwrap();
    }

    #[test]
    fn no_agreement_is_a_valid_terminal_with_evidence_retained() {
        let (mut esc, _) = Escalation::raise(
            &org(),
            "esc-000000000006",
            &urn("c1"),
            &urn("c2"),
            EscalationSubject::Artifact { artifact_id: "urn:hacp:artifact:x".into() },
            AT,
        )
        .unwrap();
        esc.refer(&org()).unwrap();
        esc.conclude_no_agreement().unwrap();
        assert_eq!(esc.stage_kind(), "escalation.no_agreement");
        // The journey record survives: parties, subject, raised_at, path.
        assert_eq!(esc.parties, [urn("c1"), urn("c2")]);
        assert_eq!(esc.raised_at, AT);
        assert!(esc.ruling.is_none(), "no ruling was papered over it");

        // Transitions out of the terminal are refused.
        assert_eq!(esc.refer(&org()), Err(EscalationError::NotRaised));
    }

    #[test]
    fn resolution_cannot_skip_the_ladder() {
        let (mut esc, _) = Escalation::raise(
            &org(),
            "esc-000000000007",
            &urn("c1"),
            &urn("c2"),
            EscalationSubject::Task { task_id: "t-4".into() },
            AT,
        )
        .unwrap();
        // No referral yet: the shared supervisor's stage is not a ruling stage.
        assert_eq!(esc.resolve(Ruling::Split), Err(EscalationError::NotReferred));
    }
}
