//! The HACP/2.0 envelope, kind registry, and agent URNs —
//! `spec/HACP-2.0-draft.md` §3, §5.
//!
//! Carried from 1.1 because they earned it: kinds are strings, not a closed enum,
//! and bodies are [`serde_json::Value`], so unknown kinds and unknown fields
//! round-trip (§5.2) instead of being rejected. New in 2.0: every envelope names
//! its session, and every field is validated against the canonical-form rules so
//! that what a peer receives is what a digest can be taken of.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canon;

/// The protocol identifier this version stamps and requires.
pub const PROTOCOL: &str = "HACP/2.0";

/// The registry of kinds a 2.0 implementation emits (§5.3). Membership is what the
/// *reference* implementation speaks, never what it will hear: an unregistered kind
/// is delivered, per the forward-compatibility rule carried from 1.1 §6.
///
/// Deliberately absent: any `execute` kind. EXECUTE is a contract lifecycle state
/// entered implicitly on freeze (§7.5); it needs no message, and a registry entry
/// would quietly become one.
pub mod kinds {
    pub const SESSION_OPEN: &str = "session.open";
    pub const SESSION_FEATURES: &str = "session.features";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const CONTRACT_PROPOSED: &str = "contract.proposed";
    pub const CONTRACT_COUNTERED: &str = "contract.countered";
    pub const CONTRACT_ACCEPTED: &str = "contract.accepted";
    pub const CONTRACT_FROZEN: &str = "contract.frozen";
    pub const CONTRACT_AMENDMENT_PROPOSED: &str = "contract.amendment.proposed";
    pub const CONTRACT_AMENDMENT_DECIDED: &str = "contract.amendment.decided";
    pub const CONTRACT_WITHDRAWN: &str = "contract.withdrawn";
    pub const CONTRACT_NO_AGREEMENT: &str = "contract.no_agreement";
    pub const SUBMISSION_DELIVERED: &str = "submission.delivered";
    pub const VERIFICATION_DELIVERED: &str = "verification.delivered";
    pub const ESCALATION_RAISED: &str = "escalation.raised";
    pub const ESCALATION_REFERRED: &str = "escalation.referred";
    pub const ESCALATION_RESOLVED: &str = "escalation.resolved";
    pub const ESCALATION_NO_AGREEMENT: &str = "escalation.no_agreement";
    pub const COLLABORATION_REQUEST: &str = "collaboration.request";
    pub const COLLABORATION_PERMIT: &str = "collaboration.permit";
    pub const HEARTBEAT: &str = "heartbeat";
    pub const ERROR: &str = "error";

    /// Every kind the reference implementation emits, in registry order.
    pub const ALL: &[&str] = &[
        SESSION_OPEN,
        SESSION_FEATURES,
        SESSION_CLOSE,
        CONTRACT_PROPOSED,
        CONTRACT_COUNTERED,
        CONTRACT_ACCEPTED,
        CONTRACT_FROZEN,
        CONTRACT_AMENDMENT_PROPOSED,
        CONTRACT_AMENDMENT_DECIDED,
        CONTRACT_WITHDRAWN,
        CONTRACT_NO_AGREEMENT,
        SUBMISSION_DELIVERED,
        VERIFICATION_DELIVERED,
        ESCALATION_RAISED,
        ESCALATION_REFERRED,
        ESCALATION_RESOLVED,
        ESCALATION_NO_AGREEMENT,
        COLLABORATION_REQUEST,
        COLLABORATION_PERMIT,
        HEARTBEAT,
        ERROR,
    ];

    /// The lifecycle kinds an observer may be granted (§6.2): the transitions that
    /// require mutual knowledge, never the traffic between them.
    pub const LIFECYCLE_EVENTS: &[&str] = &[
        CONTRACT_FROZEN,
        CONTRACT_AMENDMENT_DECIDED,
        SUBMISSION_DELIVERED,
        VERIFICATION_DELIVERED,
        SESSION_CLOSE,
        ESCALATION_RAISED,
    ];

    pub fn is_registered(kind: &str) -> bool {
        ALL.contains(&kind)
    }
}

/// A vendor-neutral agent identifier (§3): `urn:hacp:agent:<local-name>`.
///
/// The local name is 1–64 characters of `[a-z0-9._-]`. What it may never encode —
/// the tool behind the agent — is a rule the checker can only approximate; the
/// charset at least refuses the obvious vendor strings' shape (spaces, slashes,
/// version colons) and the opacity rule stays procedural, as in 1.1 §3.
pub mod agent_urn {
    const PREFIX: &str = "urn:hacp:agent:";

