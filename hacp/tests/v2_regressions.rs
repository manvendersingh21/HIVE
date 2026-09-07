//! Independent functional acceptance tests against the current public APIs.
//! No real CLI, model, SSH worker, or production data is used.

#[cfg(test)]
mod tests {
    use hacp::v2::{CapabilityGrant, GrantLedger, HiveProfile, OrgChart, ScopeElement};
    use hacp::v2::{Contract, ContractLimits, ContractState, Relationship, Session, Task, Verdict};
    use serde_json::json;

    const A: &str = "urn:hacp:agent:parent";
    const B: &str = "urn:hacp:agent:child";
    const T0: &str = "2026-09-05T00:00:00Z";
    const T1: &str = "2026-09-06T00:00:00Z";
    const T2: &str = "2026-09-07T00:00:00Z";

    fn contract(session: &Session) -> Result<Contract, hacp::v2::ContractError> {
        Contract::propose(
            session,
            "c-audit",
            Task {
                task_id: "t-audit".into(),
                summary: "write one line".into(),
                owner: B.into(),
            },
            Relationship::Delegation,
            vec![A.into()],
            ContractLimits {
                max_rounds: 3,
                max_amendments: 2,
            },
        )
    }

    fn active() -> Session {
        let mut s = Session::open("s-audit", A, B).unwrap();
        s.accept(B).unwrap();
        s
    }

    fn executing() -> Contract {
        let mut c = contract(&active()).unwrap();
        let terms = json!({"output": "one line", "budget": 5});
        c.agree(A, &terms).unwrap();
        c.agree(B, &terms).unwrap();
        c.freeze(terms).unwrap();
        c
    }

    #[test]
    fn freeze_uses_the_terms_both_parties_accepted() {
        let mut c = contract(&active()).unwrap();
        let agreed = json!({"output": "one line", "budget": 5});
        c.agree(A, &agreed).unwrap();
        c.agree(B, &agreed).unwrap();
        assert!(
            c.freeze(json!({"output": "whole book", "budget": 500}))
                .is_err(),
            "freeze accepted terms different from both recorded acceptances"
        );
    }

    #[test]
    fn disagreement_on_terms_does_not_reach_agreed() {
        let mut c = contract(&active()).unwrap();
        c.agree(A, &json!({"budget": 5})).unwrap();
        let result = c.agree(B, &json!({"budget": 500}));
        assert!(
            result.is_err() || c.state != ContractState::Agreed,
            "different terms were treated as mutual agreement"
        );
    }

    #[test]
    fn amendment_requires_agreement_on_the_same_revision() {
        let mut c = executing();
        c.propose_amendment(A).unwrap();
        c.decide_amendment(A, true, Some(json!({"budget": 6})))
            .unwrap();
        let result = c.decide_amendment(B, true, Some(json!({"budget": 900})));
        assert!(
            result.is_err() || c.state == ContractState::Amending,
            "the second participant's different terms became revision 2"
        );
    }

    #[test]
    fn amendment_supports_the_specified_counterproposal_loop() {
        let mut c = executing();
        c.propose_amendment(A).unwrap();
        assert!(
            c.counter(B).is_ok(),
            "§7.6's amendment negotiation cannot counter"
        );
    }

    #[test]
    fn closed_sessions_cannot_form_new_contracts() {
        let mut s = active();
        s.close(A, "finished").unwrap();
        assert!(
            contract(&s).is_err(),
            "contract proposal succeeded inside a closed session"
        );
    }

    fn ledger(delegable: bool, parent_end: &str, child_end: &str) -> GrantLedger {
        let mut l = GrantLedger::default();
        l.issue(
            CapabilityGrant::charter(
                "g-000000000001",
                "urn:hacp:agent:deployment",
                A,
                vec![ScopeElement {
                    name: "work/all".into(),
                    delegable,
                }],
                T0,
                parent_end,
            )
            .unwrap(),
            T0,
        )
        .unwrap();
        if delegable {
            l.issue(
                CapabilityGrant {
                    grant_id: "g-000000000002".into(),
                    grantor: A.into(),
                    grantee: B.into(),
                    scopes: vec![ScopeElement {
                        name: "work/all".into(),
                        delegable: true,
                    }],
                    valid_from: T0.into(),
                    valid_until: child_end.into(),
                    parent: Some("g-000000000001".into()),
                },
                T0,
            )
            .unwrap();
        }
        l
    }

