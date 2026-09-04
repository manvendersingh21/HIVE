//! Reports, verdicts, and run summaries (`spec/HACP.md` §10, §11).
//!
//! Constraint C2 governs this whole module: **a worker's claim about its own output is
//! evidence of nothing.** Every field a worker writes here exists to be re-checked, and
//! [`VerificationResult`] is the record of that re-checking.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::RunState;

/// The structured end of a role's work. "Done" is not a report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompletionReport {
    /// Unique report id (`r-<uuid>`).
    pub report_id: String,
    /// The reporting agent's URN.
    pub agent: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<ReportArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffstat: Option<DiffStat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestRun>,
    pub contract_status: ContractStatus,
    #[serde(default)]
    pub deviations: Vec<String>,
    #[serde(default)]
    pub follow_ups: Vec<String>,
    /// Where a human can see what the agent actually did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ReportEvidence>,
    #[serde(default)]
    pub duration_secs: u64,
    /// Who wrote this. An adapter synthesizing on the worker's behalf MUST say so.
    pub source: ReportSource,
}

impl CompletionReport {
    /// The skeleton an adapter fills from what it can observe without understanding the
    /// work: exit code, diffstat, artifact existence (§10).
    ///
    /// It starts `Blocked` / `NotReported` deliberately. An adapter that cannot tell
    /// whether the work succeeded must not guess that it did.
    pub fn fallback(agent: &str) -> Self {
        Self {
            report_id: format!("r-{}", Uuid::new_v4()),
            agent: agent.to_string(),
            outcome: Outcome::Blocked,
            summary: String::new(),
            artifacts: Vec::new(),
            diffstat: None,
            tests: None,
            contract_status: ContractStatus::NotReported,
            deviations: Vec::new(),
            follow_ups: Vec::new(),
            evidence: None,
            duration_secs: 0,
            source: ReportSource::AdapterSynthesized,
        }
    }

    /// Whether this report was written by the worker itself.
    pub fn is_self_reported(&self) -> bool {
        self.source == ReportSource::Agent
    }
}

/// Overall outcome of a role's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Success,
    Partial,
    Failure,
    Blocked,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Outcome::Success => "success",
            Outcome::Partial => "partial",
            Outcome::Failure => "failure",
            Outcome::Blocked => "blocked",
        })
    }
}

/// The worker's own claim about the contract — believed by no one (C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ContractStatus {
    Satisfied,
    Deviated,
    Partial,
    NotStarted,
    NotReported,
}

impl std::fmt::Display for ContractStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ContractStatus::Satisfied => "satisfied",
            ContractStatus::Deviated => "deviated",
            ContractStatus::Partial => "partial",
            ContractStatus::NotStarted => "not-started",
            ContractStatus::NotReported => "not-reported",
        })
    }
}

/// Who produced a [`CompletionReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReportSource {
    Agent,
    AdapterSynthesized,
}

/// An artifact claim inside a report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReportArtifact {
    pub artifact_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default)]
    pub exists: bool,
}

/// Diff summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DiffStat {
    #[serde(default)]
    pub files_changed: usize,
    #[serde(default)]
    pub insertions: usize,
    #[serde(default)]
    pub deletions: usize,
}

/// A test-run claim. Evidence, not a substitute for the arbiter's own checks.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestRun {
    pub command: String,
    #[serde(default)]
    pub passed: usize,
    #[serde(default)]
    pub failed: usize,
    /// Tail of the output.
    #[serde(default)]
    pub output: String,
}

/// Where the work's evidence lives.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReportEvidence {
    /// Relative to the run workspace; MUST point at a retained log (§10).
    pub log_path: String,
    /// An implementation-defined handle for attaching to the live session, if there is
    /// one. Deliberately an opaque string: the protocol does not know what a session is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

// ---------------------------------------------------------------------------
// Verification (§11)
// ---------------------------------------------------------------------------

/// The arbiter's verdict on one report: every check, with its evidence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VerificationResult {
    pub agent: String,
    /// The report this verdict answers.
    pub report_id: String,
    pub checks: Vec<CheckResult>,
    /// True iff every check passed.
    pub passed: bool,
}

impl VerificationResult {
    /// Build a verdict, deriving `passed` from the checks rather than accepting it as an
    /// independent claim that could disagree with them.
    pub fn new(agent: impl Into<String>, report_id: impl Into<String>, checks: Vec<CheckResult>) -> Self {
        let passed = checks.iter().all(|c| c.passed);
        Self { agent: agent.into(), report_id: report_id.into(), checks, passed }
    }

    /// The checks that failed — the body of a `rework.requested` (§11.1).
    pub fn failed_checks(&self) -> Vec<CheckResult> {
        self.checks.iter().filter(|c| !c.passed).cloned().collect()
    }
}

/// The seven check families of §11, as stable name prefixes. A verdict names its checks
/// `<family>:<artifact_id>` so a consumer can tell which check failed without parsing
/// prose.
pub mod check {
    pub const EXISTENCE: &str = "existence";
    pub const INTEGRITY: &str = "integrity";
    pub const INTERFACE_FROZEN: &str = "interface-frozen";
    pub const BUILD_PROBE: &str = "build-probe";
    pub const SYMBOLS: &str = "symbols";
    pub const SCHEMA: &str = "schema";
    pub const INTEGRATION: &str = "integration";

    /// The canonical check name for an artifact.
    pub fn name(family: &str, artifact_id: &str) -> String {
        format!("{family}:{artifact_id}")
    }
}

/// One named check with its evidence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckResult {
    /// e.g. `existence:job-store`, `interface-frozen:job-store`.
    pub name: String,
    pub passed: bool,
    /// What a human needs to judge the check: paths, digests, output tails. A failed
    /// check MUST quote its evidence (§11).
    #[serde(default)]
    pub evidence: String,
}

impl CheckResult {
    pub fn pass(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self { name: name.into(), passed: true, evidence: evidence.into() }
    }

    pub fn fail(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self { name: name.into(), passed: false, evidence: evidence.into() }
    }
}

// ---------------------------------------------------------------------------
// Run summary
// ---------------------------------------------------------------------------

/// What `run.completed` carries.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunSummary {
    pub run_id: String,
    pub goal: String,
    pub final_state: RunState,
    #[serde(default)]
    pub agents: Vec<AgentSummary>,
    /// The integration check, when the run reached that stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<CheckResult>,
    /// Implementation-defined pointers a human can follow: logs, sessions, artifacts.
    ///
    /// Deliberately opaque. 1.0 typed this as the reference implementation's own session
    /// struct, which made the protocol depend on that implementation's internals — the
    /// exact coupling this crate exists to prevent.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub duration_secs: u64,
    /// Set on failure, abort, or timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An implementation-defined pointer to evidence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceRef {
    /// What kind of thing this points at, e.g. `"log"`, `"session"`, `"artifact"`.
    pub kind: String,
    /// How to reach it, in whatever form the implementation uses.
    pub locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One agent's line in a [`RunSummary`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSummary {
    pub agent: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_passed: Option<bool>,
    #[serde(default)]
    pub checks_passed: usize,
    #[serde(default)]
    pub checks_total: usize,
    pub report_source: ReportSource,
    /// Rework rounds this agent needed (§11.1).
    #[serde(default)]
    pub rework_rounds: u32,
}
