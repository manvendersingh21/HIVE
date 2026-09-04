//! Delegation and authority (`spec/HACP-2.0-draft.md` §8) and cross-branch
//! collaboration (§10).
//!
//! §8's inviolable rule is enforced here, at grant time, for every layer of a
//! chain:
//!
//! ```text
//! grantee_authority ⊆ grantor_delegable_authority
//! ```
//!
//! A grant exceeding its grantor's delegable authority is invalid ab initio —
//! there is no window in which it was good. Revocation closes a grant; work
//! continued under a revoked grant is a contract failure to be escalated, not
//! a protocol violation to be hidden (§8.2).
//!
//! The [`OrgChart`] carries the *organizational* facts of §8.3: declared
//! parent chains, lowest-common-supervisor discovery for §10's permits and
//! §11's referrals. It never implies communication or artifact edges
//! (ADR-0001 §4) — those live in sessions and artifacts.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::canon::validate_timestamp;

/// A scope element with its own `delegable` flag (§8.2): authority to *hold*
/// and authority to *re-delegate* are different facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScopeElement {
    pub name: String,
    pub delegable: bool,
}

/// A CapabilityGrant: grantor, grantee, authority scope set, validity window,
/// and the parent grant this one descends from (`None` = chartered directly
/// by the deployment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CapabilityGrant {
    /// `g-` + 12+ lowercase hex.
    pub grant_id: String,
    pub grantor: String,
    pub grantee: String,
    pub scopes: Vec<ScopeElement>,
    /// RFC3339 Z, seconds precision (§5.1).
    pub valid_from: String,
    pub valid_until: String,
    /// Parent grant id; `None` for a deployment charter.
    pub parent: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GrantError {
    #[error("scope {0:?} is not delegable by the grantor (§8.2)")]
    NotDelegable(String),
    #[error("grant {0:?} not found")]
    UnknownGrant(String),
    #[error("grant {0:?} is closed (revoked or expired)")]
    ClosedGrant(String),
    #[error("grantor {grantor:?} does not match parent grant's grantee {grantee:?}")]
    ChainMismatch { grantor: String, grantee: String },
    #[error("window invalid: from {from:?} until {until:?}")]
    BadWindow { from: String, until: String },
    #[error("malformed grant id {0:?}")]
    BadId(String),
    #[error("bad agent URN in grant: {0}")]
    BadUrn(String),
    #[error("bad timestamp in grant: {0}")]
    BadTimestamp(String),
}

fn fresh_permit_id() -> String {
    format!("cp-{}", uuid::Uuid::new_v4().simple())
}

fn hex_id(prefix: &str, id: &str) -> Result<(), GrantError> {
    let rest = id
        .strip_prefix(prefix)
        .ok_or_else(|| GrantError::BadId(id.to_string()))?;
    if rest.len() >= 12 && rest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(GrantError::BadId(id.to_string()))
    }
}

impl CapabilityGrant {
    /// A deployment charter: the root of a grant chain, issued by the
    /// deployment itself. The chartering authority (`"deployment"` or an
    /// operator URN) becomes the grantor.
    pub fn charter(
        grant_id: &str,
        charterer: &str,
        grantee: &str,
        scopes: Vec<ScopeElement>,
        valid_from: &str,
        valid_until: &str,
    ) -> Result<Self, GrantError> {
        let g = CapabilityGrant {
            grant_id: grant_id.to_string(),
            grantor: charterer.to_string(),
            grantee: grantee.to_string(),
            scopes,
            valid_from: valid_from.to_string(),
            valid_until: valid_until.to_string(),
            parent: None,
        };
        g.check_shape()?;
        Ok(g)
    }

    fn check_shape(&self) -> Result<(), GrantError> {
        hex_id("g-", &self.grant_id)?;
        super::envelope::agent_urn::parse(&self.grantor)
            .map_err(|_| GrantError::BadUrn(self.grantor.clone()))?;
        super::envelope::agent_urn::parse(&self.grantee)
            .map_err(|_| GrantError::BadUrn(self.grantee.clone()))?;
        validate_timestamp(&self.valid_from).map_err(|_| GrantError::BadTimestamp(self.valid_from.clone()))?;
        validate_timestamp(&self.valid_until).map_err(|_| GrantError::BadTimestamp(self.valid_until.clone()))?;
        if self.valid_from > self.valid_until {
            return Err(GrantError::BadWindow {
                from: self.valid_from.clone(),
                until: self.valid_until.clone(),
            });
        }
        Ok(())
    }

