//! Artifacts and provenance — `spec/HACP-2.0-draft.md` §9.1.
//!
//! An artifact is an addressable object independent of any filesystem: identity,
//! content digest, producer, and provenance (`derived_from`). The artifact graph
//! is queried without knowing who supervised whom (ADR-0001 §4) — provenance
//! edges are artifact-to-artifact, never agent-to-agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Who may see an artifact (§9.1), as a widening ladder: an artifact with
/// visibility V is visible to every audience at or below V.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Visibility {
    /// The contract's two participants only.
    Participants,
    /// Participants plus the supervisory chain.
    Supervisors,
    /// Participants, supervisors, and session observers (§6.2).
    SessionObservers,
    /// Every member of the deployment.
    Deployment,
}

/// The audience a viewer belongs to, on the same ladder as [`Visibility`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Audience {
    Participant,
    Supervisor,
    SessionObserver,
    Deployment,
}

/// An addressable work product (§9.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    /// `urn:hacp:artifact:<uuid4>`.
    pub artifact_id: String,
    /// `type/subtype`, both non-empty tokens (RFC 6838 shape).
    pub media_type: String,
    /// SHA-256 of the content, 64 lowercase hex. Never "TBD" (§9.1).
    pub digest: String,
    /// Size in bytes.
    pub size: u64,
    /// The agent that produced it.
    pub producer: String,
    /// The task it was produced under.
    pub task_id: String,
    /// The contract it was produced under.
    pub contract_id: String,
    /// The frozen revision digest it answers.
    pub contract_revision: String,
    /// Provenance: artifacts this one derives from, by id.
    pub derived_from: Vec<String>,
    /// Binding-specific reference (path, URL, store key).
    pub location: String,
    /// Who may see it (§9.1). Defaults to `participants` at construction.
    pub visibility: Visibility,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ArtifactError {
    #[error("artifact_id {found:?} must be urn:hacp:artifact:<uuid4>")]
    BadId { found: String },
    #[error("digest must be 64 lowercase hex characters, found {found:?}")]
    BadDigest { found: String },
    #[error("media_type {found:?} must be type/subtype with non-empty tokens")]
    BadMediaType { found: String },
    #[error("producer: {0}")]
    BadProducer(String),
    #[error("derived_from entry: {0}")]
    BadDerivedFrom(String),
    #[error("provenance cycle detected involving {0:?}")]
    Cycle(String),
    #[error("unknown artifact {0:?} referenced in provenance")]
    UnknownArtifact(String),
    #[error("location must be a non-empty string")]
    BadLocation,
}

impl Visibility {
    fn level(self) -> u8 {
        match self {
            Visibility::Participants => 0,
            Visibility::Supervisors => 1,
            Visibility::SessionObservers => 2,
            Visibility::Deployment => 3,
        }
    }
}

impl Audience {
    fn level(self) -> u8 {
        match self {
            Audience::Participant => 0,
            Audience::Supervisor => 1,
            Audience::SessionObserver => 2,
            Audience::Deployment => 3,
        }
    }
}

impl Artifact {
    /// Mint an artifact record over known content. The digest is the caller's
    /// to compute honestly — [`crate::v2::canon::digest_canonical`] over the
    /// file's bytes — because this type records, it does not read.
    pub fn new(
        artifact_id: &str,
        media_type: &str,
        digest: &str,
        size: u64,
        producer: &str,
        task_id: &str,
        contract_id: &str,
        contract_revision: &str,
        location: &str,
    ) -> Result<Self, ArtifactError> {
        let artifact = Self {
            artifact_id: artifact_id.to_string(),
            media_type: media_type.to_string(),
            digest: digest.to_string(),
            size,
            producer: producer.to_string(),
            task_id: task_id.to_string(),
            contract_id: contract_id.to_string(),
            contract_revision: contract_revision.to_string(),
            derived_from: Vec::new(),
            location: location.to_string(),
            visibility: Visibility::Participants,
        };
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn validate(&self) -> Result<(), ArtifactError> {
        validate_artifact_id(&self.artifact_id)?;
        if !is_digest_shape(&self.digest) {
            return Err(ArtifactError::BadDigest {
                found: self.digest.clone(),
            });
        }
        let mut parts = self.media_type.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(t), Some(s), None) if !t.is_empty() && !s.is_empty() => {}
            _ => {
                return Err(ArtifactError::BadMediaType {
                    found: self.media_type.clone(),
                })
            }
        }
        crate::v2::envelope::agent_urn::parse(&self.producer)
            .map_err(ArtifactError::BadProducer)?;
        for parent in &self.derived_from {
            validate_artifact_id(parent)
                .map_err(|e| ArtifactError::BadDerivedFrom(e.to_string()))?;
        }
        if self.location.is_empty() {
            return Err(ArtifactError::BadLocation);
        }
        Ok(())
    }

    /// Whether a viewer audience may see this artifact (§9.1 ladder).
    pub fn visible_to(&self, viewer: Audience) -> bool {
        self.visibility.level() >= viewer.level()
    }

    /// Derive a new artifact from this one, recording provenance (§9.1).
    pub fn derive(&self, child: &mut Artifact) {
        child.derived_from.push(self.artifact_id.clone());
    }
}

/// The full ancestry of an artifact over a collection: every transitive
/// `derived_from`, in discovery order. Cycles are refused — carried from
/// 1.1's dependency validation, because a provenance cycle is the same lie.
/// Diamonds (two paths to one ancestor) are ordinary and yield one visit.
pub fn ancestry<'a>(artifacts: &'a [Artifact], id: &str) -> Result<Vec<&'a Artifact>, ArtifactError> {
    let mut colors: std::collections::BTreeMap<String, Color> = Default::default();
    let mut order: Vec<&Artifact> = Vec::new();
    let start = artifacts
        .iter()
        .find(|a| a.artifact_id == id)
        .ok_or_else(|| ArtifactError::UnknownArtifact(id.to_string()))?;
    for parent in &start.derived_from {
        visit(artifacts, parent, id, &mut colors, &mut order)?;
    }
    Ok(order)
}