    /// Mint an agent URN from a local name, validating it.
    pub fn mint(local_name: &str) -> Result<String, String> {
        validate_local_name(local_name)?;
        Ok(format!("{PREFIX}{local_name}"))
    }

    /// Validate shape and return the local name.
    pub fn parse(urn: &str) -> Result<&str, String> {
        let rest = urn
            .strip_prefix(PREFIX)
            .ok_or_else(|| format!("not an agent URN: {urn:?}"))?;
        validate_local_name(rest)?;
        Ok(rest)
    }

    fn validate_local_name(name: &str) -> Result<(), String> {
        let len = name.len();
        if !(1..=64).contains(&len) {
            return Err(format!("agent local name must be 1-64 chars, got {len}"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(format!(
                "agent local name {name:?} may use only [a-z0-9._-]"
            ));
        }
        Ok(())
    }
}

/// Why an envelope is not valid. The field is named in every case: an invalid
/// envelope is a wire bug, and a wire bug that does not say where is a second bug.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EnvelopeError {
    #[error("protocol must be {required}, found {found:?}")]
    WrongProtocol { required: &'static str, found: String },
    #[error("message_id {found:?} must be \"m-\" followed by at least 12 lowercase hex characters")]
    BadMessageId { found: String },
    #[error("session_id must be a non-empty string")]
    BadSessionId,
    #[error("from {found:?}: {reason}")]
    BadFrom { found: String, reason: String },
    #[error("to {found:?}: {reason}")]
    BadTo { found: String, reason: String },
    #[error("kind must be a non-empty string")]
    BadKind,
    #[error("timestamp: {0}")]
    BadTimestamp(String),
    #[error("in_reply_to {found:?} must have the message_id shape")]
    BadInReplyTo { found: String },
    #[error("body: {0}")]
    BadBody(String),
}

/// The HACP/2.0 wire unit (§5.2).
///
/// `extra` carries every field this build does not know, so re-serialization
/// preserves them — the forward-compatibility rule, mechanically enforced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    /// `"HACP/2.0"`.
    pub protocol: String,
    /// `"m-"` + ≥12 lowercase hex, unique per message.
    pub message_id: String,
    /// The session this message belongs to (§6). On `session.open` it names the
    /// prospective session.
    pub session_id: String,
    /// Author: an agent URN, and a participant of the session (enforced by the
    /// session engine, not the shape).
    pub from: String,
    /// Addressee: an agent URN.
    pub to: String,
    /// Registry string; unknown kinds are valid on receipt (§5.3).
    pub kind: String,
    /// Canonical timestamp: `YYYY-MM-DDTHH:MM:SSZ` (§5.1).
    pub timestamp: String,
    /// The message being answered, when this message is an answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// Kind-specific body; must be canonicalizable (integers only, §5.1).
    pub body: Value,
    /// Fields this build does not know, preserved on re-serialization (§5.2).
    #[serde(flatten, skip_serializing_if = "std::collections::BTreeMap::is_empty", default)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

impl Envelope {
    /// Mint an envelope with a fresh message id and the current canonical
    /// timestamp. Everything else is the caller's semantics.
    pub fn new(
        session_id: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        kind: impl Into<String>,
        body: Value,
    ) -> Self {
        Self {
            protocol: PROTOCOL.to_string(),
            message_id: format!("m-{}", uuid::Uuid::new_v4().simple()),
            session_id: session_id.into(),
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            timestamp: canon::canonical_now(),
            in_reply_to: None,
            body,
            extra: Default::default(),
        }
    }

    /// Validate shape against §5.1–§5.3. Delivery MUST NOT depend on the kind
    /// being registered; the registry is what the reference emits.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.protocol != PROTOCOL {
            return Err(EnvelopeError::WrongProtocol {
                required: PROTOCOL,
                found: self.protocol.clone(),
            });
        }
        if !is_message_id_shape(&self.message_id) {
            return Err(EnvelopeError::BadMessageId {
                found: self.message_id.clone(),
            });
        }
        if self.session_id.is_empty() {
            return Err(EnvelopeError::BadSessionId);
        }
        let from = agent_urn::parse(&self.from).map_err(|reason| EnvelopeError::BadFrom {
            found: self.from.clone(),
            reason,
        })?;
        let _ = from;
        agent_urn::parse(&self.to).map_err(|reason| EnvelopeError::BadTo {
            found: self.to.clone(),
            reason,
        })?;
        if self.kind.is_empty() {
            return Err(EnvelopeError::BadKind);
        }
        canon::validate_timestamp(&self.timestamp)
            .map_err(|e| EnvelopeError::BadTimestamp(e.to_string()))?;
        if let Some(reply) = &self.in_reply_to {
            if !is_message_id_shape(reply) {
                return Err(EnvelopeError::BadInReplyTo {
                    found: reply.clone(),
                });
            }
        }
        canon::canonical_json(&self.body)
            .map(|_| ())
            .map_err(|e| EnvelopeError::BadBody(e.to_string()))
    }
}