    #[test]
    fn revoking_a_parent_closes_existing_descendant_grants() {
        let mut l = ledger(true, T2, T2);
        assert!(l.covers(B, "work/all", T0));
        l.revoke("g-000000000001").unwrap();
        assert!(
            !l.covers(B, "work/all", T0),
            "existing child remains open after parent revocation"
        );
    }

    #[test]
    fn descendant_authority_does_not_outlive_its_parent() {
        let l = ledger(true, T1, T2);
        assert!(
            !l.covers(B, "work/all", T2),
            "child remains authorized after its parent's expiry"
        );
    }

    #[test]
    fn profile_checks_delegability_before_sibling_preauthorization() {
        let mut l = ledger(false, T2, T2);
        let mut org = OrgChart::default();
        org.parent_of.insert(B.into(), A.into());
        let result =
            HiveProfile.authorize_siblings(&mut l, &org, A, B, "review", "g-000000000003", T0, T2);
        assert!(
            result.is_err(),
            "non-delegable work/all still issued a cross-branch grant"
        );
    }

    #[test]
    fn working_core_amendment_rework_and_settlement() {
        let mut c = executing();
        let old = c.frozen_digest().unwrap().to_string();
        c.propose_amendment(A).unwrap();
        let terms = json!({"output": "two lines", "budget": 6});
        assert!(c
            .decide_amendment(A, true, Some(terms.clone()))
            .unwrap()
            .is_none());
        let new = c.decide_amendment(B, true, Some(terms)).unwrap().unwrap();
        assert_ne!(old, new);
        assert_eq!(c.revisions.len(), 2);
        let submission = hacp::v2::Submission {
            against_revision: new,
            artifacts: vec!["artifact-ref".into()],
            evidence: vec![],
            claim: "done".into(),
        };
        c.submit(B, submission.clone()).unwrap();
        c.decide(Verdict::Rework {
            scope: "fix line two".into(),
        })
        .unwrap();
        assert_eq!(c.state, ContractState::Executing);
        assert_eq!(c.rework_scope.as_deref(), Some("fix line two"));
        c.submit(B, submission).unwrap();
        c.decide(Verdict::Accept).unwrap();
        assert_eq!(c.state, ContractState::Settled);
    }

    #[test]
    fn rejected_terms_leave_consensus_unchanged_and_counter_resets_it() {
        let mut c = contract(&active()).unwrap();
        let terms = json!({"budget": 5});
        c.agree(A, &terms).unwrap();
        let before = c.clone();
        assert!(c.agree(B, &json!({"budget": 6})).is_err());
        assert_eq!(c, before);
        c.agree(A, &terms).unwrap(); // duplicate acceptance is not two votes
        assert_ne!(c.state, ContractState::Agreed);
        c.counter(B).unwrap();
        let new = json!({"budget": 6});
        c.agree(B, &new).unwrap();
        c.agree(A, &new).unwrap();
        assert!(c.freeze(terms).is_err());
        c.freeze(new).unwrap();
    }

    #[test]
    fn consensus_survives_serialization_but_legacy_unbound_votes_do_not() {
        let mut c = contract(&active()).unwrap();
        let terms = json!({"a": 1, "b": 2});
        c.agree(A, &terms).unwrap();
        let snapshot = serde_json::to_value(&c).unwrap();
        let mut restored: Contract = serde_json::from_value(snapshot.clone()).unwrap();
        assert!(restored.agree(B, &json!({"a": 9})).is_err());
        let reordered = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        restored.agree(B, &reordered).unwrap();
        restored.freeze(terms.clone()).unwrap();

        let mut old = snapshot;
        old.as_object_mut().unwrap().remove("agreed_terms_digest");
        let mut restored: Contract = serde_json::from_value(old).unwrap();
        restored.agree(B, &terms).unwrap();
        assert!(restored.freeze(terms.clone()).is_err());
        restored.agree(A, &terms).unwrap();
        restored.freeze(terms).unwrap();
    }

    #[test]
    fn missing_or_noncanonical_amendment_terms_do_not_record_a_vote() {
        let mut c = executing();
        c.propose_amendment(A).unwrap();
        let before = c.clone();
        assert!(c.decide_amendment(A, true, None).is_err());
        assert_eq!(c, before);
        assert!(c
            .decide_amendment(A, true, Some(json!({"float": 1.5})))
            .is_err());
        assert_eq!(c, before);
        c.decide_amendment(B, true, Some(json!({"budget": 6})))
            .unwrap();
        assert_eq!(c.state, ContractState::Amending);
        c.decide_amendment(A, false, None).unwrap();
        assert_eq!(c.state, ContractState::Executing);
        assert_eq!(c.revisions, before.revisions);
    }

