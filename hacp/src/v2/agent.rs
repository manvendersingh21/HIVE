//! Agents and capability advertisement — `spec/HACP-2.0-draft.md` §3, §6.3.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The feature identifiers of capability negotiation (§6.3). A participant that
/// did not declare a feature must never have it relied upon in that session; the
/// identifier set is the vocabulary for that rule, kept closed and small.
pub mod features {
    /// Can hold the supervisory side of a delegation (§8).
    pub const SUPERVISION: &str = "supervision";
    /// Can be granted authority under a delegation (§8).
    pub const DELEGATION: &str = "delegation";
    /// Can be granted observer subscriptions (§6.2).
    pub const OBSERVER_EVENTS: &str = "observer-events";
    /// Produces artifact digests in the manifest shape (§9.1).
    pub const ARTIFACT_DIGEST: &str = "artifact-digest";
    /// May be permitted cross-branch sessions under a permit (§10).
    pub const CROSS_BRANCH: &str = "cross-branch";

    pub const ALL: &[&str] = &[
        SUPERVISION,
        DELEGATION,
        OBSERVER_EVENTS,
        ARTIFACT_DIGEST,
        CROSS_BRANCH,
    ];
}

/// A durable identity with advertised capabilities (§3). Advertisement is
/// provenance, not proof: it informs matching, never admission — carried from
/// 1.1 §8 because the reasoning did not change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Agent {
    /// `urn:hacp:agent:<local-name>` (§3).
    pub id: String,
    /// Declared feature identifiers (§6.3) and free-form descriptors. A
    /// capability token is `[a-z0-9-]{1,32}`; anything a deployment invents
    /// beyond [`features::ALL`] negotiates as an unknown feature, which peers
    /// accept and cannot rely on.
    pub capabilities: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AgentError {
    #[error("agent id: {0}")]
    BadId(String),
    #[error("capability {found:?} must be 1-32 chars of [a-z0-9-]")]
    BadCapability { found: String },
}

impl Agent {
    pub fn new(id: &str) -> Result<Self, AgentError> {
        super::envelope::agent_urn::parse(id)
            .map_err(AgentError::BadId)
            .map(|_| Self {
                id: id.to_string(),
                capabilities: BTreeSet::new(),
            })
    }

    pub fn with_capabilities(
        id: &str,
        capabilities: &[&str],
    ) -> Result<Self, AgentError> {
        let mut agent = Self::new(id)?;
        for c in capabilities {
            validate_capability(c)?;
            agent.capabilities.insert(c.to_string());
        }
        Ok(agent)
    }

    /// Advertisement is provenance, not proof (§3): this answers "did the agent
    /// say so", never "can the agent do it".
    pub fn declares(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }
}

fn validate_capability(c: &str) -> Result<(), AgentError> {
    let len = c.len();
    if !(1..=32).contains(&len)
        || !c.chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(AgentError::BadCapability {
            found: c.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_capabilities_have_shapes() {
        let agent = Agent::with_capabilities(
            "urn:hacp:agent:a-1",
            &["supervision", "artifact-digest", "custom-thing"],
        )
        .unwrap();
        assert!(agent.declares(features::SUPERVISION));
        assert!(!agent.declares(features::CROSS_BRANCH));
        assert!(Agent::new("urn:hacp:coordinator:hive").is_err());
        assert!(Agent::with_capabilities("urn:hacp:agent:a-1", &["Bad Cap"]).is_err());
        assert!(Agent::with_capabilities("urn:hacp:agent:a-1", &[&"x".repeat(33)]).is_err());
    }

    #[test]
    fn the_feature_vocabulary_stays_small() {
        assert_eq!(features::ALL.len(), 5);
    }
}
