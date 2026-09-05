//! The bilateral run: two real agents, one contract, start to finish.
//!
//! The order below is not invented here. It is the sequence proved live in
//! `interop/live/hacp-live.py` against four different vendor CLIs, ported so that HIVE
//! itself runs it — every protocol decision taken by [`hacp::v2`]'s state machines,
//! every agent invocation supervised by [`crate::collab::SessionHost`], and every claim
//! measured by [`super::attest`] before it can settle anything.
//!
//! ```text
//!   handshake     session.open / session.features         §6
//!   authoring     the supervising agent writes the terms
//!   proposal      contract.proposed                       §7.3
//!   review        the performing agent accepts or declines
//!   freeze        contract.frozen, digest recomputed      §7.5
//!   execute       a contract STATE, not a message         §7.5
//!   submission    submission.delivered, artifact by ref   §9.1
//!   verification  verification.delivered, gated by §9.4
//!   settlement    contract.decide, session.close, transcripts compared
//! ```
//!
//! **Where the reasoning lives.** The agents decide *what* — terms, acceptance,
//! verdicts. This module decides *nothing* about the work and everything about whether
//! a claim may advance the run. That split is the protocol's whole bet, and the reason
//! findings 8–10 were caught rather than shipped.

use std::path::{Path, PathBuf};

use hacp::v2::contract::{ContractLimits, Relationship, Submission, Task, Verdict};
use hacp::v2::{canon, kinds, Artifact, Check, Contract, Session, Verification};
use serde_json::{json, Value};

use crate::collab::SessionHost;

use super::attest::{self, Corroboration, Facts};
use super::brief::{self, AgentCall, CallResult, Invocation};
use super::cli::AgentCli;
use super::edge::{self, Side};
use super::report::{LiveSession, RunOutcome, RunReport, Stage};

/// One run's inputs.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The CLI that supervises: authors the terms and verifies the result.
    pub supervisor: String,
    /// The CLI that performs: reviews the contract and does the work.
    pub worker: String,
    /// The objective, in the operator's own words.
    pub task: String,
    /// Where the whole run lives. Created if absent.
    pub run_dir: PathBuf,
    /// Wall-clock limit per agent invocation. Exceeding it suspends, never kills.
    pub timeout_secs: u64,
}

/// Features this runtime declares (§6.3). A feature both sides declare is available;
/// what one side did not declare, the other may not rely on.
const SUPERVISOR_FEATURES: &[&str] = &["supervision", "delegation", "artifact-digest", "observer-events"];
const WORKER_FEATURES: &[&str] = &["delegation", "artifact-digest", "observer-events"];

/// Negotiation bounds (§7.4). Small on purpose: silence must not consent, and an
/// unbounded loop between two agents that disagree is the deadlock the bound exists for.
const LIMITS: ContractLimits = ContractLimits {
    max_rounds: 3,
    max_amendments: 2,
};

/// Everything accumulated as the run proceeds, so that a stop at any stage still
/// produces a complete report rather than a bare error.
#[derive(Default)]
struct Accum {
    calls: Vec<AgentCall>,
    artifact: Option<Artifact>,
    corroboration: Option<Corroboration>,
    still_running: Vec<LiveSession>,
}

/// Stop the run here, with a named stage. Never used for "the agent said no" — that is
/// `NoAgreement`, and it is a success.
macro_rules! fail {
    ($stage:expr, $($arg:tt)*) => {
        return Ok(RunOutcome::Failed { stage: $stage, reason: format!($($arg)*) })
    };
}

