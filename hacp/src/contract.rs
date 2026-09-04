//! The InterfaceContract — the decided abstraction between workers (`spec/HACP.md` §9).
//!
//! A contract is drafted, negotiated, frozen with per-artifact digests, and — new in
//! 1.1 — amendable after freeze through the controlled path in §9.2. The digest freeze
//! is the mechanism that keeps a decided abstraction decided; the amendment path is what
//! keeps that from being brittle on goals where nobody knows the right interface up
//! front.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The decided abstraction between workers: what each side produces and consumes, and
/// how the result is checked.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InterfaceContract {
    /// Unique contract id (`c-<uuid>`).
    pub contract_id: String,
    /// Draft version; the arbiter bumps it on every accepted amendment.
    #[serde(default = "default_version")]
    pub version: u32,
    /// The run's goal, verbatim.
    pub goal: String,
    /// Everything any role produces. Ids are unique within the contract.
    #[serde(default)]
    pub artifacts: Vec<ArtifactSpec>,
    /// Who consumes what — the edges between the workers.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    /// The command run on the merged result of all worker output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<IntegrationSpec>,
    /// Ground rules, restated in every brief.
    #[serde(default)]
    pub workspace_rules: Vec<String>,
}

fn default_version() -> u32 {
    1
}

/// Why a contract failed validation (§9).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("field {0} must not be empty")]
    Empty(&'static str),
    #[error("duplicate artifact_id {0:?}")]
    DuplicateArtifact(String),
    #[error("artifact {artifact:?}: {detail}")]
    Artifact { artifact: String, detail: String },
    #[error("dependency references unknown artifact_id {0:?}")]
    UnknownDependency(String),
    #[error("dependency cycle through artifact {0:?}")]
    DependencyCycle(String),
}

impl InterfaceContract {
    /// Look up an artifact by id.
    pub fn artifact(&self, id: &str) -> Option<&ArtifactSpec> {
        self.artifacts.iter().find(|a| a.artifact_id == id)
    }

    /// Artifacts produced by `agent_urn`.
    pub fn produced_by(&self, agent_urn: &str) -> Vec<&ArtifactSpec> {
        self.artifacts.iter().filter(|a| a.produced_by == agent_urn).collect()
    }

    /// Artifacts `agent_urn` consumes, via `dependencies`.
    pub fn consumed_by(&self, agent_urn: &str) -> Vec<&ArtifactSpec> {
        self.dependencies
            .iter()
            .filter(|d| d.consumer == agent_urn)
            .filter_map(|d| self.artifact(&d.consumes))
            .collect()
    }

    /// The agent URNs that consume `artifact_id`.
    ///
    /// This is the audience for `interface.impacted` (§9.2): when a frozen interface
    /// changes, exactly these agents are told, and no others.
    pub fn consumers_of(&self, artifact_id: &str) -> Vec<&str> {
        self.dependencies
            .iter()
            .filter(|d| d.consumes == artifact_id)
            .map(|d| d.consumer.as_str())
            .collect()
    }

    /// Validate the document (§9). Run at draft and after every accepted amendment.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.contract_id.is_empty() {
            return Err(ContractError::Empty("contract_id"));
        }
        if self.goal.trim().is_empty() {
            return Err(ContractError::Empty("goal"));
        }

        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
        for a in &self.artifacts {
            if seen.insert(a.artifact_id.as_str(), ()).is_some() {
                return Err(ContractError::DuplicateArtifact(a.artifact_id.clone()));
            }
            a.validate()?;
        }

        for d in &self.dependencies {
            if self.artifact(&d.consumes).is_none() {
                return Err(ContractError::UnknownDependency(d.consumes.clone()));
            }
        }

        if let Some(i) = &self.integration {
            if i.command.trim().is_empty() {
                return Err(ContractError::Empty("integration.command"));
            }
        }

        self.check_acyclic()
    }

    /// Reject a dependency cycle: A's producer consuming B while B's producer consumes A
    /// cannot be built in any order, and a contract that cannot be built is not a
    /// contract. 1.0 permitted this by omission.
    fn check_acyclic(&self) -> Result<(), ContractError> {
        // Edge: artifact -> artifact, where producing the first requires the second.
        let edges: Vec<(&str, &str)> = self
            .dependencies
            .iter()
            .flat_map(|d| {
                self.produced_by(&d.consumer)
                    .into_iter()
                    .map(move |produced| (produced.artifact_id.as_str(), d.consumes.as_str()))
            })
            .collect();

        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Open,
            Done,
        }
        fn walk<'a>(
            node: &'a str,
            edges: &[(&'a str, &'a str)],
            marks: &mut BTreeMap<&'a str, Mark>,
        ) -> Result<(), ContractError> {
            match marks.get(node) {
                Some(Mark::Done) => return Ok(()),
                Some(Mark::Open) => return Err(ContractError::DependencyCycle(node.to_string())),
                None => {}
            }
            marks.insert(node, Mark::Open);
            for (from, to) in edges.iter().filter(|(f, _)| *f == node) {
                let _ = from;
                walk(to, edges, marks)?;
            }
            marks.insert(node, Mark::Done);
            Ok(())
        }

        let mut marks = BTreeMap::new();
        for a in &self.artifacts {
            walk(a.artifact_id.as_str(), &edges, &mut marks)?;
        }
        Ok(())
    }
}

