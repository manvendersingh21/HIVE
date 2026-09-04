//! The HACP Recursive Pairwise Profile (`spec/HACP-2.0-draft.md` §13),
//! version `hive-recursive-pairwise/1`.
//!
//! A profile is declared in capability negotiation and binds only the
//! deployments that declare it. Everything normative about max-two-children,
//! LCA routing, and sibling preauthorization lives **here and only here**
//! (ADR-0001 §6.1): a deployment with 40 flat workers and no supervision is
//! fully Core-conformant. Holding the profile rules out of Core is what keeps
//! Core lab-neutral.

use super::grant::{GrantLedger, OrgChart, ScopeElement};

/// The profile id, as it appears in capability negotiation.
pub const HIVE_PROFILE: &str = "hive-recursive-pairwise/1";

/// §13: supervisory arity is capped; growth goes downward.
pub const MAX_DIRECT_SUBORDINATES: usize = 2;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("agent {agent:?} has {count} direct subordinates; this profile caps arity at {max} (§13)")]
    ArityExceeded { agent: String, count: usize, max: usize },
    #[error("the profile must be declared by both session participants; {0:?} did not")]
    NotDeclared(String),
}

/// Validation and conveniences for deployments that declared the profile.
#[derive(Debug, Default, Clone, Copy)]
pub struct HiveProfile;

impl HiveProfile {
    /// Does this session's feature sets bind the profile? Both participants
    /// must declare it (§2: profiles bind only deployments that declare them).
    pub fn binds(features_a: &[String], features_b: &[String]) -> bool {
        features_a.iter().any(|f| f == HIVE_PROFILE) && features_b.iter().any(|f| f == HIVE_PROFILE)
    }

    /// Arity check over the org chart: no agent may exceed
    /// [`MAX_DIRECT_SUBORDINATES`] direct subordinates.
    pub fn validate_org(&self, org: &OrgChart) -> Result<(), ProfileError> {
        let mut supervisors: Vec<&String> = org.parent_of.values().collect();
        supervisors.sort();
        supervisors.dedup();
        for supervisor in supervisors {
            let count = org.children(supervisor).len();
            if count > MAX_DIRECT_SUBORDINATES {
                return Err(ProfileError::ArityExceeded {
                    agent: supervisor.clone(),
                    count,
                    max: MAX_DIRECT_SUBORDINATES,
                });
            }
        }
        Ok(())
    }

    /// §13 sibling preauthorization: a supervisor MAY issue a standing
    /// `cross-branch/<class>` grant to one child, at its discretion, so
    /// siblings can form collaboration sessions for named task classes
    /// without a per-session LCA ruling (§10.2 path two). The supervisor
    /// must itself hold the scope delegable.
    pub fn authorize_siblings(
        &self,
        ledger: &mut GrantLedger,
        org: &OrgChart,
        supervisor: &str,
        child: &str,
        task_class: &str,
        grant_id: &str,
        valid_from: &str,
        valid_until: &str,
    ) -> Result<(), super::grant::GrantError> {
        if !org.children(supervisor).iter().any(|c| c == child) {
            return Err(super::grant::GrantError::ChainMismatch {
                grantor: supervisor.to_string(),
                grantee: child.to_string(),
            });
        }
        if !ledger.covers(supervisor, &format!("cross-branch/{task_class}"), valid_from)
            && !ledger.covers(supervisor, "work/all", valid_from)
        {
            return Err(super::grant::GrantError::NotDelegable(format!(
                "cross-branch/{task_class}"
            )));
        }
        ledger.issue(
            super::grant::CapabilityGrant {
                grant_id: grant_id.to_string(),
                grantor: supervisor.to_string(),
                grantee: child.to_string(),
                scopes: vec![ScopeElement {
                    name: format!("cross-branch/{task_class}"),
                    delegable: false,
                }],
                valid_from: valid_from.to_string(),
                valid_until: valid_until.to_string(),
                parent: None,
            },
            valid_from,
        )
    }

    /// §13 role independence (SHOULD, not MUST): the Supervisor role and the
    /// Verifier role are held by different agents where the deployment can
    /// afford it. Advisory — returns whether the deployment took the advice.
    pub fn supervisor_verifier_independent(supervisor: &str, verifier: &str) -> bool {
        supervisor != verifier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urn(n: &str) -> String {
        format!("urn:hacp:agent:{n}")
    }

    const T0: &str = "2026-09-04T00:00:00Z";
    const T1: &str = "2026-12-31T23:59:59Z";

    #[test]
    fn arity_two_is_profile_law_not_core_law() {
        let mut org = OrgChart::default();
        org.parent_of.insert(urn("c1"), urn("boss"));
        org.parent_of.insert(urn("c2"), urn("boss"));
        assert!(HiveProfile.validate_org(&org).is_ok());

        org.parent_of.insert(urn("c3"), urn("boss"));
        assert_eq!(
            HiveProfile.validate_org(&org),
            Err(ProfileError::ArityExceeded {
                agent: urn("boss"),
                count: 3,
                max: 2,
            })
        );
        // The same chart breaks no Core rule: OrgChart itself has no cap.
    }

    #[test]
    fn the_profile_binds_only_deployments_that_declare_it() {
        let a = vec!["supervision".to_string(), HIVE_PROFILE.to_string()];
        let b = vec![HIVE_PROFILE.to_string()];
        assert!(HiveProfile::binds(&a, &b));
        let b_core_only = vec!["delegation".to_string()];
        assert!(!HiveProfile::binds(&a, &b_core_only));
    }

    #[test]
    fn siblings_may_hold_standing_cross_branch_grants() {
        let mut org = OrgChart::default();
        org.parent_of.insert(urn("c1"), urn("p1"));
        org.parent_of.insert(urn("c2"), urn("p1"));

        let mut ledger = GrantLedger::default();
        // p1 holds delegable work/all, chartered by the deployment.
        ledger
            .issue(
                super::super::grant::CapabilityGrant::charter(
                    "g-000000000001",
                    "urn:hacp:agent:charter-1",
                    &urn("p1"),
                    vec![ScopeElement { name: "work/all".into(), delegable: true }],
                    T0,
                    T1,
                )
                .unwrap(),
                T0,
            )
            .unwrap();

        // c1 gets a standing cross-branch/review grant; c2 does not.
        HiveProfile
            .authorize_siblings(&mut ledger, &org, &urn("p1"), &urn("c1"), "review",
                "g-000000000002", T0, T1)
            .unwrap();
        assert_eq!(ledger.preauthorizes(&urn("c1"), "review", T0), Some("g-000000000002".into()));
        assert_eq!(ledger.preauthorizes(&urn("c2"), "review", T0), None);

        // A supervisor cannot preauthorize someone else's child.
        assert!(matches!(
            HiveProfile.authorize_siblings(&mut ledger, &org, &urn("p1"), &urn("c9"), "review",
                "g-000000000003", T0, T1),
            Err(super::super::grant::GrantError::ChainMismatch { .. })
        ));
    }

    #[test]
    fn role_independence_is_advice() {
        assert!(HiveProfile::supervisor_verifier_independent(&urn("s"), &urn("v")));
        assert!(!HiveProfile::supervisor_verifier_independent(&urn("s"), &urn("s")));
    }
}
