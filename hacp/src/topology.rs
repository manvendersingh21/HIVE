//! Formation: team sizing, decomposition, and topology (`spec/HACP.md` §4, §7).
//!
//! This is what 1.0 left off-protocol. The number of workers is a decision derived from
//! the goal's structure, and `TaskDecomposition` is that decision made auditable: an
//! implementation that always emits the same count is detectably non-conformant.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The communication shape a run uses (§4). Determined by `agent_count`, not chosen
/// independently of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Topology {
    /// One worker; no contract required.
    Solo,
    /// Two workers addressing each other directly; arbiter optional.
    Peer,
    /// Three or more; arbiter required, peers still address each other directly.
    Federated,
}

impl Topology {
    /// The topology §4 requires for `n` workers.
    pub fn for_agent_count(n: usize) -> Self {
        match n {
            0 | 1 => Topology::Solo,
            2 => Topology::Peer,
            _ => Topology::Federated,
        }
    }

    /// Whether §4 requires an arbiter. An unarbitrated disagreement among three or more
    /// parties has no bounded resolution, which is the whole reason for the threshold.
    pub fn requires_arbiter(&self) -> bool {
        matches!(self, Topology::Federated)
    }

    /// Whether workers may address each other directly.
    pub fn allows_peer_traffic(&self) -> bool {
        !matches!(self, Topology::Solo)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Topology::Solo => "solo",
            Topology::Peer => "peer",
            Topology::Federated => "federated",
        }
    }
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The body of `run.plan` — how the goal was broken up and why that many agents.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskDecomposition {
    pub decomposition_id: String,
    /// The goal, verbatim.
    pub goal: String,
    /// The arbiter's reading of the goal's structure.
    #[serde(default)]
    pub analysis: String,
    pub components: Vec<Component>,
    pub roles: Vec<RoleSpec>,
    pub agent_count: usize,
    pub topology: Topology,
    /// Why **this many** agents — not merely how many. Required by §7.
    pub rationale: String,
}

/// Why a decomposition failed validation (§7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecompositionError {
    #[error("agent_count {count} does not match roles.length {roles}")]
    CountMismatch { count: usize, roles: usize },
    #[error("topology {topology} is not the one §4 requires for {count} agents ({required})")]
    TopologyMismatch { topology: Topology, count: usize, required: Topology },
    #[error("component {0:?} is assigned to no role")]
    UnassignedComponent(String),
    #[error("component {component:?} is assigned to {count} roles; it must be exactly one")]
    MultiplyAssignedComponent { component: String, count: usize },
    #[error("role {role:?} references unknown component {component:?}")]
    UnknownComponent { role: String, component: String },
    #[error("component {consumer:?} consumes {artifact:?}, which nothing produces")]
    UnproducedArtifact { consumer: String, artifact: String },
    #[error("rationale must say why this many agents")]
    MissingRationale,
    #[error("a decomposition must name at least one role; a team of nobody does no work")]
    EmptyTeam,
    #[error("role {0:?} is assigned no component, inflating the count §7 exists to make honest")]
    IdleRole(String),
}

impl TaskDecomposition {
    /// Validate the decomposition (§7).
    pub fn validate(&self) -> Result<(), DecompositionError> {
        if self.agent_count != self.roles.len() {
            return Err(DecompositionError::CountMismatch {
                count: self.agent_count,
                roles: self.roles.len(),
            });
        }
        let required = Topology::for_agent_count(self.agent_count);
        if self.topology != required {
            return Err(DecompositionError::TopologyMismatch {
                topology: self.topology,
                count: self.agent_count,
                required,
            });
        }
        if self.rationale.trim().is_empty() {
            return Err(DecompositionError::MissingRationale);
        }
        // Zero roles satisfies every rule below vacuously: the count matches, the
        // topology for 0 is Solo, and there are no components to leave unassigned.
        // Without this the emptiest possible answer to the run's central question
        // validates cleanly.
        if self.roles.is_empty() {
            return Err(DecompositionError::EmptyTeam);
        }
        // An agent with nothing to do inflates agent_count, which is the one number
        // §7 exists to keep honest. The component walk below only goes
        // components->roles, so it can never see a role no component points back at.
        for r in &self.roles {
            if r.components.is_empty() {
                return Err(DecompositionError::IdleRole(r.role_id.clone()));
            }
        }

        // Every component assigned to exactly one role.
        for c in &self.components {
            let n = self
                .roles
                .iter()
                .filter(|r| r.components.contains(&c.component_id))
                .count();
            match n {
                1 => {}
                0 => {
                    return Err(DecompositionError::UnassignedComponent(c.component_id.clone()))
                }
                _ => {
                    return Err(DecompositionError::MultiplyAssignedComponent {
                        component: c.component_id.clone(),
                        count: n,
                    })
                }
            }
        }
        for r in &self.roles {
            for c in &r.components {
                if !self.components.iter().any(|x| &x.component_id == c) {
                    return Err(DecompositionError::UnknownComponent {
                        role: r.role_id.clone(),
                        component: c.clone(),
                    });
                }
            }
        }
        // Every consumed artifact is produced by something.
        for c in &self.components {
            for want in &c.consumes {
                if !self.components.iter().any(|x| x.produces.contains(want)) {
                    return Err(DecompositionError::UnproducedArtifact {
                        consumer: c.component_id.clone(),
                        artifact: want.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Every capability any role needs — what the coordinator must find agents for.
    pub fn required_capabilities(&self) -> Vec<&str> {
        let mut caps: Vec<&str> = self
            .roles
            .iter()
            .flat_map(|r| r.required_capabilities.iter().map(String::as_str))
            .collect();
        caps.sort_unstable();
        caps.dedup();
        caps
    }
}

/// One independently assignable piece of the goal.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Component {
    pub component_id: String,
    pub description: String,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Artifact ids this component produces.
    #[serde(default)]
    pub produces: Vec<String>,
    /// Artifact ids this component consumes.
    #[serde(default)]
    pub consumes: Vec<String>,
}

/// One role: the components a single worker takes on.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoleSpec {
    pub role_id: String,
    pub components: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

/// Body of `role.offer`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoleOfferBody {
    pub role_id: String,
    pub description: String,
    #[serde(default)]
    pub produces: Vec<String>,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    pub round: u32,
}

/// Body of `role.accepted` / `role.declined` (§6). A role that cannot be taken must be
/// refusable; 1.0 had no way to say no.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoleResponseBody {
    pub role_id: String,
    /// Set on decline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Provenance a worker's adapter declares in `hello` (§8).
///
/// Declarative, not proof: an adapter cannot introspect a stock tool. **Admission MUST
/// NOT be gated on these; assignment MAY be informed by them.** That distinction is the
/// whole content of §8, and it is what lets capability discovery be useful without
/// becoming a gate that locks capable agents out.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CapabilityManifest {
    pub agent: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_declared_by")]
    pub declared_by: String,
    /// Additional provenance. MUST NOT carry vendor, product, or model identity (§3).
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_declared_by() -> String {
    "adapter-default".to_string()
}

impl CapabilityManifest {
    pub fn declares(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// Whether the worker was briefed to write its own report (§10). Its absence tells
    /// the coordinator to expect an adapter-synthesized one.
    pub fn writes_own_report(&self) -> bool {
        self.declares("report-json")
    }
}
