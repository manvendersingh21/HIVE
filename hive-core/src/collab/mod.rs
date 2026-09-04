//! Hive's binding to HACP — the Heterogeneous Agent Collaboration Protocol.
//!
//! The protocol itself lives in the standalone [`hacp`] crate and its specification in
//! `hacp/spec/HACP.md`. Nothing in that crate knows Hive exists, and nothing in this
//! module may leak back into it: Hive is *a* reference implementation of HACP, not the
//! definition of it.
//!
//! This file declares the **seams** between the parts of that binding. It exists so the
//! parts can be built independently and checked against each other, which is the same
//! reason HACP itself freezes an `InterfaceContract` before work starts (`spec/HACP.md`
//! §9). Implementations code against these traits, never against one another.
//!
//! ```text
//!   Orchestrator ── the FSM: formation, negotiation, freeze, verify, rework, integrate
//!        │
//!        ├── Formation    goal ─► how many agents, which roles, why
//!        ├── MessageBus   durable, ordered, deduplicated message log
//!        ├── RunStore     the on-disk run workspace (§14) and the file edge (§13.2)
//!        ├── SessionHost  launching and supervising a stock CLI
//!        └── Verifier     the seven checks (§11) and the acceptance test
//! ```
//!
//! Each trait is deliberately narrow and free of the others: an implementation of one
//! never names another. That is what lets them be written in parallel, and what makes a
//! substitution (an in-memory bus for tests, a remote session host) a local change.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hacp::contract::InterfaceContract;
use hacp::report::{CheckResult, CompletionReport, VerificationResult};
use hacp::state::{RunLimits, RunState};
use hacp::topology::TaskDecomposition;
use hacp::Envelope;

/// Result type used across the collab binding.
pub type Result<T> = anyhow::Result<T>;

// ---------------------------------------------------------------------------
// MessageBus
// ---------------------------------------------------------------------------

/// What happened to an ingested envelope (`spec/HACP.md` §13.1).
///
/// The three outcomes are distinct on purpose: at-least-once edges make duplicates
/// normal traffic, and collapsing `Duplicate` into `Accepted` would let a redelivery
/// silently re-trigger whatever the message causes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingested {
    /// Newly accepted, assigned this per-run sequence number.
    Accepted { seq: u64 },
    /// A `message_id` already seen. An idempotent no-op; the original `seq` is returned.
    Duplicate { seq: u64 },
    /// Refused. `code` is the `error.protocol` code the sender should be given.
    Rejected { code: String, detail: String },
}

/// One page of messages addressed to a caller.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// The run's state at the time of the read.
    pub state: RunState,
    /// The caller's next cursor.
    pub seq: u64,
    /// Envelopes with `seq` strictly greater than the requested cursor, in order, that
    /// [`Envelope::deliverable_to`] admits for this caller, each carrying its own
    /// sequence number.
    pub messages: Vec<Sequenced>,
}

/// An envelope together with the bus sequence number assigned to it.
///
/// The seq is carried per message, not derived from the page cursor, because a caller's
/// deliverable seqs are **not contiguous**: peer traffic between two other agents occupies
/// sequence numbers that this caller never sees. Spec §13.2 names each INBOX file by the
/// message's sequence number, so a transport that only had the page cursor would have to
/// invent local ordinals — preserving order but silently renaming every message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Sequenced {
    pub seq: u64,
    pub envelope: Envelope,
}

/// The durable, ordered, deduplicated message log — the coordinator's mechanical half.
///
/// **Implementers:** a bus is content-blind (§2). It MUST NOT inspect, alter, summarize,
/// or withhold a body, and it MUST persist and deliver an unregistered `kind` rather than
/// rejecting it (§5) — that rule is the protocol's only forward-compatibility mechanism.
/// Enforce the envelope's *shape*, the sender's role binding, and nothing else.
#[async_trait]
pub trait MessageBus: Send + Sync {
    /// Register a new run. Idempotent on `run_id`.
    async fn create_run(&self, run_id: &str, goal: &str, limits: RunLimits) -> Result<()>;

    /// Accept one envelope. MUST deduplicate on `message_id` and assign a monotonic
    /// per-run `seq`. MUST reject an envelope whose `from` does not match `sender_role`
    /// (§13.3) — a worker cannot speak as its peer.
    async fn ingest(&self, sender_role: &str, envelope: Envelope) -> Result<Ingested>;

    /// Messages for `who` after `since`. Peer traffic is filtered per
    /// [`Envelope::deliverable_to`]: private to its endpoints and the arbiter (§6).
    async fn poll(&self, run_id: &str, who: &str, since: u64) -> Result<Delivery>;

    /// The full ordered log, for audit and for `hive collab show`.
    async fn history(&self, run_id: &str) -> Result<Vec<Envelope>>;

