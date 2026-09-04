//! Bilateral sessions — `spec/HACP-2.0-draft.md` §6.
//!
//! A session is the communication relationship between **exactly two
//! participants** (§6.1) — the type says `[String; 2]`, so "exactly two" is not
//! a runtime convention but a compile-time fact. Optional **observers** (§6.2)
//! subscribe to lifecycle events under an explicit grant; they cannot author,
//! cannot alter state, and their presence is recorded where both participants
//! can see it.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::agent::features;
use super::envelope::kinds;

/// Session lifecycle (§6.4). Sessions are never reopened: parties open a new
/// session with a fresh negotiation instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// `session.open` sent, not yet accepted.
    Opening,
    /// Both participants bound; messages flow, contracts may form.
    Active,
    /// Terminal, by `session.close`.
    Closed,
    /// Terminal, by failure, timeout, or an unresponsive peer.
    Abandoned,
}

/// What an observer was granted: the lifecycle kinds it receives (§6.2), never
/// the traffic between them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ObserverGrant {
    pub kinds: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SessionError {
    #[error("{who:?} is not a participant of session {}", .session)]
    NotAParticipant { who: String, session: String },
    #[error("{who:?} is an observer; observers cannot author messages (§6.2)")]
    ObserverCannotAuthor { who: String },
    #[error("cannot {action} from state {state:?}")]
    IllegalTransition { action: &'static str, state: SessionState },
    #[error("observer {who:?} must not be a participant (§6.2)")]
    ObserverIsParticipant { who: String },
    #[error("observer grants cover lifecycle events only; {found:?} is not one")]
    NotALifecycleEvent { found: String },
    #[error("the opener {who:?} cannot accept its own session.open")]
    OpenerCannotAccept { who: String },
    #[error("participants must differ: a session is between two agents")]
    SameParticipant,
}

/// The bilateral session (§6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub session_id: String,
    /// Exactly two participants (§6.1). The first is the opener.
    pub participants: [String; 2],
    /// Observer subscriptions, recorded and visible (§6.2).
    pub observers: BTreeMap<String, ObserverGrant>,
    pub state: SessionState,
    /// Features each participant declared (§6.3).
    declared: BTreeMap<String, BTreeSet<String>>,
    /// Why a terminal state was reached.
    pub close_reason: Option<String>,
}

impl Session {
    /// Open a prospective session. The opener is `participants[0]`; the
    /// acceptor — the first and only other author — is `participants[1]`.
    pub fn open(session_id: &str, opener: &str, acceptor: &str) -> Result<Self, SessionError> {
        if opener == acceptor {
            return Err(SessionError::SameParticipant);
        }
        for urn in [opener, acceptor] {
            super::envelope::agent_urn::parse(urn)
                .map_err(|reason| SessionError::NotAParticipant {
                    who: urn.to_string(),
                    session: reason,
                })?;
        }
        Ok(Self {
            session_id: session_id.to_string(),
            participants: [opener.to_string(), acceptor.to_string()],
            observers: BTreeMap::new(),
            state: SessionState::Opening,
            declared: BTreeMap::new(),
            close_reason: None,
        })
    }

    /// May `who` author a message in this session? Participants only (§6.2);
    /// observers fail with their own error so the caller can tell refusal of
    /// an outsider from refusal of a subscriber.
    pub fn authorize_author(&self, who: &str) -> Result<(), SessionError> {
        if self.observers.contains_key(who) {
            return Err(SessionError::ObserverCannotAuthor { who: who.to_string() });
        }
        if self.participants.contains(&who.to_string()) {
            return Ok(());
        }
        Err(SessionError::NotAParticipant {
            who: who.to_string(),
            session: self.session_id.clone(),
        })
    }

    /// The second participant accepts (§6.1): `Opening → Active`. Acceptance is
    /// the first message that participant authors — which is why the opener
    /// cannot accept its own open.
    pub fn accept(&mut self, by: &str) -> Result<(), SessionError> {
        self.authorize_author(by)?;
        if self.state != SessionState::Opening {
            return Err(SessionError::IllegalTransition {
                action: "accept",
                state: self.state,
            });
        }
        if by == self.participants[0] {
            return Err(SessionError::OpenerCannotAccept { who: by.to_string() });
        }
        self.state = SessionState::Active;
        Ok(())
    }

