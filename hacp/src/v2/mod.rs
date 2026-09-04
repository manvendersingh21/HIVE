//! HACP/2.0 — the clean-draft namespace beside the frozen 1.1 implementation.
//!
//! Everything in this module tree implements
//! [`spec/HACP-2.0-draft.md`](../spec/HACP-2.0-draft.md) (see its section-freeze
//! ledger) and [`docs/adr/ADR-0001`](../../../docs/adr/ADR-0001-hacp-core-is-bilateral.md):
//! bilateral sessions and contracts, addressable artifacts, evidence, verification,
//! escalation semantics — with no organizational shape in the protocol and no
//! implementation dependency in this crate. The 1.1 modules above stay frozen as the
//! reference baseline; nothing here may alter them.

pub mod agent;
pub mod artifact;
pub mod canon;
pub mod contract;
pub mod envelope;
pub mod evidence;
pub mod schema;
pub mod session;
pub mod verification;

pub use agent::Agent;
pub use artifact::{Artifact, ArtifactError, Audience, Visibility};
pub use contract::{Contract, ContractError, ContractLimits, ContractState, Relationship, Revision, Submission, Task, Verdict};
pub use envelope::{agent_urn, kinds, Envelope, EnvelopeError, PROTOCOL};
pub use evidence::{Evidence, EvidenceError, EvidenceKind, EvidenceSubject};
pub use session::{ObserverGrant, Session, SessionError, SessionState};
pub use verification::{Check, Verification, VerificationError};