    #[test]
    fn amendment_counters_clear_votes_and_exhaust_at_the_bound() {
        let mut c = executing();
        c.propose_amendment(A).unwrap();
        c.decide_amendment(A, true, Some(json!({"budget": 6})))
            .unwrap();
        c.counter(B).unwrap();
        c.decide_amendment(B, true, Some(json!({"budget": 7})))
            .unwrap();
        assert_eq!(c.state, ContractState::Amending);
        c.counter(A).unwrap();
        assert!(c.counter(B).is_err());
        assert_eq!(c.state, ContractState::NoAgreement);
        assert_eq!(c.revisions.len(), 1);
    }

    #[test]
    fn opening_and_abandoned_sessions_cannot_form_contracts() {
        let mut s = Session::open("s-audit", A, B).unwrap();
        assert!(contract(&s).is_err());
        s.abandon("timeout").unwrap();
        assert!(contract(&s).is_err());
    }

    #[test]
    fn expired_amendments_end_in_no_agreement_without_rewriting_history() {
        let mut c = executing();
        let revisions = c.revisions.clone();
        c.propose_amendment(A).unwrap();
        c.expire_negotiation().unwrap();
        assert_eq!(c.state, ContractState::NoAgreement);
        assert_eq!(c.revisions, revisions);
    }

    #[test]
    fn revoked_ancestors_prevent_new_grandchild_grants() {
        let mut l = ledger(true, T2, T2);
        l.revoke("g-000000000001").unwrap();
        let grandchild = CapabilityGrant {
            grant_id: "g-000000000003".into(),
            grantor: B.into(),
            grantee: "urn:hacp:agent:grandchild".into(),
            scopes: vec![ScopeElement {
                name: "work/all".into(),
                delegable: false,
            }],
            valid_from: T0.into(),
            valid_until: T2.into(),
            parent: Some("g-000000000002".into()),
        };
        assert!(l.issue(grandchild, T0).is_err());
        assert!(l.held_by(B, T0).is_empty());
    }

    #[test]
    fn grant_ids_are_idempotent_but_never_replace_content_or_revocation() {
        let mut l = ledger(true, T2, T2);
        let original = l.held_by(A, T0)[0].clone();
        l.issue(original.clone(), T0).unwrap();
        let mut changed = original.clone();
        changed.grantee = B.into();
        assert!(l.issue(changed, T0).is_err());
        assert!(l.covers(A, "work/all", T0));
        l.revoke(&original.grant_id).unwrap();
        l.issue(original.clone(), T0).unwrap();
        assert!(!l.is_open(&original.grant_id, T0).unwrap());
    }

    fn cross_branch(delegable: bool) -> (GrantLedger, OrgChart) {
        let mut l = GrantLedger::default();
        l.issue(
            CapabilityGrant::charter(
                "g-000000000010",
                "urn:hacp:agent:deployment",
                A,
                vec![ScopeElement {
                    name: "cross-branch/review".into(),
                    delegable,
                }],
                T0,
                T1,
            )
            .unwrap(),
            T0,
        )
        .unwrap();
        let mut org = OrgChart::default();
        org.parent_of.insert(B.into(), A.into());
        org.parent_of.insert("urn:hacp:agent:peer".into(), A.into());
        (l, org)
    }

    #[test]
    fn sibling_preauthorization_preserves_exact_scope_delegability_and_ancestry() {
        let (mut l, org) = cross_branch(false);
        assert!(HiveProfile
            .authorize_siblings(&mut l, &org, A, B, "review", "g-000000000011", T0, T2)
            .is_err());
        let (mut l, org) = cross_branch(true);
        assert!(HiveProfile
            .authorize_siblings(&mut l, &org, A, B, "deploy", "g-000000000011", T0, T2)
            .is_err());
        HiveProfile
            .authorize_siblings(&mut l, &org, A, B, "review", "g-000000000011", T0, T2)
            .unwrap();
        assert_eq!(
            l.held_by(B, T0)[0].parent.as_deref(),
            Some("g-000000000010")
        );
        assert!(l.covers(B, "cross-branch/review", T0));
        assert!(!l.covers(B, "cross-branch/review", T2));
        l.revoke("g-000000000010").unwrap();
        assert!(!l.covers(B, "cross-branch/review", T0));
    }