    async fn state(&self, run_id: &str) -> Result<RunState>;

    /// Advance the run. MUST reject a transition [`RunState::can_transition_to`] forbids,
    /// so an illegal path fails where it happens rather than three states later.
    async fn set_state(&self, run_id: &str, next: RunState) -> Result<()>;
}

// ---------------------------------------------------------------------------
// RunStore
// ---------------------------------------------------------------------------

/// Where one worker's files live (`spec/HACP.md` §14).
#[derive(Debug, Clone)]
pub struct AgentPaths {
    pub role: String,
    /// `agents/<role>/` — the only directory this worker may write to.
    pub root: PathBuf,
    pub brief: PathBuf,
    pub inbox: PathBuf,
    pub outbox: PathBuf,
    /// The worker's isolated checkout of the shared repository.
    pub workspace: PathBuf,
    /// Tee'd worker output; the supervision target.
    pub log: PathBuf,
}

/// One `{kind, body}` file a worker wrote to its OUTBOX (§13.2).
#[derive(Debug, Clone)]
pub struct Outbound {
    /// The file it came from, so a malformed one can be reported by name.
    pub source: PathBuf,
    pub kind: String,
    pub body: serde_json::Value,
}

/// The on-disk run workspace, and the file edge a stock CLI actually sees.
///
/// **Implementers:** this is constraint C1 made concrete. A worker only ever reads
/// `BRIEF.md` and `INBOX/`, and writes `OUTBOX/` and `REPORT.json`. Everything else is
/// yours. Two rules are load-bearing:
///
/// * A worker writes ONLY inside `agents/<role>/` (§14). Enforce it where you can.
/// * `drain_outbox` MUST NOT interpret a body — shape only (§2). A malformed file is
///   reported, not repaired, and never silently dropped.
#[async_trait]
pub trait RunStore: Send + Sync {
    /// `<run_root>/` for this run.
    fn root(&self) -> &Path;

    /// Create the run workspace and its `run.json`. MUST NOT write tokens to disk.
    async fn init_run(&self, run_id: &str, goal: &str) -> Result<()>;

    /// Create one worker's workspace and mailbox, and write its brief.
    async fn init_agent(&self, role: &str, brief: &str) -> Result<AgentPaths>;

    async fn agent_paths(&self, role: &str) -> Result<AgentPaths>;

    /// Write an inbound message as `INBOX/<seq:06>-<kind>.json`, so filename order is
    /// message order for a worker that can only `ls`.
    async fn write_inbox(&self, role: &str, seq: u64, envelope: &Envelope) -> Result<()>;

    /// Take everything the worker has written to OUTBOX, removing what is returned so a
    /// message is emitted once.
    async fn drain_outbox(&self, role: &str) -> Result<Vec<Outbound>>;

    /// The worker's own `REPORT.json`, if it wrote one. `None` means the adapter must
    /// synthesize one and mark it `adapter-synthesized` (§10).
    async fn read_report(&self, role: &str) -> Result<Option<CompletionReport>>;

    /// Persist a decomposition, draft, or frozen contract to the run's audit trail (§14).
    async fn record(&self, name: &str, value: &serde_json::Value) -> Result<()>;

    /// Append one line to `AMENDMENTS.jsonl` — every amendment and change request, with
    /// its decision.
    async fn append_amendment(&self, entry: &serde_json::Value) -> Result<()>;
}

// ---------------------------------------------------------------------------
// SessionHost
// ---------------------------------------------------------------------------

/// How to start one stock agentic CLI on its brief.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Session name, unique per run and role.
    pub name: String,
    /// The program, e.g. `claude`, `codex`, `agy`. Recorded here and **never** placed on
    /// the bus, in a brief, or in a contract: §3 forbids a peer learning it.
    pub program: String,
    /// Arguments that put the program in non-interactive mode on `prompt`.
    pub args: Vec<String>,
    /// The brief, handed to the program as its prompt.
    pub prompt: String,
    /// Working directory — the worker's own workspace, never the run root.
    pub cwd: PathBuf,
    /// Combined stdout/stderr is tee'd here for supervision and evidence.
    pub log: PathBuf,
    /// Hard limit on wall-clock time.
    pub timeout_secs: u64,
}

/// A running worker session.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub name: String,
    pub log: PathBuf,
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    Exited { code: i32 },
    TimedOut,
    /// Suspended by supervision; the process tree is intact and attachable.
    Paused { reason: String },
}

