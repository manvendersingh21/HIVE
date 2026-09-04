//! Envelope, actor URNs, and the message-kind registry — `spec/HACP.md` §3, §5, §6.
//!
//! Two properties of the specification shape this module and must not be lost in
//! refactors:
//!
//! * **Forward compatibility.** [`MessageKind`] is a string newtype, not a closed enum,
//!   and bodies are [`serde_json::Value`]. Unknown kinds and unknown body fields MUST
//!   round-trip so a newer minor version's messages are persisted and delivered, never
//!   rejected (§5).
//! * **Vendor neutrality.** Actor URNs carry no vendor, product, or model identity; the
//!   URN-to-tool mapping lives only in the coordinator's private run record (§3).

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The protocol identifier this build stamps on outbound envelopes.
pub const PROTOCOL: &str = "HACP/1.1";

/// The major protocol version this build speaks. Differing majors are rejected;
/// higher minors are accepted, because minor versions are additive (§5).
pub const PROTOCOL_MAJOR: u8 = 1;

/// The minor protocol version this build speaks.
pub const PROTOCOL_MINOR: u8 = 1;

// ---------------------------------------------------------------------------
// Actor URNs (§3)
// ---------------------------------------------------------------------------

/// Vendor-neutral actor identifiers.
///
/// A worker learns that a peer exists and what it produces. It never learns who builds
/// that peer, which model runs it, or how to reach it outside the bus.
pub mod urn {
    use super::PROTOCOL_MAJOR;

    /// Every participant on the run. Valid only in an envelope's `to` field.
    pub const ALL: &str = "urn:hacp:all";

    const AGENT_PREFIX: &str = "urn:hacp:agent:";
    const COORDINATOR_PREFIX: &str = "urn:hacp:coordinator:";
    const ARBITER_PREFIX: &str = "urn:hacp:arbiter:";

    /// The coordinator's URN — the mechanical, content-blind role.
    pub fn coordinator(name: &str) -> String {
        format!("{COORDINATOR_PREFIX}{name}")
    }

    /// The arbiter's URN — the reasoning authority, optional at N=2 (§4).
    pub fn arbiter(name: &str) -> String {
        format!("{ARBITER_PREFIX}{name}")
    }

    /// A worker's URN: `urn:hacp:agent:<role>-<run_short>`.
    pub fn agent(role: &str, run_short: &str) -> String {
        format!("{AGENT_PREFIX}{role}-{run_short}")
    }

    /// Split an agent URN back into `(role, run_short)`, if it is one.
    ///
    /// Role ids may themselves contain `-` (`job-store`), so the split is on the
    /// **last** hyphen: the run-short suffix is the unambiguous part.
    pub fn parse_agent(urn: &str) -> Option<(String, String)> {
        let rest = urn.strip_prefix(AGENT_PREFIX)?;
        let (role, run) = rest.rsplit_once('-')?;
        if role.is_empty() || run.is_empty() {
            return None;
        }
        Some((role.to_string(), run.to_string()))
    }

    /// Whether this URN names a worker.
    pub fn is_agent(urn: &str) -> bool {
        parse_agent(urn).is_some()
    }

    /// Whether this URN names the coordinator.
    pub fn is_coordinator(urn: &str) -> bool {
        urn.starts_with(COORDINATOR_PREFIX)
    }

    /// Whether this URN names the arbiter.
    pub fn is_arbiter(urn: &str) -> bool {
        urn.starts_with(ARBITER_PREFIX)
    }

    /// Whether this URN is the broadcast address.
    pub fn is_broadcast(urn: &str) -> bool {
        urn == ALL
    }

    /// Whether a URN carries no vendor, product, or model identity — i.e. whether it is
    /// one of the four shapes §3 permits at all.
    ///
    /// This is the mechanical half of §3. It cannot catch a deployment that names its
    /// coordinator after a product, which is why §3 states the rule in prose as well.
    pub fn is_neutral(urn: &str) -> bool {
        is_agent(urn) || is_coordinator(urn) || is_arbiter(urn) || is_broadcast(urn)
    }