/// Run one bilateral collaboration to a terminal state.
///
/// Returns `Err` only for failures of *this process* — a directory that cannot be
/// created, a tmux that will not start. Everything an agent or the protocol can do
/// wrong comes back inside the report, because a run that ends badly still has to be
/// readable.
pub async fn run_bilateral(host: &dyn SessionHost, cfg: &RunConfig) -> anyhow::Result<RunReport> {
    let sup_cli = AgentCli::resolve(&cfg.supervisor)?;
    let wrk_cli = AgentCli::resolve(&cfg.worker)?;

    let run_tag = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let paths = Paths::create(&cfg.run_dir).await?;

    // §3: neutral, vendor-free identities. Nothing downstream can recover the tool from
    // these, which is the point — the mapping lives in the run report and nowhere else.
    let a_urn = format!("urn:hacp:agent:sup-{run_tag}");
    let b_urn = format!("urn:hacp:agent:wrk-{run_tag}");
    let session_id = format!("s-{run_tag}{}", &uuid::Uuid::new_v4().simple().to_string()[..4]);
    let contract_id = format!("c-{run_tag}{}", &uuid::Uuid::new_v4().simple().to_string()[..4]);
    let task_id = format!("t-{run_tag}{}", &uuid::Uuid::new_v4().simple().to_string()[..4]);

    let mut a = Side::new("a", &a_urn, paths.a_out.clone(), paths.b_out.clone());
    let mut b = Side::new("b", &b_urn, paths.b_out.clone(), paths.a_out.clone());
    let mut acc = Accum::default();

    let sup = Invocation {
        host,
        cli: sup_cli,
        role: "supervisor",
        cwd: &paths.sup,
        logs: &paths.logs,
        run_tag: &run_tag,
        timeout_secs: cfg.timeout_secs,
    };
    let wrk = Invocation {
        host,
        cli: wrk_cli,
        role: "worker",
        cwd: &paths.wrk,
        logs: &paths.logs,
        run_tag: &run_tag,
        timeout_secs: cfg.timeout_secs,
    };

    let outcome = drive(
        cfg, &paths, &mut a, &mut b, &mut acc, &sup, &wrk, &session_id, &contract_id, &task_id,
        &a_urn, &b_urn,
    )
    .await?;

    // Clean shutdown: whatever this runtime started and did not see end is named here
    // with the command to take it over. Nothing is killed — a suspended agent is the
    // state a person was just asked to inspect.
    if let RunOutcome::Paused { session, .. } = &outcome {
        acc.still_running.push(LiveSession {
            name: session.clone(),
            attach: format!("tmux attach -t {session}"),
        });
    }

    let transcript = match edge::transcript_lines(&a.frames) {
        Ok(lines) if !lines.is_empty() => {
            let p = paths.root.join(format!("{}x{}.jsonl", cfg.supervisor, cfg.worker));
            tokio::fs::write(&p, lines).await?;
            Some(p)
        }
        _ => None,
    };

    let report = RunReport {
        pair: format!("{} x {}", cfg.supervisor, cfg.worker),
        supervisor_cli: cfg.supervisor.clone(),
        worker_cli: cfg.worker.clone(),
        run_dir: paths.root.clone(),
        session_id,
        contract_id,
        task_id,
        task: cfg.task.clone(),
        outcome,
        frames: a.frames.len(),
        calls: acc.calls,
        artifact: acc.artifact,
        corroboration: acc.corroboration,
        transcript,
        still_running: acc.still_running,
    };
    report.write(&paths.root).await?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    cfg: &RunConfig,
    paths: &Paths,
    a: &mut Side,
    b: &mut Side,
    acc: &mut Accum,
    sup: &Invocation<'_>,
    wrk: &Invocation<'_>,
    session_id: &str,
    contract_id: &str,
    task_id: &str,
    a_urn: &str,
    b_urn: &str,
) -> anyhow::Result<RunOutcome> {
    // -- §6: handshake ------------------------------------------------------
    let mut session = Session::open(session_id, a_urn, b_urn)?;
    a.emit(session_id, b_urn, kinds::SESSION_OPEN, json!({"prospective": true})).await?;
    let env = b.read_latest().await?;
    b.receive(env)?;
    session.accept(b_urn)?;

    session.declare_features(b_urn, WORKER_FEATURES)?;
    b.emit(session_id, a_urn, kinds::SESSION_FEATURES, json!({"features": WORKER_FEATURES})).await?;
    let env = a.read_latest().await?;
    a.receive(env)?;

    session.declare_features(a_urn, SUPERVISOR_FEATURES)?;
    a.emit(session_id, b_urn, kinds::SESSION_FEATURES, json!({"features": SUPERVISOR_FEATURES})).await?;
    let env = b.read_latest().await?;
    b.receive(env)?;

    // -- the supervising agent authors the terms ----------------------------
    let terms_path = paths.sup.join("delegation-terms.json");
    let text = brief::author(&cfg.task, &terms_path);
    if let Some(stop) = absorb(acc, sup.require_file("01-author", &text, "delegation-terms.json").await?, Stage::Authoring, "the supervising agent did not write delegation-terms.json") {
        return Ok(stop);
    }
    let terms: Value = match read_json(&terms_path).await {
        Ok(v) => v,
        Err(e) => fail!(Stage::Authoring, "{e}"),
    };
    let output_name = match output_name_from(&terms) {
        Ok(n) => n,
        Err(e) => fail!(Stage::Authoring, "{e}"),
    };
    let acceptance = acceptance_from(&terms);
    if acceptance.is_empty() {
        fail!(Stage::Authoring, "the terms name no acceptance criteria; there would be nothing to verify");
    }
    let one_line = terms["outputs"][0]["one_line"].as_bool().unwrap_or(false);
    let media_type = terms["outputs"][0]["media_type"].as_str().unwrap_or("text/plain").to_string();

    // -- §7.3: propose ------------------------------------------------------
    let mut contract = Contract::propose(
        &session,
        contract_id,
        Task {
            task_id: task_id.to_string(),
            summary: cfg.task.clone(),
            owner: b_urn.to_string(),
        },
        Relationship::Delegation,
        // §8.3: a delegation must declare where a dispute goes. Here that is the
        // supervising agent — one link, because this run has no deeper org yet.
        vec![a_urn.to_string()],
        LIMITS,
    )?;
    let proposal_body = json!({
        "contract_id": contract_id,
        "task_id": task_id,
        "terms": terms,
    });
    a.emit(session_id, b_urn, kinds::CONTRACT_PROPOSED, proposal_body.clone()).await?;
    let env = b.read_latest().await?;
    let proposal = b.receive(env)?;

    // -- review: the performing agent decides -------------------------------
    let accept_path = paths.wrk.join("accept.json");
    let text = brief::review(&proposal.body, &accept_path);
    if let Some(stop) = absorb(acc, wrk.require_file("02-review", &text, "accept.json").await?, Stage::Review, "the performing agent did not write accept.json") {
        return Ok(stop);
    }
    let decision: Value = match read_json(&accept_path).await {
        Ok(v) => v,
        Err(e) => fail!(Stage::Review, "{e}"),
    };
    if decision["accepted"].as_bool() != Some(true) {
        // §7.4: not a failure. The contract did not form, and that is an answer.
        let reasons: Vec<String> = decision["reasons"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        contract.expire_negotiation()?;
        b.emit(session_id, a_urn, kinds::CONTRACT_NO_AGREEMENT, json!({
            "contract_id": contract_id,
            "reasons": reasons,
        }))
        .await?;
        let env = a.read_latest().await?;
        a.receive(env)?;
        close(&mut session, a, b, session_id, a_urn, b_urn, "no agreement").await?;
        let reason = if reasons.is_empty() {
            "the performing agent declined the contract".to_string()
        } else {
            format!("the performing agent declined: {}", reasons.join("; "))
        };
        return Ok(RunOutcome::NoAgreement { reason });
    }

    contract.agree(b_urn, &terms)?;
    b.emit(session_id, a_urn, kinds::CONTRACT_ACCEPTED, json!({"contract_id": contract_id, "accepted": true})).await?;
    let env = a.read_latest().await?;
    a.receive(env)?;

    contract.agree(a_urn, &terms)?;
    a.emit(session_id, b_urn, kinds::CONTRACT_ACCEPTED, json!({"contract_id": contract_id, "accepted": true})).await?;
    let env = b.read_latest().await?;
    b.receive(env)?;

    // -- §7.5: freeze -------------------------------------------------------
    let frozen_digest = contract.freeze(terms.clone())?;
    let revision = contract.revisions.len() as u64;
    a.emit(session_id, b_urn, kinds::CONTRACT_FROZEN, json!({
        "contract_id": contract_id,
        "revision": revision,
        "digest": frozen_digest,
    }))
    .await?;
    let env = b.read_latest().await?;
    let frozen = b.receive(env)?;

    // The performing side recomputes the revision digest from the §7.5 preimage rather
    // than calling the same function that produced it. That is the whole point: the
    // independent Python peer found a spec defect here precisely because it could not
    // reuse the reference's code, and a check that shares an implementation with the
    // thing it checks is not a check.
    let recomputed = canon::digest_of(&json!({
        "contract_id": contract_id,
        "revision": revision,
        "content": terms,
    }))?;
    if recomputed != frozen.body["digest"].as_str().unwrap_or_default() {
        fail!(
            Stage::Freeze,
            "the two sides disagree about the frozen revision digest (§7.5): \
             supervisor sent {}, performer computed {recomputed}",
            frozen.body["digest"]
        );
    }

    // -- EXECUTE: a contract state, not a message (§7.5) --------------------
    let artifact_path = paths.wrk.join(&output_name);
    let text = brief::work(&frozen.body, &terms, &output_name, &artifact_path, &paths.wrk);
    if let Some(stop) = absorb(acc, wrk.require_file("03-work", &text, &output_name).await?, Stage::Execute, &format!("the performing agent claims completion but {output_name} does not exist")) {
        return Ok(stop);
    }

    // -- §9.1: submission ---------------------------------------------------
    let facts = match Facts::measure(&artifact_path).await {
        Ok(f) => f,
        Err(e) => fail!(Stage::Submission, "{e}"),
    };
    let artifact = match Artifact::new(
        &format!("urn:hacp:artifact:{}", uuid::Uuid::new_v4()),
        &media_type,
        &facts.digest,
        facts.size,
        b_urn,
        task_id,
        contract_id,
        &frozen_digest,
        &relative(&paths.root, &artifact_path),
    ) {
        Ok(a) => a,
        Err(e) => fail!(Stage::Submission, "the artifact record is invalid: {e}"),
    };
    acc.artifact = Some(artifact.clone());

    contract.submit(
        b_urn,
        Submission {
            against_revision: frozen_digest.clone(),
            artifacts: vec![artifact.artifact_id.clone()],
            evidence: vec![],
            claim: format!("{output_name} written per the frozen terms"),
        },
    )?;
    b.emit(session_id, a_urn, kinds::SUBMISSION_DELIVERED, json!({
        "contract_id": contract_id,
        "against_revision": frozen_digest,
        "artifacts": [artifact.artifact_id],
        "artifacts_info": [artifact],
        "evidence": [],
        "claim": format!("{output_name} written per the frozen terms"),
    }))
    .await?;
    let env = a.read_latest().await?;
    a.receive(env)?;

    // -- §9.3: the supervising agent verifies -------------------------------
    let verdict_path = paths.sup.join("verdict.json");
    let text = brief::verify(&artifact_path, &artifact.digest, artifact.size, &facts.text(), &acceptance, &verdict_path);
    if let Some(stop) = absorb(acc, sup.require_file("04-verify", &text, "verdict.json").await?, Stage::Verification, "the verifying agent did not write verdict.json") {
        return Ok(stop);
    }
    let record: Value = match read_json(&verdict_path).await {
        Ok(v) => v,
        Err(e) => fail!(Stage::Verification, "{e}"),
    };
    let verdict = match parse_verdict(&record) {
        Ok(v) => v,
        Err(e) => fail!(Stage::Verification, "{e}"),
    };
    let checks = parse_checks(&record);
    if checks.is_empty() {
        fail!(Stage::Verification, "the verifying agent recorded no checks; a verdict with no checks is an opinion");
    }

    // Measured again, now — not reused from submission time. An artifact that changed
    // between the manifest and the verdict is exactly the case a single read cannot see.
    let now = match Facts::measure(&artifact_path).await {
        Ok(f) => f,
        Err(e) => fail!(Stage::Verification, "{e}"),
    };
    let corroboration = attest::corroborate(&checks, &now, &artifact, one_line);
    acc.corroboration = Some(corroboration.clone());
    if let Err(e) = attest::gate(&verdict, &corroboration) {
        fail!(Stage::Verification, "{e}");
    }

    let reasons: Vec<String> = record["reasons"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    // §9.4 again, this time by construction: `Verification::decide` refuses an accept
    // with no subject artifacts or no passing check. Two independent gates on the same
    // rule is deliberate — one is ours, one is the protocol's.
    let verification = Verification::decide(
        &format!("v-{}", uuid::Uuid::new_v4().simple()),
        a_urn,
        contract_id,
        &frozen_digest,
        vec![artifact.artifact_id.clone()],
        vec![],
        checks.clone(),
        verdict.clone(),
        reasons.clone(),
        vec![],
    );
    let verification = match verification {
        Ok(v) => v,
        Err(e) => fail!(Stage::Verification, "the verification record is invalid: {e}"),
    };

    a.emit(session_id, b_urn, kinds::VERIFICATION_DELIVERED, json!({
        "contract_id": contract_id,
        "verdict": verification.verdict,
        "artifacts": verification.artifacts,
        "checks": verification.checks,
        "reasons": verification.reasons,
    }))
    .await?;
    let env = b.read_latest().await?;
    let delivered = b.receive(env)?;

    // The performing side applies §9.4 to what it was sent, rather than trusting that
    // the sender applied it. A settled contract binds both parties; both check.
    if delivered.body["verdict"] == json!("accept") {
        let subjects = delivered.body["artifacts"].as_array().map(|a| a.len()).unwrap_or(0);
        let passing = delivered.body["checks"]
            .as_array()
            .map(|cs| cs.iter().any(|c| c["passed"] == json!(true)))
            .unwrap_or(false);
        if subjects == 0 || !passing {
            fail!(Stage::Verification, "an accept arrived with no subject artifacts or no passing check (§9.4)");
        }
    }

    // -- settlement ---------------------------------------------------------
    contract.decide(verdict.clone())?;
    close(&mut session, a, b, session_id, a_urn, b_urn, "run complete").await?;

    if let Err(e) = edge::transcripts_agree(&a.frames, &b.frames) {
        fail!(Stage::Transcript, "{e}");
    }

    Ok(match verdict {
        Verdict::Accept => RunOutcome::Settled { verdict: "accept".into() },
        Verdict::Reject => RunOutcome::Settled { verdict: "reject".into() },
        Verdict::Rework { .. } => RunOutcome::Settled { verdict: "rework".into() },
    })
}

async fn close(
    session: &mut Session,
    a: &mut Side,
    b: &mut Side,
    session_id: &str,
    a_urn: &str,
    b_urn: &str,
    reason: &str,
) -> anyhow::Result<()> {
    session.close(a_urn, reason)?;
    a.emit(session_id, b_urn, kinds::SESSION_CLOSE, json!({"reason": reason})).await?;
    let env = b.read_latest().await?;
    b.receive(env)?;
    Ok(())
}

/// Fold an invocation's calls into the accumulator, and say whether the run stops here.
fn absorb(acc: &mut Accum, result: CallResult, stage: Stage, missing: &str) -> Option<RunOutcome> {
    match result {
        CallResult::Produced(calls) => {
            acc.calls.extend(calls);
            None
        }
        CallResult::Missing(calls) => {
            acc.calls.extend(calls);
            Some(RunOutcome::Failed {
                stage,
                reason: missing.to_string(),
            })
        }
        CallResult::Paused { session, reason, calls } => {
            acc.calls.extend(calls);
            Some(RunOutcome::Paused {
                attach: format!("tmux attach -t {session}"),
                session,
                reason,
            })
        }
    }
}

async fn read_json(path: &Path) -> anyhow::Result<Value> {
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))
}