/// Launches and supervises a stock CLI on behalf of one role.
///
/// **Implementers:** two rules come straight from operational experience already recorded
/// in `docs/STATUS.md`, and both matter more than they look.
///
/// * **Pause, never kill.** Suspend the pane's foreground process group (SIGSTOP). An
///   interrupt ends the session and orphans its children, destroying the very state an
///   operator is being asked to inspect.
/// * **Scan every line.** Tier-1 rules run on each tailed line as it arrives
///   (`crate::watchdog::Watchdog::scan_line`); polling for output can miss short bursts.
#[async_trait]
pub trait SessionHost: Send + Sync {
    async fn launch(&self, spec: &SessionSpec) -> Result<SessionHandle>;

    /// Block until the session ends, tailing its log and applying supervision.
    async fn wait(&self, handle: &SessionHandle) -> Result<SessionOutcome>;

    /// Suspend the process group, preserving state for review.
    async fn pause(&self, handle: &SessionHandle, reason: &str) -> Result<()>;

    async fn resume(&self, handle: &SessionHandle) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Verifier
// ---------------------------------------------------------------------------

/// Everything the seven checks need (`spec/HACP.md` §11).
#[derive(Debug, Clone)]
pub struct VerifyContext<'a> {
    pub contract: &'a InterfaceContract,
    /// The report under scrutiny. Its contents are claims, never evidence (C2).
    pub report: &'a CompletionReport,
    /// The worker's workspace, where its artifacts actually are.
    pub workspace: &'a Path,
    /// Digests in force: from `contract.frozen`, or from the most recent
    /// `contract.amended` (§9.2). Check 3 compares against these, which is how an
    /// *agreed* interface change is distinguished from an undeclared one.
    pub frozen_digests: &'a std::collections::BTreeMap<String, String>,
}

/// Re-checks every claim a worker makes, and runs the contract itself.
///
/// **Implementers:** the ordering in §11 is normative and each check MUST record its
/// evidence — a failed check that says only "failed" is unactionable, and §11.1 feeds
/// these results verbatim into a rework request. Check 5 (symbols) is a literal grep and
/// MUST be described as such: it is not proof of semantics, and a passing verdict must
/// not imply more than it checked.
#[async_trait]
pub trait Verifier: Send + Sync {
    /// Checks 1–6, for one report.
    async fn verify(&self, ctx: &VerifyContext<'_>) -> Result<VerificationResult>;

    /// Check 7: merge all worker output and run `integration.command`, including the
    /// acceptance test generated from the contract's `examples`. This is the frozen
    /// contract executed rather than any agent's self-assessment.
    async fn integrate(&self, contract: &InterfaceContract, integration_root: &Path) -> Result<CheckResult>;

    /// Generate the acceptance test from `examples`. Separate from [`Self::integrate`] so
    /// it can be inspected, and shown to workers, before anything runs.
    fn acceptance_test(&self, contract: &InterfaceContract) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Formation
// ---------------------------------------------------------------------------

/// An agent that could take a role.
#[derive(Debug, Clone)]
pub struct AgentCandidate {
    /// Master-side label for the CLI. Never leaves the run record (§3).
    pub cli_label: String,
    /// The machine it would run on.
    pub machine: String,
    /// Capabilities the machine graph reports for it.
    pub capabilities: Vec<String>,
}

/// Turns a goal into a team: how many agents, which roles, and why.
///
/// **Implementers:** §7 requires `agent_count` to be *derived* from the goal's structure.
/// An implementation that returns the same count for every goal is not conformant, and
/// `rationale` must say why this many — not restate how many. Capability claims may
/// inform assignment and MUST NOT gate admission (§8).
#[async_trait]
pub trait Formation: Send + Sync {
    async fn decompose(&self, goal: &str, available: &[AgentCandidate]) -> Result<TaskDecomposition>;

    /// Bind roles to candidates. Returns `(role_id, cli_label)` pairs.
    ///
    /// Returns an error rather than substituting when no candidate has a required
    /// capability: silently running work on a machine that cannot do it fails in a far
    /// more confusing way than being told no agent matched (`docs/PLACEMENT.md` §4).
    async fn assign(
        &self,
        decomposition: &TaskDecomposition,
        available: &[AgentCandidate],
    ) -> Result<Vec<(String, String)>>;
}

// The bindings. Each implements one trait above and knows nothing of the others:
// that independence is what let all five be written in parallel, and it is the
// property to preserve if any of them grows.
//
// These declarations were missing when the traits were frozen, which made every
// parcel uncompilable and forced all five agents into the same corner. Two
// escalated rather than edit a frozen file, one edited and marked it TEMPORARY,
// and the remaining two built against a copy. That is the same defect HACP §8
// had before §9.2 gave freeze a legitimate door — reproduced on ourselves within
// the hour, and worth more as evidence than the delay it cost.
pub mod bus;
pub mod formation;
pub mod session;
pub mod verify;
pub mod workspace;
