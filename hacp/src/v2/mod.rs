//! HACP/2.0 — the clean-draft namespace beside the frozen 1.1 implementation.
//!
//! Everything in this module tree implements
//! [`spec/HACP-2.0-draft.md`](../spec/HACP-2.0-draft.md) (see its section-freeze
//! ledger) and [`docs/adr/ADR-0001`](../../../docs/adr/ADR-0001-hacp-core-is-bilateral.md):
//! bilateral sessions and contracts, addressable artifacts, evidence, verification,
//! escalation semantics — with no organizational shape in the protocol and no
//! implementation dependency in this crate. The 1.1 modules above stay frozen as the
//! reference baseline; nothing here may alter them.

pub mod canon;
pub mod schema;