    /// Declare features (§6.3). Callable while opening or active — negotiation
    /// rides alongside the handshake rather than gating it, so a mismatch is
    /// discovered when a feature is first relied on, and closes cleanly.
    pub fn declare_features(
        &mut self,
        who: &str,
        features: &[&str],
    ) -> Result<(), SessionError> {
        self.authorize_author(who)?;
        if matches!(self.state, SessionState::Closed | SessionState::Abandoned) {
            return Err(SessionError::IllegalTransition {
                action: "declare features",
                state: self.state,
            });
        }
        let entry = self
            .declared
            .entry(who.to_string())
            .or_default();
        for f in features {
            entry.insert(f.to_string());
        }
        Ok(())
    }

    /// A feature is available in this session only when **both** participants
    /// declared it (§6.3): what one side did not declare, the other side may
    /// not rely on.
    pub fn feature_available(&self, feature: &str) -> bool {
        self.declared
            .get(&self.participants[0])
            .is_some_and(|s| s.contains(feature))
            && self
                .declared
                .get(&self.participants[1])
                .is_some_and(|s| s.contains(feature))
    }

    /// Capability mismatch, stated cleanly (§6.3): an ordinary close, not an
    /// error — the reason string names the missing feature.
    pub fn close_for_capability_mismatch(&mut self, feature: &str) -> Result<(), SessionError> {
        self.close_internal(&format!("capability-mismatch: {feature}"))
    }

    /// Grant an observer subscription (§6.2). Lifecycle events only; a
    /// participant can never also be an observer; requires both sides to have
    /// declared `observer-events` so a participant that cannot host observers
    /// is never forced to.
    pub fn grant_observer(
        &mut self,
        observer: &str,
        kinds: &[&str],
    ) -> Result<(), SessionError> {
        if self.state != SessionState::Active {
            return Err(SessionError::IllegalTransition {
                action: "grant observer",
                state: self.state,
            });
        }
        if self.participants.contains(&observer.to_string()) {
            return Err(SessionError::ObserverIsParticipant {
                who: observer.to_string(),
            });
        }
        for k in kinds {
            if !kinds::LIFECYCLE_EVENTS.contains(k) {
                return Err(SessionError::NotALifecycleEvent { found: k.to_string() });
            }
        }
        if !self.feature_available(features::OBSERVER_EVENTS) {
            return Err(SessionError::IllegalTransition {
                action: "grant observer without the observer-events feature on both sides",
                state: self.state,
            });
        }
        let grant = ObserverGrant {
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
        };
        self.observers.insert(observer.to_string(), grant);
        Ok(())
    }

    /// Whether an observer receives a given kind (§6.2).
    pub fn observer_receives(&self, observer: &str, kind: &str) -> bool {
        self.observers
            .get(observer)
            .is_some_and(|g| g.kinds.contains(kind))
    }

    /// Close (§6.4): terminal by either participant's `session.close`.
    pub fn close(&mut self, by: &str, reason: &str) -> Result<(), SessionError> {
        self.authorize_author(by)?;
        self.close_internal(reason)
    }

    /// Abandon (§6.4): terminal on failure, timeout, or an unresponsive peer —
    /// three missed heartbeats at the binding layer, recorded here as a state.
    pub fn abandon(&mut self, reason: &str) -> Result<(), SessionError> {
        if matches!(self.state, SessionState::Closed | SessionState::Abandoned) {
            return Err(SessionError::IllegalTransition {
                action: "abandon",
                state: self.state,
            });
        }
        self.state = SessionState::Abandoned;
        self.close_reason = Some(reason.to_string());
        Ok(())
    }