    fn window_open(&self, at: &str) -> bool {
        self.valid_from.as_str() <= at && at <= self.valid_until.as_str()
    }

    /// The delegable view of this grant's authority: names marked delegable.
    fn delegable(&self) -> BTreeMap<&str, bool> {
        self.scopes
            .iter()
            .map(|s| (s.name.as_str(), s.delegable))
            .collect()
    }
}

/// A ledger of issued grants; the enforcement point of §8.2.
#[derive(Debug, Default, Clone)]
pub struct GrantLedger {
    grants: BTreeMap<String, CapabilityGrant>,
    revoked: BTreeSet<String>,
}

impl GrantLedger {
    /// Enforce the inviolable rule and record the grant. For a child grant
    /// (`parent` set): the parent must exist, be open at `at`, have the
    /// declared grantor as its grantee, and carry every requested scope as
    /// delegable — and a scope may only stay delegable downstream if it was
    /// delegable upstream. Every layer of a chain passes the same test.
    pub fn issue(&mut self, grant: CapabilityGrant, at: &str) -> Result<(), GrantError> {
        grant.check_shape()?;
        if let Some(parent_id) = &grant.parent {
            let parent = self
                .grants
                .get(parent_id)
                .ok_or_else(|| GrantError::UnknownGrant(parent_id.clone()))?;
            if self.revoked.contains(parent_id) || !parent.window_open(at) {
                return Err(GrantError::ClosedGrant(parent_id.clone()));
            }
            if parent.grantee != grant.grantor {
                return Err(GrantError::ChainMismatch {
                    grantor: grant.grantor.clone(),
                    grantee: parent.grantee.clone(),
                });
            }
            let upstream = parent.delegable();
            for scope in &grant.scopes {
                match upstream.get(scope.name.as_str()) {
                    None => return Err(GrantError::NotDelegable(scope.name.clone())),
                    Some(false) => return Err(GrantError::NotDelegable(scope.name.clone())),
                    Some(true) => {}
                }
            }
        }
        self.grants.insert(grant.grant_id.clone(), grant);
        Ok(())
    }

    /// Close a grant (§8.2). Idempotent for an already-revoked grant.
    pub fn revoke(&mut self, grant_id: &str) -> Result<(), GrantError> {
        match self.grants.get(grant_id) {
            Some(_) => {
                self.revoked.insert(grant_id.to_string());
                Ok(())
            }
            None => Err(GrantError::UnknownGrant(grant_id.to_string())),
        }
    }

    /// Is the grant open (unrevoked, inside its window) at `at`?
    pub fn is_open(&self, grant_id: &str, at: &str) -> Result<bool, GrantError> {
        let g = self
            .grants
            .get(grant_id)
            .ok_or_else(|| GrantError::UnknownGrant(grant_id.to_string()))?;
        Ok(!self.revoked.contains(grant_id) && g.window_open(at))
    }

    /// The open grants held by `agent` at `at`.
    pub fn held_by(&self, agent: &str, at: &str) -> Vec<&CapabilityGrant> {
        self.grants
            .values()
            .filter(|g| g.grantee == agent && !self.revoked.contains(&g.grant_id) && g.window_open(at))
            .collect()
    }

    /// Does `agent` hold an open grant covering `scope` at `at`?
    pub fn covers(&self, agent: &str, scope: &str, at: &str) -> bool {
        self.held_by(agent, at)
            .iter()
            .any(|g| g.scopes.iter().any(|s| s.name == scope))
    }

    /// §10 preauthorization: does `agent` hold an open standing grant whose
    /// `cross-branch/<class>` scope covers this task class? The supervisor
    /// issues these at its discretion (§13 sibling preauthorization).
    pub fn preauthorizes(&self, agent: &str, task_class: &str, at: &str) -> Option<String> {
        let wanted = format!("cross-branch/{task_class}");
        self.held_by(agent, at)
            .iter()
            .find(|g| g.scopes.iter().any(|s| s.name == wanted))
            .map(|g| g.grant_id.clone())
    }
}

/// The organizational chart of §8.3: declared parent chains only. It answers
/// chain and LCA questions for §10 permits and §11 escalation referrals, and
/// arity questions for profiles (§13). It grants no communication rights.
#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrgChart {
    /// child agent URN → parent agent URN. Absent = root.
    pub parent_of: BTreeMap<String, String>,
}

impl OrgChart {
    /// The declared chain from `agent` up to its root, inclusive of both.
    /// §11 and §10 walk this; it never implies session or artifact edges.
    pub fn chain(&self, agent: &str) -> Vec<String> {
        let mut path = vec![agent.to_string()];
        let mut current = agent;
        while let Some(parent) = self.parent_of.get(current) {
            path.push(parent.clone());
            current = parent;
        }
        path
    }