/// The single output file's name, validated as a name and not a path.
///
/// An agent chooses this string, and it becomes a filesystem path. `../../.ssh/authorized_keys`
/// is a valid JSON string; it is not a valid output name.
fn output_name_from(terms: &Value) -> anyhow::Result<String> {
    let name = terms["outputs"][0]["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the terms name no output file"))?;
    anyhow::ensure!(!name.is_empty(), "the output file has an empty name");
    anyhow::ensure!(
        !name.contains('/') && !name.contains('\\') && name != "." && name != "..",
        "output name {name:?} is a path, not a file name; it would write outside the workspace"
    );
    Ok(name.to_string())
}

fn acceptance_from(terms: &Value) -> Vec<String> {
    terms["acceptance"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn parse_verdict(record: &Value) -> anyhow::Result<Verdict> {
    match record["verdict"].as_str() {
        Some("accept") => Ok(Verdict::Accept),
        Some("reject") => Ok(Verdict::Reject),
        Some("rework") => Ok(Verdict::Rework {
            scope: record["reasons"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("unspecified")
                .to_string(),
        }),
        other => anyhow::bail!(
            "the verifying agent wrote verdict {other:?}; expected accept, reject, or rework"
        ),
    }
}

fn parse_checks(record: &Value) -> Vec<Check> {
    record["checks"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter_map(|c| {
                    Some(Check {
                        name: c["name"].as_str()?.to_string(),
                        passed: c["passed"].as_bool().unwrap_or(false),
                        detail: c["detail"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

/// The run's directory layout.
///
/// The two agents' workspaces are siblings of the edge, never parents of it: an agent
/// that could walk up one directory could read its peer's outbox, and "direct means
/// addressing, not transport bypass" would stop being true of this implementation.
struct Paths {
    root: PathBuf,
    a_out: PathBuf,
    b_out: PathBuf,
    sup: PathBuf,
    wrk: PathBuf,
    logs: PathBuf,
}

impl Paths {
    async fn create(root: &Path) -> anyhow::Result<Self> {
        let p = Self {
            root: root.to_path_buf(),
            a_out: root.join("edge/a-out"),
            b_out: root.join("edge/b-out"),
            sup: root.join("sup"),
            wrk: root.join("wrk"),
            logs: root.join("logs"),
        };
        for d in [&p.root, &p.a_out, &p.b_out, &p.sup, &p.wrk, &p.logs] {
            tokio::fs::create_dir_all(d).await.map_err(|e| {
                anyhow::anyhow!("cannot create run directory {}: {e}", d.display())
            })?;
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use crate::collab::{SessionHandle, SessionOutcome, SessionSpec};
    use crate::runtime::Scratch;

    /// What a scripted agent does on one invocation.
    #[derive(Clone)]
    enum Step {
        /// Write these files (paths relative to the agent's workspace) and exit 0.
        Writes(Vec<(String, String)>),
        /// Exit 0 having written nothing — the measured failure mode (findings 8–10).
        Silent,
        /// Tier-1 supervision suspended it.
        Paused(String),
    }

    /// A `SessionHost` that runs a script instead of a CLI.
    ///
    /// It exists so the lifecycle's refusals can be tested at all. Every one of them is
    /// about an agent doing something wrong, and a real CLI cannot be made to do a
    /// specific wrong thing on demand — which is exactly why those cases went unnoticed
    /// until they happened live.
    struct FakeAgent {
        steps: Mutex<VecDeque<Step>>,
        outcomes: Mutex<HashMap<String, SessionOutcome>>,
        briefs: Mutex<Vec<String>>,
    }

    impl FakeAgent {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                outcomes: Mutex::new(HashMap::new()),
                briefs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SessionHost for FakeAgent {
        async fn launch(&self, spec: &SessionSpec) -> anyhow::Result<SessionHandle> {
            self.briefs.lock().await.push(spec.prompt.clone());
            let step = self.steps.lock().await.pop_front().unwrap_or(Step::Silent);
            let outcome = match step {
                Step::Writes(files) => {
                    for (rel, contents) in files {
                        let p = spec.cwd.join(rel);
                        if let Some(parent) = p.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        tokio::fs::write(p, contents).await?;
                    }
                    SessionOutcome::Exited { code: 0 }
                }
                Step::Silent => SessionOutcome::Exited { code: 0 },
                Step::Paused(reason) => SessionOutcome::Paused { reason },
            };
            tokio::fs::write(&spec.log, "").await.ok();
            self.outcomes.lock().await.insert(spec.name.clone(), outcome);
            Ok(SessionHandle {
                name: spec.name.clone(),
                log: spec.log.clone(),
            })
        }

        async fn wait(&self, handle: &SessionHandle) -> anyhow::Result<SessionOutcome> {
            Ok(self
                .outcomes
                .lock()
                .await
                .get(&handle.name)
                .cloned()
                .unwrap_or(SessionOutcome::Exited { code: 0 }))
        }

        async fn pause(&self, _h: &SessionHandle, _reason: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn resume(&self, _h: &SessionHandle) -> anyhow::Result<()> {
            Ok(())
        }
    }

    const CONTENT: &str = "all delegated work is complete and ready\n";

    fn terms(output: &str) -> String {
        json!({
            "outputs": [{"name": output, "media_type": "text/plain", "one_line": true}],
            "acceptance": [
                "the file is exactly one line",
                "the sha256 matches the submitted manifest",
                "the file is not empty"
            ],
            "budget": {"max_minutes": 5}
        })
        .to_string()
    }

    fn write(rel: &str, contents: &str) -> Step {
        Step::Writes(vec![(rel.into(), contents.into())])
    }

    fn verdict(checks: Value) -> Step {
        write(
            "verdict.json",
            &json!({"verdict": "accept", "checks": checks, "reasons": []}).to_string(),
        )
    }

    fn good_checks() -> Value {
        json!([
            {"name": "sha256 recomputed", "passed": true, "detail": "shasum -a 256 agreed"},
            {"name": "exactly one line", "passed": true, "detail": "wc -l reported 1"}
        ])
    }

    fn happy_path() -> Vec<Step> {
        vec![
            write("delegation-terms.json", &terms("status.txt")),
            write("accept.json", &json!({"accepted": true}).to_string()),
            write("status.txt", CONTENT),
            verdict(good_checks()),
        ]
    }

    async fn run(steps: Vec<Step>, scratch: &Scratch) -> RunReport {
        let host = FakeAgent::new(steps);
        run_bilateral(
            &host,
            &RunConfig {
                supervisor: "claude".into(),
                worker: "codex".into(),
                task: "produce a one-line status report ending with the word ready".into(),
                run_dir: scratch.join("run"),
                timeout_secs: 60,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_full_lifecycle_settles_and_both_sides_agree_about_it() {
        let s = Scratch::new("lifecycle");
        let r = run(happy_path(), &s).await;
        assert_eq!(
            r.outcome,
            RunOutcome::Settled { verdict: "accept".into() },
            "{}",
            r.summary()
        );
        assert_eq!(r.frames, 10, "the exchange is a fixed shape: {}", r.summary());
        assert_eq!(r.calls.len(), 4, "one call per agent step, no retries");
        assert!(r.calls.iter().all(|c| c.produced));
        assert!(r.still_running.is_empty(), "a settled run leaves nothing behind");

        let art = r.artifact.as_ref().unwrap();
        assert_eq!(art.size, CONTENT.len() as u64);
        assert_eq!(art.digest, canon::digest_canonical(CONTENT));
        assert_eq!(art.location, "wrk/status.txt");

        let c = r.corroboration.as_ref().unwrap();
        assert_eq!(c.backed.len(), 2, "both claims were independently measured");
        assert!(c.contradicted.is_empty());

        // The record and the transcript are on disk, which is what makes a run
        // reviewable after the process is gone.
        assert!(s.join("run/run-report.json").is_file());
        let t = r.transcript.as_ref().unwrap();
        assert_eq!(std::fs::read_to_string(t).unwrap().lines().count(), 10);
    }

    #[tokio::test]
    async fn an_agent_that_narrates_success_without_writing_fails_the_run() {
        // Findings 8–10, pinned in HIVE's own code: two different real CLIs described
        // creating a file that did not exist, and every one of those calls exited 0.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[2] = Step::Silent; // the work step produces nothing
        steps.insert(3, Step::Silent); // and the retry produces nothing either
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Execute);
                assert!(reason.contains("status.txt does not exist"), "{reason}");
            }
            other => panic!("a run that produced nothing reported {other:?}"),
        }
        assert_eq!(r.outcome.exit_code(), 1);
        // Both attempts are in the record, and neither claims to have produced anything.
        let work: Vec<_> = r.calls.iter().filter(|c| c.stage.starts_with("03")).collect();
        assert_eq!(work.len(), 2, "the retry is recorded, not hidden");
        assert!(work.iter().all(|c| !c.produced));
    }

    #[tokio::test]
    async fn a_retry_recovers_an_agent_that_missed_the_first_time() {
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps.insert(2, Step::Silent); // first work attempt writes nothing
        let r = run(steps, &s).await;
        assert_eq!(r.outcome, RunOutcome::Settled { verdict: "accept".into() });
        assert_eq!(r.calls.len(), 5, "the wasted attempt is still counted");
    }

    #[tokio::test]
    async fn an_accept_no_measurement_can_back_is_refused() {
        // §9.4. The verifier is confident and specific and says nothing checkable.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[3] = verdict(json!([
            {"name": "looks correct to me", "passed": true, "detail": "I read it carefully"}
        ]));
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Verification);
                assert!(reason.contains("no claimed check survives"), "{reason}");
            }
            other => panic!("an unbacked accept reported {other:?}"),
        }
        let c = r.corroboration.as_ref().unwrap();
        assert_eq!(c.unmatched, vec!["looks correct to me"]);
    }

    #[tokio::test]
    async fn an_artifact_changed_after_its_manifest_is_caught() {
        // The reason verification re-measures rather than reusing the submission's
        // read: between the two, the file can change, and one read cannot see that.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[3] = Step::Writes(vec![
            ("../wrk/status.txt".into(), "something else entirely\n".into()),
            (
                "verdict.json".into(),
                json!({"verdict": "accept", "checks": good_checks(), "reasons": []}).to_string(),
            ),
        ]);
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Verification);
                assert!(reason.contains("measurement says otherwise"), "{reason}");
            }
            other => panic!("a swapped artifact reported {other:?}"),
        }
        let c = r.corroboration.as_ref().unwrap();
        assert!(c.contradicted.contains(&"sha256 recomputed".to_string()));
    }

    #[tokio::test]
    async fn a_worker_that_declines_ends_in_no_agreement_and_exits_green() {
        // §7.4: the contract not forming is an answer, not an error. A runtime that
        // failed here would teach agents to accept work they cannot do.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[1] = write(
            "accept.json",
            &json!({"accepted": false, "reasons": ["the output format is ambiguous"]}).to_string(),
        );
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::NoAgreement { reason } => {
                assert!(reason.contains("output format is ambiguous"), "{reason}");
            }
            other => panic!("a decline reported {other:?}"),
        }
        assert_eq!(r.outcome.exit_code(), 0);
        assert!(r.artifact.is_none());
    }

    #[tokio::test]
    async fn a_suspended_agent_is_named_with_the_command_to_take_it_over() {
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[2] = Step::Paused("Tier-1 rule matched: rm -rf /".into());
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Paused { session, reason, attach } => {
                assert!(reason.contains("rm -rf"), "{reason}");
                assert_eq!(attach, &format!("tmux attach -t {session}"));
            }
            other => panic!("a suspended session reported {other:?}"),
        }
        assert_eq!(r.outcome.exit_code(), 2, "a pause must never exit green");
        assert_eq!(r.still_running.len(), 1, "nothing is abandoned silently");
        assert!(r.summary().contains("STILL RUNNING"));
    }

    #[tokio::test]
    async fn a_suspended_agent_is_not_retried() {
        // Retrying beside a frozen session puts two agents in one workspace.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[2] = Step::Paused("tier 1".into());
        let r = run(steps, &s).await;
        assert_eq!(
            r.calls.iter().filter(|c| c.stage.starts_with("03")).count(),
            1
        );
    }

    #[tokio::test]
    async fn an_output_name_that_is_really_a_path_is_refused() {
        // The output name is chosen by an agent and becomes a filesystem path.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[0] = write("delegation-terms.json", &terms("../../.ssh/authorized_keys"));
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Authoring);
                assert!(reason.contains("is a path, not a file name"), "{reason}");
            }
            other => panic!("a path-shaped output name reported {other:?}"),
        }
    }

    #[tokio::test]
    async fn terms_with_no_acceptance_criteria_are_refused() {
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[0] = write(
            "delegation-terms.json",
            &json!({
                "outputs": [{"name": "status.txt", "media_type": "text/plain", "one_line": true}],
                "acceptance": [],
                "budget": {"max_minutes": 5}
            })
            .to_string(),
        );
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Authoring);
                assert!(reason.contains("nothing to verify"), "{reason}");
            }
            other => panic!("criterion-free terms reported {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_verdict_with_no_checks_is_an_opinion_and_is_refused() {
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[3] = verdict(json!([]));
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Verification);
                assert!(reason.contains("no checks"), "{reason}");
            }
            other => panic!("a check-free verdict reported {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_from_an_agent_names_the_file() {
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[0] = write("delegation-terms.json", "here are your terms: {oops");
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Authoring);
                assert!(reason.contains("delegation-terms.json"), "{reason}");
                assert!(reason.contains("not valid JSON"), "{reason}");
            }
            other => panic!("unparseable terms reported {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejecting_verdict_settles_without_the_corroboration_gate() {
        // Reject asks for nothing and claims nothing. Gating it would only make an
        // honest failure harder to report than a dishonest success.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[3] = write(
            "verdict.json",
            &json!({
                "verdict": "reject",
                "checks": [{"name": "reads as a draft", "passed": false, "detail": "no closing word"}],
                "reasons": ["the file does not end with the required word"]
            })
            .to_string(),
        );
        let r = run(steps, &s).await;
        assert_eq!(r.outcome, RunOutcome::Settled { verdict: "reject".into() });
    }

    #[tokio::test]
    async fn the_run_record_names_the_tools_and_the_wire_never_does() {
        // §3: the role-to-vendor mapping lives in the run record and nowhere else.
        let s = Scratch::new("lifecycle");
        let r = run(happy_path(), &s).await;
        assert_eq!(r.supervisor_cli, "claude");
        assert_eq!(r.worker_cli, "codex");

        let transcript = std::fs::read_to_string(r.transcript.as_ref().unwrap()).unwrap();
        for vendor in super::super::cli::KNOWN {
            assert!(
                !transcript.contains(vendor),
                "the transcript names the vendor {vendor:?}"
            );
        }
        // And the URNs on the wire carry no vendor either.
        assert!(transcript.contains("urn:hacp:agent:sup-"));
        assert!(transcript.contains("urn:hacp:agent:wrk-"));
    }

    #[tokio::test]
    async fn each_agent_only_ever_sees_its_own_workspace() {
        // An agent that could reach its peer's outbox would make "direct means
        // addressing, not transport bypass" untrue of this implementation.
        let s = Scratch::new("lifecycle");
        let _ = run(happy_path(), &s).await;
        let root = s.join("run");
        assert!(root.join("edge/a-out").is_dir());
        assert!(root.join("edge/b-out").is_dir());
        for side in ["sup", "wrk"] {
            let ws = root.join(side);
            assert!(ws.is_dir());
            assert!(
                !ws.join("edge").exists(),
                "{side}'s workspace contains the edge"
            );
        }
    }
}