    fn close_internal(&mut self, reason: &str) -> Result<(), SessionError> {        if self.state != SessionState::Active {
            return Err(SessionError::IllegalTransition {
                action: "close",
                state: self.state,
            });
        }
        self.state = SessionState::Closed;
        self.close_reason = Some(reason.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::envelope::agent_urn;

    fn urn(name: &str) -> String {
        agent_urn::mint(name).unwrap()
    }

    fn active_session() -> Session {
        let mut s = Session::open("s-1", &urn("a"), &urn("b")).unwrap();
        s.accept(&urn("b")).unwrap();
        s.declare_features(&urn("a"), &[features::OBSERVER_EVENTS]).unwrap();
        s.declare_features(&urn("b"), &[features::OBSERVER_EVENTS, features::ARTIFACT_DIGEST])
            .unwrap();
        s
    }

    #[test]
    fn exactly_two_participants_and_the_opener_cannot_accept() {
        let mut s = Session::open("s-1", &urn("a"), &urn("b")).unwrap();
        assert_eq!(s.participants.len(), 2);
        assert!(matches!(
            s.accept(&urn("a")),
            Err(SessionError::OpenerCannotAccept { .. })
        ));
        s.accept(&urn("b")).unwrap();
        assert_eq!(s.state, SessionState::Active);
        assert!(matches!(
            s.accept(&urn("b")),
            Err(SessionError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn outsiders_cannot_author_and_neither_can_observers() {
        let s = active_session();
        s.authorize_author(&urn("a")).unwrap();
        assert!(matches!(
            s.authorize_author(&urn("stranger")),
            Err(SessionError::NotAParticipant { .. })
        ));
        let mut s = s;
        s.grant_observer(&urn("auditor"), &[kinds::CONTRACT_FROZEN]).unwrap();
        // The observer is recorded and visible to both participants…
        assert!(s.observers.contains_key(&urn("auditor")));
        // …receives only what it was granted…
        assert!(s.observer_receives(&urn("auditor"), kinds::CONTRACT_FROZEN));
        assert!(!s.observer_receives(&urn("auditor"), kinds::SUBMISSION_DELIVERED));
        // …and can never author (§6.2).
        assert!(matches!(
            s.authorize_author(&urn("auditor")),
            Err(SessionError::ObserverCannotAuthor { .. })
        ));
    }

    #[test]
    fn a_participant_cannot_become_an_observer() {
        let mut s = active_session();
        assert!(matches!(
            s.grant_observer(&urn("a"), &[kinds::CONTRACT_FROZEN]),
            Err(SessionError::ObserverIsParticipant { .. })
        ));
    }

    #[test]
    fn observer_grants_cover_lifecycle_events_only() {
        let mut s = active_session();
        assert!(matches!(
            s.grant_observer(&urn("auditor"), &[kinds::HEARTBEAT]),
            Err(SessionError::NotALifecycleEvent { .. })
        ));
        assert!(matches!(
            s.grant_observer(&urn("auditor"), &[kinds::SESSION_OPEN]),
            Err(SessionError::NotALifecycleEvent { .. })
        ));
    }

    #[test]
    fn observers_require_the_feature_on_both_sides() {
        let mut s = Session::open("s-2", &urn("a"), &urn("b")).unwrap();
        s.accept(&urn("b")).unwrap();
        // Only side a declared observer-events: not available (§6.3).
        s.declare_features(&urn("a"), &[features::OBSERVER_EVENTS]).unwrap();
        assert!(!s.feature_available(features::OBSERVER_EVENTS));
        assert!(s.grant_observer(&urn("aud"), &[kinds::CONTRACT_FROZEN]).is_err());
        // Both sides declare it: available, and a grant succeeds.
        s.declare_features(&urn("b"), &[features::OBSERVER_EVENTS]).unwrap();
        assert!(s.feature_available(features::OBSERVER_EVENTS));
    }

    #[test]
    fn capability_mismatch_is_an_ordinary_close() {
        let mut s = active_session();
        s.close_for_capability_mismatch(features::CROSS_BRANCH).unwrap();
        assert_eq!(s.state, SessionState::Closed);
        assert_eq!(
            s.close_reason.as_deref(),
            Some("capability-mismatch: cross-branch")
        );
    }

    #[test]
    fn terminal_states_are_terminal_and_abandonment_is_recorded() {
        let mut s = active_session();
        s.close(&urn("a"), "done").unwrap();
        assert!(matches!(
            s.close(&urn("b"), "again"),
            Err(SessionError::IllegalTransition { .. })
        ));
        let mut s = active_session();
        s.abandon("three missed heartbeats").unwrap();
        assert_eq!(s.state, SessionState::Abandoned);
        assert!(s.abandon("twice").is_err());
    }
}