    /// The lowest common supervisor of `a` and `b` (§10.2): the first agent
    /// appearing in both declared chains, nearest to the leaves.
    pub fn lca(&self, a: &str, b: &str) -> Option<String> {
        let chain_b: BTreeSet<String> = self.chain(b).into_iter().collect();
        self.chain(a).into_iter().find(|node| chain_b.contains(node))
    }

    /// Do `a` and `b` report to the same direct supervisor? (§11's stage one.)
    pub fn shares_parent(&self, a: &str, b: &str) -> bool {
        a != b
            && self.parent_of.get(a).is_some()
            && self.parent_of.get(a) == self.parent_of.get(b)
    }

    /// Direct reports of `agent` (§13 arity checks).
    pub fn children(&self, agent: &str) -> Vec<String> {
        let mut kids: Vec<String> = self
            .parent_of
            .iter()
            .filter(|(_, p)| p.as_str() == agent)
            .map(|(c, _)| c.clone())
            .collect();
        kids.sort();
        kids
    }
}

/// §10.1: one agent asks its own supervisor chain; names the prospective
/// peer, task class, scope, expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CollaborationRequest {
    /// `cr-` + 12+ lowercase hex.
    pub request_id: String,
    pub requester: String,
    pub peer: String,
    pub task_class: String,
    pub expires: String,
}

/// How a §10.2 permit was grounded: an LCA ruling, or a standing
/// preauthorization grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum PermitBasis {
    Lca { supervisor: String },
    Preauthorization { grant_id: String },
}