/// One producible artifact.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactSpec {
    /// Unique within the contract.
    pub artifact_id: String,
    /// The agent URN of the single producer.
    pub produced_by: String,
    /// Path relative to the run's shared repository root.
    pub path: String,
    /// How the artifact is realized.
    pub format: ArtifactFormat,
    /// JSON-Schema document; REQUIRED when `format` is `json`, otherwise null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Paths relative to the artifact whose contents are frozen at `contract.frozen`.
    #[serde(default)]
    pub interface_files: Vec<String>,
    /// Grep-level interface claims. Shallow by design (§11).
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Input/output pairs compiled into the acceptance test.
    #[serde(default)]
    pub examples: Vec<ExamplePair>,
    /// Build probe run from the repository root; MUST exit 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<ContractCheck>,
}

impl ArtifactSpec {
    fn err(&self, detail: impl Into<String>) -> ContractError {
        ContractError::Artifact {
            artifact: self.artifact_id.clone(),
            detail: detail.into(),
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.artifact_id.is_empty() {
            return Err(ContractError::Empty("artifact_id"));
        }
        if self.produced_by.is_empty() {
            return Err(self.err("produced_by must name exactly one agent"));
        }
        if self.path.trim().is_empty() {
            return Err(self.err("path must not be empty"));
        }
        match (self.format, self.schema.is_some()) {
            (ArtifactFormat::Json, false) => {
                return Err(self.err("format \"json\" requires a schema"))
            }
            (ArtifactFormat::Json, true) => {}
            (_, true) => return Err(self.err("schema is only valid with format \"json\"")),
            (_, false) => {}
        }
        if let Some(c) = &self.check {
            if c.command.trim().is_empty() {
                return Err(self.err("check.command must not be empty"));
            }
        }
        // §9: v1 acceptance tests require string inputs and outputs. 1.0 typed these as
        // arbitrary JSON while the prose required strings; the type is narrowed here at
        // validation rather than in the type, so a non-conforming example is reported as
        // a contract error instead of failing to parse.
        for e in &self.examples {
            if !e.input.is_string() || !e.output.is_string() {
                return Err(self.err("examples must have string input and output in v1"));
            }
        }
        Ok(())
    }

    /// The canonical interface digest (§9): sha256 over each `interface_files` entry's
    /// bytes, **in listed order**, newline-joined, hex, prefixed `sha256:`.
    ///
    /// `read` resolves a path relative to the artifact to its bytes. An artifact with no
    /// `interface_files` digests the empty input, which is well-defined and constant —
    /// it declares no frozen interface, so nothing about it can drift.
    pub fn interface_digest<F, E>(&self, mut read: F) -> Result<String, E>
    where
        F: FnMut(&str) -> Result<Vec<u8>, E>,
    {
        let mut hasher = Sha256::new();
        for (i, path) in self.interface_files.iter().enumerate() {
            if i > 0 {
                hasher.update(b"\n");
            }
            hasher.update(read(path)?);
        }
        Ok(format!("sha256:{:x}", hasher.finalize()))
    }
}

/// How an artifact is realized. Closed in v1: a new format is a major-version concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactFormat {
    RustCrate,
    Json,
    File,
}

/// An input/output pair compiled into the run's acceptance test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExamplePair {
    pub input: serde_json::Value,
    pub output: serde_json::Value,
}

/// A build probe attached to an artifact.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractCheck {
    /// `"command"` in v1.
    pub kind: String,
    /// Run from the repository root; MUST exit 0.
    pub command: String,
}

/// A consumption edge: `consumer` (agent URN) depends on `consumes` (artifact id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Dependency {
    pub consumer: String,
    pub consumes: String,
}

/// The integration command run on the merged result of all worker output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IntegrationSpec {
    pub command: String,
}

// ---------------------------------------------------------------------------
// Negotiation bodies (§9.1)
// ---------------------------------------------------------------------------

/// Body of `contract.drafted`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractDraftedBody {
    pub contract: InterfaceContract,
    pub round: u32,
    pub rounds_remaining: u32,
    /// Silence past this counts as consent (§9.1).
    pub respond_by: DateTime<Utc>,
}

/// Body of `contract.frozen`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractFrozenBody {
    pub contract: InterfaceContract,
    /// Per-artifact digest over its `interface_files`.
    pub interface_digests: BTreeMap<String, String>,
}

/// A worker-proposed change during negotiation (`contract.amendment`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractAmendment {
    pub target_version: u32,
    /// Why. Required: adjudication reads it, and so do humans.
    pub rationale: String,
    /// Strictly new artifacts.
    #[serde(default)]
    pub additions: Vec<ArtifactSpec>,
    /// Changes to existing artifacts.
    #[serde(default)]
    pub changes: Vec<AmendmentChange>,
}

impl ContractAmendment {
    /// Whether this amendment only adds (§9.1's auto-acceptance rule).
    ///
    /// The rule is stated here rather than left to each implementation because
    /// "strictly additive" is exactly the kind of judgement that drifts.
    pub fn is_strictly_additive(&self) -> bool {
        self.changes.is_empty() && !self.additions.is_empty()
    }
}

/// One change inside a [`ContractAmendment`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AmendmentChange {
    pub artifact_id: String,
    pub change: String,
    pub reason: String,
}

/// Body of `contract.amendment.accepted` / `.rejected`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AmendmentDecisionBody {
    /// Set on acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_version: Option<u32>,
    #[serde(default)]
    pub note: String,
    /// Set on rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Body of `artifact.published`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactPublishedBody {
    pub artifact_id: String,
    pub path: String,
    pub sha256: String,
}

/// Body of `question` and `peer.question`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionBody {
    /// An artifact id, or the literal `"goal"`.
    pub about: String,
    pub text: String,
}

/// Body of `answer`. `peer.answer` carries only `text`, since a peer answering about its
/// own interface has no other provenance to declare.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnswerBody {
    pub text: String,
    /// `"contract"` — answered from the contract — or `"unknown"`, which an arbiter MUST
    /// use rather than inventing an interface fact (§6).
    pub scope: String,
}
