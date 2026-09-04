//! Post-freeze contract evolution (`spec/HACP.md` §9.2).
//!
//! 1.0 froze a contract permanently: after `contract.frozen`, any amendment was rejected
//! and any change to a frozen `interface_files` was simply a violation. That is brittle
//! on exactly the goals HACP exists for — the ones where no participant knows the right
//! interface in advance.
//!
//! 1.1 keeps freeze meaningful and adds one controlled door. An *undeclared* change is
//! still a violation, caught by the digest check (§11.3). A *declared* one goes through
//! [`ChangeRequest`], and every consumer of the changed artifact is told
//! ([`InterfaceImpacted`]) rather than discovering it at integration.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contract::InterfaceContract;

/// Body of `contract.change.requested`.
///
/// A worker that finds it must change a frozen interface sends this and MUST NOT change
/// the file first.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChangeRequest {
    pub artifact_id: String,
    /// What the worker wants to change.
    pub change: String,
    /// Why the frozen interface cannot stand.
    pub reason: String,
    /// The requester's own assessment of whether consumers must adapt. Advisory: the
    /// arbiter decides, and per C2 a worker's self-assessment is never evidence.
    #[serde(default)]
    pub breaking: bool,
}

/// Body of `contract.amended` — the full contract at its new version, with fresh digests.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractAmended {
    pub contract: InterfaceContract,
    pub new_version: u32,
    pub interface_digests: BTreeMap<String, String>,
    /// The artifact ids whose interfaces actually moved.
    pub changed: Vec<String>,
}

/// Body of `interface.impacted` — sent to the **consumers** of a changed artifact, and
/// to no one else (§9.2).
///
/// This is the protocol's answer to "detect when one agent's work affects another".
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InterfaceImpacted {
    pub artifact_id: String,
    /// The digest at the previous freeze or amendment.
    pub was_digest: String,
    /// The digest now in force.
    pub now_digest: String,
    /// What changed, in terms a consumer can act on.
    pub what_changed: String,
    /// What the consumer must do. "Nothing" is a valid and useful answer.
    pub action_required: String,
}

/// Body of `rework.requested` (§11.1).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReworkRequested {
    pub report_id: String,
    /// The failed checks, verbatim from the verdict. A worker cannot repair what it is
    /// only told "failed".
    pub failed_checks: Vec<crate::report::CheckResult>,
    pub round: u32,
    pub rounds_remaining: u32,
}

/// Body of `rework.completed`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReworkCompleted {
    pub report_id: String,
    pub summary: String,
}