fn is_message_id_shape(s: &str) -> bool {
    let Some(hex) = s.strip_prefix("m-") else {
        return false;
    };
    hex.len() >= 12 && hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> Envelope {
        Envelope::new(
            "s-000000000001",
            agent_urn::mint("a-1").unwrap(),
            agent_urn::mint("b-1").unwrap(),
            kinds::SESSION_OPEN,
            json!({"greeting": true}),
        )
    }

    #[test]
    fn a_freshly_minted_envelope_validates() {
        valid().validate().unwrap();
    }

    #[test]
    fn message_ids_and_replies_must_have_the_registry_shape() {
        let mut e = valid();
        e.message_id = "m-Short".into();
        assert!(matches!(e.validate(), Err(EnvelopeError::BadMessageId { .. })));
        e.message_id = "x-000000000000".into();
        assert!(matches!(e.validate(), Err(EnvelopeError::BadMessageId { .. })));
        e.message_id = "m-000000000ABC".into();
        assert!(matches!(e.validate(), Err(EnvelopeError::BadMessageId { .. })));
        e.message_id = "m-000000000000".into();
        e.in_reply_to = Some("not-an-id".into());
        assert!(matches!(e.validate(), Err(EnvelopeError::BadInReplyTo { .. })));
    }

    #[test]
    fn a_future_kind_is_valid_on_receipt() {
        let mut e = valid();
        e.kind = "some.future.kind".into();
        e.validate().unwrap();
        assert!(!kinds::is_registered("some.future.kind"));
        assert!(kinds::is_registered(kinds::CONTRACT_FROZEN));
    }

    #[test]
    fn the_registry_names_no_execute_kind() {
        assert!(
            !kinds::ALL.iter().any(|k| k.contains("execute")),
            "EXECUTE is a lifecycle state, not a message (§7.5)"
        );
    }

    #[test]
    fn unknown_fields_round_trip_verbatim() {
        let raw = json!({
            "protocol": PROTOCOL,
            "message_id": "m-000000000000",
            "session_id": "s-1",
            "from": agent_urn::mint("a-1").unwrap(),
            "to": agent_urn::mint("b-1").unwrap(),
            "kind": kinds::HEARTBEAT,
            "timestamp": "2026-09-04T12:00:00Z",
            "body": {},
            "shimmer": {"future": [1, 2]}
        });
        let envelope: Envelope = serde_json::from_value(raw.clone()).unwrap();
        envelope.validate().unwrap();
        assert_eq!(envelope.extra.get("shimmer"), Some(&json!({"future": [1, 2]})));
        let reserialized = serde_json::to_value(&envelope).unwrap();
        assert_eq!(reserialized, raw, "re-serialization must not lose the future");
    }

    #[test]
    fn a_float_body_is_rejected_at_validation_not_at_digest_time() {
        let mut e = valid();
        e.body = json!({"confidence": 0.5});
        assert!(matches!(e.validate(), Err(EnvelopeError::BadBody(_))));
    }

    #[test]
    fn agent_local_names_have_a_shape() {
        assert!(agent_urn::mint("worker-a.1_2").is_ok());
        for bad in ["", "CamelCase", "has space", "slash/one", &"x".repeat(65)] {
            assert!(agent_urn::mint(bad).is_err(), "accepted: {bad:?}");
        }
        assert_eq!(
            agent_urn::parse("urn:hacp:agent:worker-a").unwrap(),
            "worker-a"
        );
        assert!(agent_urn::parse("urn:hacp:coordinator:hive").is_err());
    }
}