    /// Whether this protocol version string is one this build accepts: same major, any
    /// minor (§5).
    pub fn protocol_supported(protocol: &str) -> bool {
        let Some(v) = protocol.strip_prefix("HACP/") else {
            return false;
        };
        let Some((major, minor)) = v.split_once('.') else {
            return false;
        };
        // A well-formed minor is required even though its value is not gated on:
        // "HACP/1.x" is malformed, not a future minor.
        if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        major.parse::<u8>().map(|m| m == PROTOCOL_MAJOR).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Message kinds (§6)
// ---------------------------------------------------------------------------

/// A message kind. A string newtype, **not** a closed enum: unknown kinds MUST be
/// persisted and delivered, never rejected (§5). This is the protocol's only
/// forward-compatibility mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct MessageKind(pub String);

impl MessageKind {
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this kind is in the v1.1 registry. A `false` here means "relay it
    /// untouched", never "reject it".
    pub fn is_registered(&self) -> bool {
        kinds::REGISTRY.contains(&self.0.as_str())
    }

    /// Whether this is peer-to-peer traffic, which the coordinator delivers only to its
    /// endpoints and the arbiter (§6).
    pub fn is_peer(&self) -> bool {
        self.0.starts_with("peer.")
    }
}

impl std::fmt::Display for MessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The v1.1 kind registry (§6). Constants rather than an enum, so a future minor
/// version adds a kind without a breaking change here.
pub mod kinds {
    // session
    pub const HELLO: &str = "hello";
    pub const HEARTBEAT: &str = "heartbeat";
    // run
    pub const RUN_STARTED: &str = "run.started";
    pub const RUN_PLAN: &str = "run.plan";
    pub const RUN_COMPLETED: &str = "run.completed";
    pub const RUN_FAILED: &str = "run.failed";
    // roles
    pub const ROLE_OFFER: &str = "role.offer";
    pub const ROLE_ACCEPTED: &str = "role.accepted";
    pub const ROLE_DECLINED: &str = "role.declined";
    // contract
    pub const CONTRACT_DRAFTED: &str = "contract.drafted";
    pub const CONTRACT_AMENDMENT: &str = "contract.amendment";
    pub const CONTRACT_AMENDMENT_ACCEPTED: &str = "contract.amendment.accepted";
    pub const CONTRACT_AMENDMENT_REJECTED: &str = "contract.amendment.rejected";
    pub const CONTRACT_FROZEN: &str = "contract.frozen";
    // peer
    pub const PEER_QUESTION: &str = "peer.question";
    pub const PEER_ANSWER: &str = "peer.answer";
    pub const PEER_PROPOSAL: &str = "peer.proposal";
    pub const PEER_ACCEPTED: &str = "peer.accepted";
    pub const PEER_REJECTED: &str = "peer.rejected";
    // dispute
    pub const DISPUTE_RAISED: &str = "dispute.raised";
    pub const DISPUTE_RULING: &str = "dispute.ruling";
    // evolution
    pub const CONTRACT_CHANGE_REQUESTED: &str = "contract.change.requested";
    pub const CONTRACT_AMENDED: &str = "contract.amended";
    pub const INTERFACE_IMPACTED: &str = "interface.impacted";
    // work
    pub const WORK_STARTED: &str = "work.started";
    pub const ARTIFACT_PUBLISHED: &str = "artifact.published";
    // mediation
    pub const QUESTION: &str = "question";
    pub const ANSWER: &str = "answer";
    // reporting
    pub const REPORT_SUBMITTED: &str = "report.submitted";
    pub const REPORT_VERDICT: &str = "report.verdict";
    // rework
    pub const REWORK_REQUESTED: &str = "rework.requested";
    pub const REWORK_COMPLETED: &str = "rework.completed";
    // error
    pub const ERROR_PROTOCOL: &str = "error.protocol";

    /// Every kind in the v1.1 registry, for shape checks and documentation. Membership
    /// is informational: §5 forbids rejecting a kind for being absent.
    pub const REGISTRY: &[&str] = &[
        HELLO, HEARTBEAT,
        RUN_STARTED, RUN_PLAN, RUN_COMPLETED, RUN_FAILED,
        ROLE_OFFER, ROLE_ACCEPTED, ROLE_DECLINED,
        CONTRACT_DRAFTED, CONTRACT_AMENDMENT, CONTRACT_AMENDMENT_ACCEPTED,
        CONTRACT_AMENDMENT_REJECTED, CONTRACT_FROZEN,
        PEER_QUESTION, PEER_ANSWER, PEER_PROPOSAL, PEER_ACCEPTED, PEER_REJECTED,
        DISPUTE_RAISED, DISPUTE_RULING,
        CONTRACT_CHANGE_REQUESTED, CONTRACT_AMENDED, INTERFACE_IMPACTED,
        WORK_STARTED, ARTIFACT_PUBLISHED,
        QUESTION, ANSWER,
        REPORT_SUBMITTED, REPORT_VERDICT,
        REWORK_REQUESTED, REWORK_COMPLETED,
        ERROR_PROTOCOL,
    ];

    /// Kinds that REQUIRE `in_reply_to` (§5).
    pub const REQUIRES_IN_REPLY_TO: &[&str] = &[
        ANSWER, PEER_ANSWER, ROLE_ACCEPTED, ROLE_DECLINED,
        CONTRACT_AMENDMENT_ACCEPTED, CONTRACT_AMENDMENT_REJECTED,
        DISPUTE_RULING, REPORT_VERDICT, REWORK_COMPLETED,
    ];
}

// ---------------------------------------------------------------------------
// Envelope (§5)
// ---------------------------------------------------------------------------

/// One message on the bus. Bodies are opaque here; the typed bodies live beside the
/// structures they carry (see [`crate::contract`], [`crate::topology`], …).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    /// `"HACP/<major>.<minor>"`.
    pub protocol: String,
    /// Unique message id (`m-<uuid>`); the dedupe key for at-least-once edges.
    pub message_id: String,
    /// The run this message belongs to.
    pub run_id: String,
    /// Sender URN. An adapter only ever emits its own agent URN here, and the
    /// coordinator enforces that against the sender's credentials (§13.3).
    pub from: String,
    /// Recipient URN, or [`urn::ALL`] for broadcast.
    pub to: String,
    /// Registry kind. Unknown kinds round-trip untouched.
    pub kind: MessageKind,
    /// Causal link; REQUIRED on the kinds in [`kinds::REQUIRES_IN_REPLY_TO`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// RFC 3339 UTC.
    pub timestamp: DateTime<Utc>,
    /// Kind-specific payload, opaque at the envelope level.
    pub body: serde_json::Value,
}

/// Why an envelope failed its shape check (§5). Never returned for an unknown *kind*.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("unsupported protocol version {found:?}; this build speaks {expected:?}")]
    ProtocolMismatch { found: String, expected: String },
    #[error("field {0} must not be empty")]
    Empty(&'static str),
    #[error("actor URN {0:?} is not a valid HACP actor name")]
    BadActor(String),
    #[error("{0} may not be addressed to the broadcast URN")]
    BroadcastNotAllowed(&'static str),
    #[error("kind {0:?} requires in_reply_to")]
    MissingInReplyTo(String),
}

impl Envelope {
    /// Build an envelope with a fresh message id and the current timestamp.
    pub fn new(
        run_id: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        kind: impl Into<String>,
        body: serde_json::Value,
    ) -> Self {
        Self {
            protocol: PROTOCOL.to_string(),
            message_id: format!("m-{}", Uuid::new_v4()),
            run_id: run_id.into(),
            from: from.into(),
            to: to.into(),
            kind: MessageKind::new(kind),
            in_reply_to: None,
            timestamp: Utc::now(),
            body,
        }
    }

    /// Attach a causal link (builder-style).
    pub fn with_in_reply_to(mut self, id: impl Into<String>) -> Self {
        self.in_reply_to = Some(id.into());
        self
    }

    /// Whether the envelope's protocol version is one this build accepts.
    pub fn protocol_ok(&self) -> bool {
        urn::protocol_supported(&self.protocol)
    }

    /// Whether this message is addressed to every participant.
    pub fn is_broadcast(&self) -> bool {
        urn::is_broadcast(&self.to)
    }

    /// Whether `who` should receive this message.
    ///
    /// Peer traffic is private to its endpoints and the arbiter (§6): a worker MUST NOT
    /// be delivered a peer message it is neither sender nor recipient of, and a
    /// broadcast `peer.*` message is a contradiction that resolves to "endpoints only".
    pub fn deliverable_to(&self, who: &str) -> bool {
        if self.kind.is_peer() {
            return self.to == who || self.from == who || urn::is_arbiter(who);
        }
        self.is_broadcast() || self.to == who
    }

    /// Shape validation (§5). Deliberately does **not** inspect `body`, and deliberately
    /// accepts unknown kinds: content is the arbiter's business, and unknown kinds are
    /// the forward-compatibility mechanism.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if !self.protocol_ok() {
            return Err(EnvelopeError::ProtocolMismatch {
                found: self.protocol.clone(),
                expected: PROTOCOL.to_string(),
            });
        }
        if self.message_id.is_empty() {
            return Err(EnvelopeError::Empty("message_id"));
        }
        if self.run_id.is_empty() {
            return Err(EnvelopeError::Empty("run_id"));
        }
        if !urn::is_neutral(&self.from) {
            return Err(EnvelopeError::BadActor(self.from.clone()));
        }
        if urn::is_broadcast(&self.from) {
            return Err(EnvelopeError::BroadcastNotAllowed("from"));
        }
        if !urn::is_neutral(&self.to) {
            return Err(EnvelopeError::BadActor(self.to.clone()));
        }
        if self.kind.as_str().is_empty() {
            return Err(EnvelopeError::Empty("kind"));
        }
        if kinds::REQUIRES_IN_REPLY_TO.contains(&self.kind.as_str())
            && self.in_reply_to.is_none()
        {
            return Err(EnvelopeError::MissingInReplyTo(self.kind.0.clone()));
        }
        Ok(())
    }
}

/// Body of `error.protocol`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolErrorBody {
    /// Short machine-readable code, e.g. `"contract-frozen"`, `"role-mismatch"`.
    pub code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_versions: Option<Vec<String>>,
}

/// Body of `heartbeat` (§12).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatBody {
    /// Free-form worker-side state label, e.g. `"working"`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