    #[test]
    fn generic_work_scope_does_not_mint_cross_branch_authority() {
        let mut l = ledger(true, T2, T2);
        let mut org = OrgChart::default();
        org.parent_of.insert(B.into(), A.into());
        assert!(HiveProfile
            .authorize_siblings(&mut l, &org, A, B, "review", "g-000000000003", T0, T2)
            .is_err());
    }

    #[test]
    fn organizational_cycles_are_rejected_without_looping() {
        let mut org = OrgChart::default();
        org.parent_of.insert(A.into(), B.into());
        org.parent_of.insert(B.into(), A.into());
        assert!(org.checked_chain(A).is_err());
        assert!(org.validate().is_err());
        assert!(org.chain(A).is_empty());
        assert!(org.lca(A, B).is_none());
        assert!(HiveProfile.validate_org(&org).is_err());
    }

    #[test]
    fn permits_are_bounded_by_ancestor_expiry_and_recheck_revocation_and_class() {
        use hacp::v2::{CollaborationPermit, CollaborationRequest};
        let (mut l, org) = cross_branch(true);
        HiveProfile
            .authorize_siblings(&mut l, &org, A, B, "review", "g-000000000011", T0, T2)
            .unwrap();
        let request = CollaborationRequest {
            request_id: "cr-000000000001".into(),
            requester: B.into(),
            peer: "urn:hacp:agent:peer".into(),
            task_class: "review".into(),
            expires: T2.into(),
        };
        let p = CollaborationPermit::by_preauthorization(&l, &request, T0).unwrap();
        assert_eq!(p.expires, T1);
        p.authorize_session(&org, &l, B, &request.peer, "review", T0)
            .unwrap();
        assert!(p
            .authorize_session(&org, &l, B, &request.peer, "deploy", T0)
            .is_err());
        assert!(p.authorizes(B, &request.peer, T2).is_err());
        assert!(p.authorizes(B, &request.peer, "2026-09-05").is_err());
        l.revoke("g-000000000010").unwrap();
        assert!(p
            .authorize_session(&org, &l, B, &request.peer, "review", T0)
            .is_err());
    }

    #[test]
    fn lca_permits_recheck_current_organizational_relationships() {
        use hacp::v2::{CollaborationPermit, CollaborationRequest};
        let (l, mut org) = cross_branch(true);
        let mut request = CollaborationRequest {
            request_id: "cr-000000000001".into(),
            requester: B.into(),
            peer: "urn:hacp:agent:peer".into(),
            task_class: "review".into(),
            expires: T1.into(),
        };
        let p = CollaborationPermit::by_lca(&org, &request, A).unwrap();
        p.authorize_session(&org, &l, B, &request.peer, "review", T0)
            .unwrap();
        org.parent_of.remove(B);
        assert!(p
            .authorize_session(&org, &l, B, &request.peer, "review", T0)
            .is_err());
        request.expires = "not-a-timestamp".into();
        assert!(CollaborationPermit::by_lca(&org, &request, A).is_err());
    }

    #[test]
    fn all_envelope_fields_must_be_canonical_and_body_must_be_an_object() {
        let mut e = hacp::v2::Envelope::new("s-audit", A, B, "future.kind", json!({}));
        e.extra.insert("future".into(), json!({"n": 1}));
        e.validate().unwrap(); // unknown kinds and extensions remain valid
        let roundtrip =
            serde_json::from_str::<hacp::v2::Envelope>(&serde_json::to_string(&e).unwrap())
                .unwrap();
        assert_eq!(roundtrip, e);
        e.extra.insert("future".into(), json!({"n": 0.5}));
        assert!(e.validate().is_err());
        e.extra.clear();
        e.extra
            .insert("from".into(), json!("urn:hacp:agent:imposter"));
        assert!(e.validate().is_err());
        e.extra.clear();
        e.body = json!([]);
        assert!(e.validate().is_err());
    }

    fn pending() -> (Contract, hacp::v2::Verification) {
        let mut c = executing();
        let revision = c.frozen_digest().unwrap().to_string();
        let artifacts = vec!["urn:hacp:artifact:9f0d0e6a-1111-4222-8333-444444444444".into()];
        c.submit(
            B,
            hacp::v2::Submission {
                against_revision: revision.clone(),
                artifacts: artifacts.clone(),
                evidence: vec![],
                claim: "done".into(),
            },
        )
        .unwrap();
        let v = hacp::v2::Verification::decide(
            "v-000000000001",
            A,
            &c.contract_id,
            &revision,
            artifacts,
            vec![],
            vec![hacp::v2::Check {
                name: "content matches".into(),
                passed: true,
                detail: "measured bytes".into(),
            }],
            Verdict::Accept,
            vec![],
            vec![],
        )
        .unwrap();
        (c, v)
    }

