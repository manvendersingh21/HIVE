//! Disputes and rulings (`spec/HACP.md` §4, §6).
//!
//! This is what makes the arbiter an authority rather than a message router. Workers
//! resolve what they can between themselves over `peer.*`; what they cannot, they
//! escalate here, and the ruling binds.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Body of `dispute.raised`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DisputeRaised {
    /// An artifact id, or the literal `"goal"`.
    pub about: String,
    /// Each party's position, in that party's own words. A dispute raised by one agent
    /// still records what it understands the other to be claiming, so the arbiter can
    /// see the disagreement rather than only one side of it.
    pub positions: Vec<Position>,
    /// The precise question the arbiter is asked to settle.
    pub question: String,
}

/// One party's stated position in a dispute.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Position {
    pub agent: String,
    pub position: String,
}

/// Body of `dispute.ruling`. Binding on every agent in `binds`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DisputeRuling {
    pub about: String,
    /// What is now true, stated so an agent can act on it without further discussion.
    pub decision: String,
    /// Why. Required: an unexplained ruling is indistinguishable from a coin toss, and
    /// a worker that cannot see the reasoning cannot apply it to the next case.
    pub rationale: String,
    /// The agents bound by this ruling.
    pub binds: Vec<String>,
}
