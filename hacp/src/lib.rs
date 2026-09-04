//! # HACP — Heterogeneous Agent Collaboration Protocol
//!
//! Wire types, validation, and conformance vectors for HACP/1.1. The specification is
//! [`spec/HACP.md`](../spec/HACP.md), distributed with this crate.
//!
//! HACP lets worker agents built by different organizations, running different models,
//! and invoked through their own stock command-line tools collaborate on one goal by
//! agreeing on **interface contracts** — the decided abstractions between them, carried
//! as protocol artifacts rather than as conversational context.
//!
//! ## This crate is the protocol, not a system that uses one
//!
//! It has no async runtime, no database, no HTTP client, and no dependency on any
//! particular implementation. That is a deliberate boundary, not an accident of
//! packaging: the specification names no product for the same reason, and a system
//! built on HACP is *a* reference implementation, never the definition of the
//! protocol's limits. Adding an implementation dependency here is a design error.
//!
//! What this crate does provide is everything needed to *be* checked: shape validation
//! ([`Envelope::validate`], [`InterfaceContract::validate`],
//! [`TaskDecomposition::validate`]), the canonical interface digest
//! ([`ArtifactSpec::interface_digest`]), the state machine
//! ([`RunState::can_transition_to`]), and the conformance vectors in `tests/`.
//!
//! ## The three constraints that shape everything
//!
//! * **C1 — Workers cannot be modified.** The protocol's edge is files; an adapter owns
//!   protocol behavior on a stock tool's behalf.
//! * **C2 — Claims are not evidence.** Every worker claim is re-verified ([`report`]).
//! * **C3 — Peers do not learn each other's identity.** URNs are vendor-neutral
//!   ([`envelope::urn`]).

pub mod contract;
pub mod dispute;
pub mod envelope;
pub mod evolution;
pub mod report;
pub mod state;
pub mod topology;
pub mod v2;

pub use contract::{
    ArtifactFormat, ArtifactSpec, ContractAmendment, ContractError, Dependency,
    InterfaceContract,
};
pub use envelope::{kinds, urn, Envelope, EnvelopeError, MessageKind, PROTOCOL, PROTOCOL_MAJOR};
pub use report::{CheckResult, CompletionReport, Outcome, ReportSource, VerificationResult};
pub use state::{RunLimits, RunState};
pub use topology::{CapabilityManifest, TaskDecomposition, Topology};