#[derive(Clone, Copy, PartialEq)]
enum Color {
    InProgress,
    Done,
}

fn visit<'a>(
    artifacts: &'a [Artifact],
    id: &str,
    root: &str,
    colors: &mut std::collections::BTreeMap<String, Color>,
    order: &mut Vec<&'a Artifact>,
) -> Result<(), ArtifactError> {
    match colors.get(id) {
        Some(Color::Done) => return Ok(()), // diamond: already expanded
        Some(Color::InProgress) => {
            return Err(ArtifactError::Cycle(id.to_string()));
        }
        None => {}
    }
    if id == root {
        return Err(ArtifactError::Cycle(id.to_string()));
    }
    colors.insert(id.to_string(), Color::InProgress);
    let artifact = artifacts
        .iter()
        .find(|a| a.artifact_id == id)
        .ok_or_else(|| ArtifactError::UnknownArtifact(id.to_string()))?;
    for parent in &artifact.derived_from {
        visit(artifacts, parent, root, colors, order)?;
    }
    colors.insert(id.to_string(), Color::Done);
    order.push(artifact);
    Ok(())
}

fn validate_artifact_id(id: &str) -> Result<(), ArtifactError> {
    let rest = id
        .strip_prefix("urn:hacp:artifact:")
        .ok_or_else(|| ArtifactError::BadId { found: id.to_string() })?;
    // uuid4 shape: 8-4-4-4-12 hex, version nibble 4, variant nibble 8/9/a/b.
    let bytes = rest.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
    {
        return Err(ArtifactError::BadId { found: id.to_string() });
    }
    let hex: String = rest.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return Err(ArtifactError::BadId { found: id.to_string() });
    }
    let nibbles: Vec<char> = hex.chars().collect();
    if nibbles[12] != '4' || !matches!(nibbles[16], '8' | '9' | 'a' | 'b') {
        return Err(ArtifactError::BadId { found: id.to_string() });
    }
    Ok(())
}

pub(crate) fn is_digest_shape(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID4: &str = "9f0d0e6a-1111-4222-8333-444444444444";

    fn id() -> String {
        format!("urn:hacp:artifact:{UUID4}")
    }

    fn artifact(name_digits: &str) -> Artifact {
        Artifact::new(
            &format!("urn:hacp:artifact:{name_digits}-1111-4222-8333-444444444444"),
            "text/plain",
            &"a".repeat(64),
            12,
            "urn:hacp:agent:child-1",
            "t-1",
            "c-1",
            &"b".repeat(64),
            "runs/r1/agents/b/thing.txt",
        )
        .unwrap()
    }

    #[test]
    fn ids_must_be_uuid4_urns() {
        assert!(validate_artifact_id(&id()).is_ok());
        for bad in [
            "urn:hacp:artifact:not-a-uuid",
            &format!("urn:hacp:artifact:{}", UUID4.replace('4', "3")),
            "urn:hacp:artifact:9F0D0E6A-1111-4222-8333-444444444444",
        ] {
            assert!(validate_artifact_id(bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn fields_have_shapes() {
        let mut a = artifact("00000000");
        assert!(a.validate().is_ok());
        a.digest = "TBD".into();
        assert!(matches!(a.validate(), Err(ArtifactError::BadDigest { .. })));
        let mut a = artifact("00000000");
        a.media_type = "text".into();
        assert!(matches!(a.validate(), Err(ArtifactError::BadMediaType { .. })));
        let mut a = artifact("00000000");
        a.producer = "codex".into();
        assert!(matches!(a.validate(), Err(ArtifactError::BadProducer(_))));
    }

    #[test]
    fn visibility_is_a_widening_ladder() {
        let mut a = artifact("00000000");
        a.visibility = Visibility::Participants;
        assert!(a.visible_to(Audience::Participant));
        assert!(!a.visible_to(Audience::Supervisor));
        a.visibility = Visibility::Supervisors;
        assert!(a.visible_to(Audience::Participant));
        assert!(a.visible_to(Audience::Supervisor));
        assert!(!a.visible_to(Audience::SessionObserver));
        a.visibility = Visibility::Deployment;
        assert!(a.visible_to(Audience::Deployment));
    }

    #[test]
    fn provenance_walks_transitively_and_refuses_cycles() {
        let base = artifact("00000000");
        let mut mid = artifact("00000001");
        base.derive(&mut mid);
        let mut top = artifact("00000002");
        mid.derive(&mut top);
        let all = [base.clone(), mid, top];
        let chain = ancestry(&all, &format!("urn:hacp:artifact:00000002-1111-4222-8333-444444444444")).unwrap();
        assert_eq!(chain.len(), 2, "base and mid, not just the direct parent");
        // A cycle: base and top claim to derive from each other.
        let mut cyclic = base;
        cyclic.derived_from = vec![format!("urn:hacp:artifact:00000002-1111-4222-8333-444444444444")];
        let mut top = artifact("00000002");
        top.derived_from = vec![format!("urn:hacp:artifact:00000000-1111-4222-8333-444444444444")];
        let all = [cyclic, artifact("00000001"), top];
        assert!(matches!(
            ancestry(&all, &format!("urn:hacp:artifact:00000000-1111-4222-8333-444444444444")),
            Err(ArtifactError::Cycle(_))
        ));
    }
}