/// §10.2: the permit that authorizes a cross-branch *session* — never its
/// outcome. The resulting bilateral session records the permit id; the
/// contract still negotiates, freezes, and verifies like any other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CollaborationPermit {
    /// `cp-` + 12+ lowercase hex.
    pub permit_id: String,
    pub request_id: String,
    /// The pair this permit authorizes, from the §10.1 request.
    pub requester: String,
    pub peer: String,
    pub issued_by: String,
    pub basis: PermitBasis,
    pub expires: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CrossBranchError {
    #[error("issuer {issuer:?} is not the lowest common supervisor of {a:?} and {b:?} (§10.2)")]
    NotLca { issuer: String, a: String, b: String },
    #[error("no common supervisor for {0:?} and {1:?}")]
    NoLca(String, String),
    #[error("preauthorization missing or closed for {requester:?}/{class:?}")]
    NoPreauthorization { requester: String, class: String },
    #[error("permit {0:?} does not authorize a session between {1:?} and {2:?}")]
    WrongPair(String, String, String),
    #[error("permit expired at {0:?}")]
    Expired(String),
    #[error(transparent)]
    Grant(#[from] GrantError),
}

impl CollaborationPermit {
    /// §10.2, path one: the LCA of the two branches issues the permit.
    pub fn by_lca(
        org: &OrgChart,
        request: &CollaborationRequest,
        issuer: &str,
    ) -> Result<Self, CrossBranchError> {
        let lca = org
            .lca(&request.requester, &request.peer)
            .ok_or_else(|| CrossBranchError::NoLca(request.requester.clone(), request.peer.clone()))?;
        if issuer != lca {
            return Err(CrossBranchError::NotLca {
                issuer: issuer.to_string(),
                a: request.requester.clone(),
                b: request.peer.clone(),
            });
        }
        Ok(CollaborationPermit {
            permit_id: fresh_permit_id(),
            request_id: request.request_id.clone(),
            requester: request.requester.clone(),
            peer: request.peer.clone(),
            issued_by: issuer.to_string(),
            basis: PermitBasis::Lca { supervisor: lca },
            expires: request.expires.clone(),
        })
    }

    /// §10.2, path two: preauthorization — a standing `cross-branch/<class>`
    /// grant substitutes for the LCA's per-session ruling.
    pub fn by_preauthorization(
        ledger: &GrantLedger,
        request: &CollaborationRequest,
        at: &str,
    ) -> Result<Self, CrossBranchError> {
        let grant_id = ledger
            .preauthorizes(&request.requester, &request.task_class, at)
            .ok_or_else(|| CrossBranchError::NoPreauthorization {
                requester: request.requester.clone(),
                class: request.task_class.clone(),
            })?;
        Ok(CollaborationPermit {
            permit_id: fresh_permit_id(),
            request_id: request.request_id.clone(),
            requester: request.requester.clone(),
            peer: request.peer.clone(),
            issued_by: request.requester.clone(),
            basis: PermitBasis::Preauthorization { grant_id },
            expires: request.expires.clone(),
        })
    }

    /// Does this permit authorize a session between exactly these two agents,
    /// unexpired at `at`? The permit authorizes the session, not the outcome.
    pub fn authorizes(&self, a: &str, b: &str, at: &str) -> Result<(), CrossBranchError> {
        if at > self.expires.as_str() {
            return Err(CrossBranchError::Expired(self.expires.clone()));
        }
        let pair_matches = (a == self.requester && b == self.peer)
            || (a == self.peer && b == self.requester);
        if pair_matches {
            Ok(())
        } else {
            Err(CrossBranchError::WrongPair(
                self.permit_id.clone(),
                a.to_string(),
                b.to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-09-04T00:00:00Z";
    const T1: &str = "2026-12-31T23:59:59Z";

    fn urn(n: &str) -> String {
        format!("urn:hacp:agent:{n}")
    }

    fn root_grant(grantee: &str, scopes: Vec<ScopeElement>) -> CapabilityGrant {
        CapabilityGrant::charter(
            "g-000000000001",
            "urn:hacp:agent:charter-1",
            grantee,
            scopes,
            T0,
            T1,
        )
        .unwrap()
    }

    #[test]
    fn a_charter_is_valid_ab_initio() {
        let mut ledger = GrantLedger::default();
        ledger
            .issue(root_grant(&urn("boss"), vec![ScopeElement { name: "work/all".into(), delegable: true }]), T0)
            .unwrap();
        assert!(ledger.covers(&urn("boss"), "work/all", "2026-10-01T00:00:00Z"));
    }

    #[test]
    fn a_grant_exceeding_delegable_authority_is_invalid_ab_initio() {
        let mut ledger = GrantLedger::default();
        ledger
            .issue(
                root_grant(&urn("boss"), vec![ScopeElement { name: "work/a".into(), delegable: false }]),
                T0,
            )
            .unwrap();
        let child = CapabilityGrant {
            grant_id: "g-000000000002".into(),
            grantor: urn("boss"),
            grantee: urn("kid"),
            scopes: vec![ScopeElement { name: "work/a".into(), delegable: false }],
            valid_from: T0.into(),
            valid_until: T1.into(),
            parent: Some("g-000000000001".into()),
        };
        assert_eq!(ledger.issue(child, T0), Err(GrantError::NotDelegable("work/a".into())));
    }

    #[test]
    fn every_layer_of_a_chain_passes_the_same_test() {
        let mut ledger = GrantLedger::default();
        ledger
            .issue(root_grant(&urn("root"), vec![
                ScopeElement { name: "work/all".into(), delegable: true },
                ScopeElement { name: "work/secret".into(), delegable: false },
            ]), T0)
            .unwrap();
        // Layer 1: root delegates work/all (delegable) down.
        ledger
            .issue(CapabilityGrant {
                grant_id: "g-000000000002".into(),
                grantor: urn("root"),
                grantee: urn("mid"),
                scopes: vec![ScopeElement { name: "work/all".into(), delegable: true }],
                valid_from: T0.into(),
                valid_until: T1.into(),
                parent: Some("g-000000000001".into()),
            }, T0)
            .unwrap();
        // Layer 2 tries to smuggle work/secret in: refused, and delegable
        // cannot be re-marked either.
        let bad = CapabilityGrant {
            grant_id: "g-000000000003".into(),
            grantor: urn("mid"),
            grantee: urn("leaf"),
            scopes: vec![ScopeElement { name: "work/secret".into(), delegable: true }],
            valid_from: T0.into(),
            valid_until: T1.into(),
            parent: Some("g-000000000002".into()),
        };
        assert_eq!(ledger.issue(bad, T0), Err(GrantError::NotDelegable("work/secret".into())));
    }

    #[test]
    fn revocation_closes_the_grant_and_everything_downstream_of_it() {
        let mut ledger = GrantLedger::default();
        ledger
            .issue(root_grant(&urn("root"), vec![ScopeElement { name: "work/all".into(), delegable: true }]), T0)
            .unwrap();
        ledger
            .issue(CapabilityGrant {
                grant_id: "g-000000000002".into(),
                grantor: urn("root"),
                grantee: urn("mid"),
                scopes: vec![ScopeElement { name: "work/all".into(), delegable: false }],
                valid_from: T0.into(),
                valid_until: T1.into(),
                parent: Some("g-000000000001".into()),
            }, T0)
            .unwrap();
        ledger.revoke("g-000000000001").unwrap();
        assert!(!ledger.is_open("g-000000000001", T0).unwrap());
        // A new child under the revoked root is refused.
        let child = CapabilityGrant {
            grant_id: "g-000000000003".into(),
            grantor: urn("root"),
            grantee: urn("late"),
            scopes: vec![ScopeElement { name: "work/all".into(), delegable: false }],
            valid_from: T0.into(),
            valid_until: T1.into(),
            parent: Some("g-000000000001".into()),
        };
        assert_eq!(ledger.issue(child, T0), Err(GrantError::ClosedGrant("g-000000000001".into())));
    }

    #[test]
    fn windows_and_ids_are_checked() {
        let mut ledger = GrantLedger::default();
        // A backwards window is rejected ab initio, at construction.
        let backwards = CapabilityGrant::charter(
            "g-000000000009",
            "urn:hacp:agent:charter-1",
            &urn("x"),
            vec![ScopeElement { name: "s".into(), delegable: true }],
            T1,
            T0,
        );
        assert!(matches!(backwards, Err(GrantError::BadWindow { .. })));
        let bad_id = CapabilityGrant::charter(
            "g-short",
            "urn:hacp:agent:charter-1",
            &urn("x"),
            vec![],
            T0,
            T1,
        );
        assert!(matches!(bad_id, Err(GrantError::BadId(_))));
        let _ = &mut ledger;
    }

    #[test]
    fn chains_and_lca_walk_the_declared_org() {
        let mut org = OrgChart::default();
        // root -> {p1 -> c1, c2}, root -> {p2 -> c3}
        for (child, parent) in [
            ("p1", "root"),
            ("p2", "root"),
            ("c1", "p1"),
            ("c2", "p1"),
            ("c3", "p2"),
        ] {
            org.parent_of.insert(urn(child), urn(parent));
        }
        assert_eq!(org.lca(&urn("c1"), &urn("c2")), Some(urn("p1")));
        assert_eq!(org.lca(&urn("c1"), &urn("c3")), Some(urn("root")));
        assert_eq!(org.lca(&urn("c1"), &urn("root")), Some(urn("root")));
        assert!(org.shares_parent(&urn("c1"), &urn("c2")));
        assert!(!org.shares_parent(&urn("c1"), &urn("c3")));
        assert_eq!(org.children(&urn("p1")), vec![urn("c1"), urn("c2")]);
    }

    #[test]
    fn cross_branch_permits_come_from_the_lca_or_preauthorization() {
        let mut org = OrgChart::default();
        for (child, parent) in [("p1", "root"), ("p2", "root"), ("c1", "p1"), ("c3", "p2")] {
            org.parent_of.insert(urn(child), urn(parent));
        }
        let request = CollaborationRequest {
            request_id: "cr-000000000001".into(),
            requester: urn("c1"),
            peer: urn("c3"),
            task_class: "review".into(),
            expires: T1.into(),
        };
        // Only the LCA may issue; a mere parent of one side may not.
        assert!(matches!(
            CollaborationPermit::by_lca(&org, &request, &urn("p1")),
            Err(CrossBranchError::NotLca { .. })
        ));
        let permit = CollaborationPermit::by_lca(&org, &request, &urn("root")).unwrap();
        assert_eq!(permit.basis, PermitBasis::Lca { supervisor: urn("root") });

        // Preauthorization: a standing cross-branch/review grant for c1.
        let mut ledger = GrantLedger::default();
        assert!(matches!(
            CollaborationPermit::by_preauthorization(&ledger, &request, T0),
            Err(CrossBranchError::NoPreauthorization { .. })
        ));
        ledger
            .issue(root_grant(&urn("c1"), vec![ScopeElement { name: "cross-branch/review".into(), delegable: false }]), T0)
            .unwrap();
        let preauth = CollaborationPermit::by_preauthorization(&ledger, &request, T0).unwrap();
        assert!(matches!(preauth.basis, PermitBasis::Preauthorization { .. }));

        // A permit authorizes exactly its pair, until it expires.
        assert!(permit.authorizes(&urn("c1"), &urn("c3"), T0).is_ok());
        assert!(permit.authorizes(&urn("c3"), &urn("c1"), T0).is_ok());
        assert!(matches!(
            permit.authorizes(&urn("c1"), &urn("c2"), T0),
            Err(CrossBranchError::WrongPair(..))
        ));
        assert!(matches!(
            permit.authorizes(&urn("c1"), &urn("c3"), "2027-01-01T00:00:00Z"),
            Err(CrossBranchError::Expired(_))
        ));
    }
}