    #[test]
    fn failed_checks_cannot_be_hidden_among_passing_checks() {
        let (mut c, mut v) = pending();
        v.checks.push(hacp::v2::Check {
            name: "required line count".into(),
            passed: false,
            detail: "two lines".into(),
        });
        assert!(v.validate().is_err());
        assert!(c.apply_verification(&v).is_err());
        assert_eq!(c.state, ContractState::Verifying);
        v.verdict = Verdict::Rework {
            scope: "fix line count".into(),
        };
        c.apply_verification(&v).unwrap();
        assert_eq!(c.state, ContractState::Executing);
    }

    #[test]
    fn verification_is_bound_to_contract_revision_artifacts_and_counterparty() {
        let (mut c, v) = pending();
        for field in ["contract", "revision", "artifacts", "verifier", "outsider"] {
            let mut wrong = v.clone();
            match field {
                "contract" => wrong.contract_id = "another-contract".into(),
                "revision" => wrong.against_revision = "0".repeat(64),
                "artifacts" => wrong.artifacts.push("another-artifact".into()),
                "verifier" => wrong.verifier = B.into(),
                _ => wrong.verifier = "urn:hacp:agent:outsider".into(),
            }
            let before = c.clone();
            assert!(c.apply_verification(&wrong).is_err(), "{field}");
            assert_eq!(c, before);
        }
        c.apply_verification(&v).unwrap();
        assert_eq!(c.state, ContractState::Settled);
        assert!(c.apply_verification(&v).is_err()); // binding deduplicates messages
    }

    #[test]
    fn artifacts_require_a_real_revision_digest() {
        let result = hacp::v2::Artifact::new(
            "urn:hacp:artifact:9f0d0e6a-1111-4222-8333-444444444444",
            "text/plain",
            &"a".repeat(64),
            6,
            B,
            "t-audit",
            "c-audit",
            "TBD",
            "result.txt",
        );
        assert!(result.is_err());
    }

    #[test]
    fn verification_checks_actual_submitter_not_just_task_owner() {
        let (mut c, mut v) = pending();
        let submission = c.pending_submission.clone().unwrap();
        c.decide(Verdict::Rework {
            scope: "retry".into(),
        })
        .unwrap();
        c.submit(A, submission).unwrap();
        assert!(c.apply_verification(&v).is_err());
        v.verifier = B.into();
        c.apply_verification(&v).unwrap();
        assert_eq!(c.state, ContractState::Settled);
    }

    #[test]
    fn pending_submitter_survives_restore_but_legacy_snapshots_fail_closed() {
        let (c, v) = pending();
        let mut snapshot = serde_json::to_value(&c).unwrap();
        let mut restored: Contract = serde_json::from_value(snapshot.clone()).unwrap();
        restored.apply_verification(&v).unwrap();
        snapshot
            .as_object_mut()
            .unwrap()
            .remove("pending_submitter");
        let mut restored: Contract = serde_json::from_value(snapshot).unwrap();
        assert!(restored.apply_verification(&v).is_err());
        assert_eq!(restored.state, ContractState::Verifying);
    }

    #[test]
    fn legacy_permits_without_task_class_need_reissue() {
        use hacp::v2::{CollaborationPermit, CollaborationRequest};
        let (l, org) = cross_branch(true);
        let request = CollaborationRequest {
            request_id: "cr-000000000001".into(),
            requester: B.into(),
            peer: "urn:hacp:agent:peer".into(),
            task_class: "review".into(),
            expires: T1.into(),
        };
        let p = CollaborationPermit::by_lca(&org, &request, A).unwrap();
        let mut snapshot = serde_json::to_value(p).unwrap();
        snapshot.as_object_mut().unwrap().remove("task_class");
        let restored: CollaborationPermit = serde_json::from_value(snapshot).unwrap();
        assert!(restored
            .authorize_session(&org, &l, B, &request.peer, "review", T0)
            .is_err());
    }

    #[test]
    fn grant_lookups_fail_closed_for_noncanonical_times() {
        let l = ledger(true, T2, T2);
        for time in [
            "",
            "2026-09-05",
            "2026-09-05T00:00:00+00:00",
            "2026-09-05T00:00:00.1Z",
        ] {
            assert!(l.is_open("g-000000000001", time).is_err());
            assert!(!l.covers(B, "work/all", time));
        }
    }
}
