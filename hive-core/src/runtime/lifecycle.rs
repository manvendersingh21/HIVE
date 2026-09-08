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
//!   settlement    contract.apply_verification, session.close, transcripts compared
//! ```
//!
//! **Where the reasoning lives.** The agents decide *what* — terms, acceptance,
//! verdicts. This module decides *nothing* about the work and everything about whether
//! a claim may advance the run. That split is the protocol's whole bet, and the reason
//! findings 8–10 were caught rather than shipped.

use std::path::{Path, PathBuf};

use hacp::v2::contract::{ContractLimits, Relationship, Submission, Task, Verdict};
use hacp::v2::{
    canon, kinds, Artifact, Check, Contract, ContractState, Envelope, Session, Verification,
};
use serde_json::{json, Value};

use crate::collab::SessionHost;

use super::attest::{self, Corroboration, Facts};
use super::brief::{self, AgentCall, CallResult, Invocation};
use super::cli::AgentCli;
use super::edge::{self, Side};
use super::report::{LiveSession, RunOutcome, RunReport, Stage};

/// One run's inputs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
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
    #[serde(default)]
    pub supervisor_placement: super::hosting::Placement,
    #[serde(default)]
    pub worker_placement: super::hosting::Placement,
    #[serde(default)]
    pub max_rework: u32,
}

/// Features this runtime declares (§6.3). A feature both sides declare is available;
/// what one side did not declare, the other may not rely on.
const SUPERVISOR_FEATURES: &[&str] = &[
    "supervision",
    "delegation",
    "artifact-digest",
    "observer-events",
];
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
    artifacts: Vec<Artifact>,
    corroboration: Option<Corroboration>,
    still_running: Vec<LiveSession>,
    executions: Vec<super::verification::Execution>,
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
    run_persistent(host, host, cfg, false, None).await
}

/// Production entry point: each role gets its explicitly configured device.
pub async fn run_configured(cfg: &RunConfig) -> anyhow::Result<RunReport> {
    let (sup, wrk) = tokio::try_join!(
        super::hosting::make_host(&cfg.supervisor_placement, &cfg.run_dir),
        super::hosting::make_host(&cfg.worker_placement, &cfg.run_dir))?;
    run_persistent(sup.as_ref(), wrk.as_ref(), cfg, false, None).await
}

pub async fn resume_configured(root: &Path) -> anyhow::Result<RunReport> {
    let cfg = recorded_config(root)?;
    let (sup, wrk) = tokio::try_join!(
        super::hosting::make_host(&cfg.supervisor_placement, &cfg.run_dir),
        super::hosting::make_host(&cfg.worker_placement, &cfg.run_dir))?;
    run_persistent(sup.as_ref(), wrk.as_ref(), &cfg, true, None).await
}

/// Explicit operator action. The named invocation and its role come from this
/// run's journal, not from an arbitrary tmux name supplied on the command line.
pub async fn authorize_continuation(root: &Path, name: &str, reason: &str) -> anyhow::Result<()> {
    let cfg = recorded_config(root)?;
    let _lock = super::journal::RunLock::acquire(&cfg.run_dir)?;
    let journal = super::journal::Journal::open(&cfg.run_dir.join("runtime.db"))?;
    let record = journal.invocation(name)?;
    let result = record.result.as_ref().ok_or_else(|| anyhow::anyhow!("invocation has no recorded suspension"))?;
    let role = result["call"]["role"].as_str().or_else(|| result["role"].as_str())
        .ok_or_else(|| anyhow::anyhow!("invocation has no recorded role"))?;
    let placement = match role {
        "supervisor" => &cfg.supervisor_placement,
        "worker" => &cfg.worker_placement,
        _ => anyhow::bail!("unknown recorded invocation role"),
    };
    let host = super::hosting::make_host(placement, &cfg.run_dir).await?;
    let handle = host.recover(&record.spec, record.started_unix).await?;
    let bytes = host.log_size(&handle).await?;
    journal.authorize_continue(name, reason, bytes)
}

/// Resume recorded inputs through the protocol library without repeating completed
/// invocations. An unresolved host effect must be recovered, never blindly relaunched.
pub async fn resume_bilateral(host: &dyn SessionHost, root: &Path) -> anyhow::Result<RunReport> {
    let cfg = recorded_config(root)?;
    run_persistent(host, host, &cfg, true, None).await
}

fn recorded_config(root: &Path) -> anyhow::Result<RunConfig> {
    anyhow::ensure!(root.join("runtime.db").is_file(), "no durable runtime journal in this run");
    let journal = super::journal::Journal::open(&root.join("runtime.db"))?;
    let record = journal.get("run")?.ok_or_else(|| anyhow::anyhow!("journal has no run identity"))?;
    let cfg: RunConfig = serde_json::from_value(record["config"].clone())?;
    anyhow::ensure!(std::fs::canonicalize(root)? == std::fs::canonicalize(&cfg.run_dir)?,
        "run workspace moved; original invocation paths must be reconciled before resume");
    Ok(cfg)
}

async fn run_persistent(sup_host: &dyn SessionHost, wrk_host: &dyn SessionHost, cfg: &RunConfig, resume: bool,
    journal: Option<super::journal::Journal>) -> anyhow::Result<RunReport> {
    let sup_cli = AgentCli::resolve(&cfg.supervisor)?;
    anyhow::ensure!(cfg.max_rework <= 5, "max_rework must be at most 5");
    let wrk_cli = AgentCli::resolve(&cfg.worker)?;
    tokio::fs::create_dir_all(&cfg.run_dir).await?;
    let mut normalized = cfg.clone();
    normalized.run_dir = std::fs::canonicalize(&cfg.run_dir)?;
    let cfg = &normalized;
    let paths = Paths::create(&cfg.run_dir).await?;
    let _lock = super::journal::RunLock::acquire(&paths.root)?;
    let journal = match journal {
        Some(j) => j,
        None => super::journal::Journal::open(&paths.root.join("runtime.db"))?,
    };
    anyhow::ensure!(resume || journal.get("run")?.is_none(),
        "run already initialized; use hive collab resume to recover it");
    let run_tag = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();

    // §3: neutral, vendor-free identities. Nothing downstream can recover the tool from
    // these, which is the point — the mapping lives in the run report and nowhere else.
    let a_urn = format!("urn:hacp:agent:sup-{run_tag}");
    let b_urn = format!("urn:hacp:agent:wrk-{run_tag}");
    let session_id = format!(
        "s-{run_tag}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..4]
    );
    let contract_id = format!(
        "c-{run_tag}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..4]
    );
    let task_id = format!(
        "t-{run_tag}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..4]
    );
    let identity = journal.initialize("run", &json!({"config": cfg, "run_tag": run_tag,
        "a_urn":a_urn,"b_urn":b_urn,"session_id":session_id,"contract_id":contract_id,"task_id":task_id}))?;
    anyhow::ensure!(serde_json::from_value::<RunConfig>(identity["config"].clone())? == *cfg, "run configuration changed");
    let field = |key: &str| -> anyhow::Result<String> {
        identity[key].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("missing run identity {key}"))
    };
    let run_tag = field("run_tag")?;
    let a_urn = field("a_urn")?;
    let b_urn = field("b_urn")?;
    let session_id = field("session_id")?;
    let contract_id = field("contract_id")?;
    let task_id = field("task_id")?;

    let mut a = Side::new("a", &a_urn, paths.a_out.clone(), paths.b_out.clone()).durable(journal.clone());
    let mut b = Side::new("b", &b_urn, paths.b_out.clone(), paths.a_out.clone()).durable(journal.clone());
    let mut acc = Accum::default();

    let sup = Invocation {
        host: sup_host,
        cli: sup_cli,
        role: "supervisor",
        cwd: &paths.sup,
        conversation_cwd: &paths.sup,
        logs: &paths.logs,
        run_tag: &run_tag,
        timeout_secs: cfg.timeout_secs,
        journal: Some(&journal),
        model: cfg.supervisor_placement.model.as_deref(),
    };
    let wrk = Invocation {
        host: wrk_host,
        cli: wrk_cli,
        role: "worker",
        cwd: &paths.wrk,
        conversation_cwd: &paths.wrk,
        logs: &paths.logs,
        run_tag: &run_tag,
        timeout_secs: cfg.timeout_secs,
        journal: Some(&journal),
        model: cfg.worker_placement.model.as_deref(),
    };

    let mut outcome = drive(
        cfg,
        &paths,
        &mut a,
        &mut b,
        &mut acc,
        &sup,
        &wrk,
        &session_id,
        &contract_id,
        &task_id,
        &a_urn,
        &b_urn,
    )
    .await?;

    // Clean shutdown: whatever this runtime started and did not see end is named here
    // with the command to take it over. Nothing is killed — a suspended agent is the
    // state a person was just asked to inspect.
    if let RunOutcome::Paused { session, .. } = &outcome {
        let is_worker = acc.executions.iter().find(|e| &e.session == session)
            .map(|e| e.role == "worker").unwrap_or_else(|| acc.calls.last().is_some_and(|c| c.role == "worker"));
        let placement = if is_worker { &cfg.worker_placement } else { &cfg.supervisor_placement };
        let attach = match &placement.host {
            Some(host) => format!("ssh -t {host} 'exec \"$SHELL\" -lc \"tmux attach -t {session}\"'"),
            None => format!("tmux attach -t {session}"),
        };
        acc.still_running.push(LiveSession {
            name: session.clone(),
            attach,
        });
    }
    if let RunOutcome::Paused { attach, .. } = &mut outcome {
        if let Some(live) = acc.still_running.last() { attach.clone_from(&live.attach); }
    }

    let transcript = match edge::transcript_lines(&a.frames) {
        Ok(lines) if !lines.is_empty() => {
            let p = paths
                .root
                .join(format!("{}x{}.jsonl", cfg.supervisor, cfg.worker));
            tokio::fs::write(&p, lines).await?;
            Some(p)
        }
        _ => None,
    };

    let report = RunReport {
        pair: format!("{} x {}", cfg.supervisor, cfg.worker),
        supervisor_cli: cfg.supervisor.clone(),
        worker_cli: cfg.worker.clone(),
        supervisor_placement:cfg.supervisor_placement.clone(),
        worker_placement:cfg.worker_placement.clone(),
        run_dir: paths.root.clone(),
        session_id,
        contract_id,
        task_id,
        task: cfg.task.clone(),
        outcome,
        frames: a.frames.len(),
        calls: acc.calls,
        artifact: acc.artifact,
        artifacts: acc.artifacts,
        executions: acc.executions,
        corroboration: acc.corroboration,
        transcript,
        still_running: acc.still_running,
    };
    report.write(&paths.root).await?;
    journal.checkpoint("report", &serde_json::to_value(&report)?)?;
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
    a.emit(
        session_id,
        b_urn,
        kinds::SESSION_OPEN,
        json!({"prospective": true}),
    )
    .await?;
    let env = b.read_latest().await?;
    b.receive_in_session(env, &session)?;
    session.accept(b_urn)?;

    session.declare_features(b_urn, WORKER_FEATURES)?;
    b.emit(
        session_id,
        a_urn,
        kinds::SESSION_FEATURES,
        json!({"features": WORKER_FEATURES}),
    )
    .await?;
    let env = a.read_latest().await?;
    a.receive_in_session(env, &session)?;

    session.declare_features(a_urn, SUPERVISOR_FEATURES)?;
    a.emit(
        session_id,
        b_urn,
        kinds::SESSION_FEATURES,
        json!({"features": SUPERVISOR_FEATURES}),
    )
    .await?;
    let env = b.read_latest().await?;
    b.receive_in_session(env, &session)?;

    if let Some(journal) = sup.journal {
        journal.checkpoint("handshake", &json!({"session": session}))?;
    }

    // -- the supervising agent authors the terms ----------------------------
    let terms_path = paths.sup.join("delegation-terms.json");
    let text = brief::author(&cfg.task, &terms_path);
    if let Some(stop) = absorb(
        acc,
        sup.require_json("01-author", &text, "delegation-terms.json")
            .await?,
        Stage::Authoring,
        "the supervising agent did not write delegation-terms.json (missing or not valid JSON after retry)",
    ) {
        return Ok(stop);
    }
    let mut terms: Value = match read_json(&terms_path).await {
        Ok(v) => v,
        Err(e) => fail!(Stage::Authoring, "{e}"),
    };
    if let Err(e) = validate_terms(&terms) { fail!(Stage::Authoring, "{e}"); }

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
    a.emit(
        session_id,
        b_urn,
        kinds::CONTRACT_PROPOSED,
        proposal_body.clone(),
    )
    .await?;
    let env = b.read_latest().await?;
    let mut proposal = b.receive_in_session(env, &session)?;

    // -- review: the performing agent decides -------------------------------
    let mut review_round = 1;
    let mut supervisor_accepted = false;
    loop {
    let review_workspace = if review_round == 1 { paths.wrk.clone() }
        else { paths.root.join(format!("negotiation/worker-{review_round}")) };
    tokio::fs::create_dir_all(&review_workspace).await?;
    let reviewer = Invocation { cwd:&review_workspace, ..*wrk };
    let accept_path = review_workspace.join("accept.json");
    let text = brief::review(&proposal.body, &accept_path);
    if let Some(stop) = absorb(
        acc,
        reviewer.require_json(&if review_round == 1 { "02-review".into() } else { format!("02-review-n{review_round}") }, &text, "accept.json").await?,
        Stage::Review,
        "the performing agent did not write accept.json (missing or not valid JSON after retry)",
    ) {
        return Ok(stop);
    }
    let decision: Value = match read_json(&accept_path).await {
        Ok(v) => v,
        Err(e) => fail!(Stage::Review, "{e}"),
    };
    if decision["accepted"].as_bool().is_none() { fail!(Stage::Review, "review must declare a boolean accepted field"); }
    if decision.get("question").is_some_and(|q| q.as_str().is_none_or(|s| s.trim().is_empty())) {
        fail!(Stage::Review, "clarification question must be a nonempty string");
    }
    if decision["accepted"].as_bool() == Some(true) {
        if decision.get("counter_terms").is_some() || decision.get("question").is_some() {
            fail!(Stage::Review, "acceptance cannot simultaneously counter or request clarification");
        }
        break;
    }
    if decision.get("counter_terms").is_some() || decision.get("question").is_some() {
        let candidate = decision.get("counter_terms").cloned().unwrap_or_else(|| terms.clone());
        if let Err(e) = validate_terms(&candidate) { fail!(Stage::Review, "invalid counter terms: {e}"); }
        if let Err(e) = contract.counter(b_urn) {
            if contract.state == ContractState::NoAgreement {
                return negotiation_exhausted(&mut session, a, b, session_id, a_urn, b_urn, contract_id, &e.to_string()).await;
            }
            return Err(e.into());
        }
        supervisor_accepted = false;
        b.emit(session_id, a_urn, kinds::CONTRACT_COUNTERED,
            json!({"contract_id":contract_id,"terms":candidate,"question":decision.get("question"),"reasons":decision.get("reasons")})).await?;
        let counter = a.receive_in_session(a.read_latest().await?, &session)?;
        let response_name = format!("counter-{review_round}.json");
        let prompt = format!("The worker has countered or requested clarification. Original objective:\n{}\nCurrent terms:\n{}\nCounter received:\n{}\nWrite {} with either {{\"accepted\":true}} to adopt the proposed terms, {{\"accepted\":false,\"terms\":<complete revised terms>,\"answer\":\"clarification\"}} to propose a revision, or {{\"accepted\":false,\"reasons\":[\"decline\"]}} to end negotiation. Do not weaken the original objective or its tests. The worker must explicitly accept any revised terms before work starts.",
            cfg.task, terms, counter.body, paths.sup.join(&response_name).display());
        if let Some(stop) = absorb(acc, sup.require_json(&format!("02-counter-n{review_round}"), &prompt, &response_name).await?,
            Stage::Review, "supervisor did not produce a counter response") { return Ok(stop); }
        let response = read_json(&paths.sup.join(response_name)).await?;
        if response["accepted"].as_bool().is_none() { fail!(Stage::Review, "counter response must declare a boolean accepted field"); }
        let kind = if response["accepted"] == true {
            if response.get("terms").is_some() { fail!(Stage::Review, "counter acceptance cannot also change terms"); }
            terms = candidate;
            contract.agree(a_urn, &terms)?;
            supervisor_accepted = true;
            kinds::CONTRACT_ACCEPTED
        } else if let Some(revised) = response.get("terms") {
            if let Err(e) = validate_terms(revised) { fail!(Stage::Review, "invalid revised terms: {e}"); }
            if let Err(e) = contract.counter(a_urn) {
                if contract.state == ContractState::NoAgreement {
                    return negotiation_exhausted(&mut session, a, b, session_id, a_urn, b_urn, contract_id, &e.to_string()).await;
                }
                return Err(e.into());
            }
            terms = revised.clone();
            kinds::CONTRACT_COUNTERED
        } else {
            contract.expire_negotiation()?;
            return negotiation_exhausted(&mut session, a, b, session_id, a_urn, b_urn, contract_id,
                "supervisor declined the counterproposal").await;
        };
        a.emit(session_id, b_urn, kind, json!({"contract_id":contract_id,"terms":terms,
            "accepted":kind == kinds::CONTRACT_ACCEPTED,"answer":response.get("answer")})).await?;
        proposal = b.receive_in_session(b.read_latest().await?, &session)?;
        review_round += 1;
        continue;
    }
    if decision["accepted"].as_bool() != Some(true) {
        // §7.4: not a failure. The contract did not form, and that is an answer.
        let reasons: Vec<String> = decision["reasons"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        contract.expire_negotiation()?;
        b.emit(
            session_id,
            a_urn,
            kinds::CONTRACT_NO_AGREEMENT,
            json!({
                "contract_id": contract_id,
                "reasons": reasons,
            }),
        )
        .await?;
        let env = a.read_latest().await?;
        a.receive_in_session(env, &session)?;
        close(&mut session, a, b, session_id, a_urn, b_urn, "no agreement").await?;
        let reason = if reasons.is_empty() {
            "the performing agent declined the contract".to_string()
        } else {
            format!("the performing agent declined: {}", reasons.join("; "))
        };
        return Ok(RunOutcome::NoAgreement { reason });
    }
    }

    let outputs = outputs_from(&terms)?;
    let acceptance = acceptance_from(&terms)?;
    let output_names: Vec<String> = outputs.iter().map(|o| o.name.clone()).collect();
    let output_name = output_names.join(", ");
    let verification_plan = super::verification::VerificationPlan::from_terms(&terms, &output_names)?;
    let file_acceptance: Vec<String> = acceptance.iter().filter(|a| a.as_str() != "the frozen acceptance suite passes").cloned().collect();

    contract.agree(b_urn, &terms)?;
    b.emit(
        session_id,
        a_urn,
        kinds::CONTRACT_ACCEPTED,
        json!({"contract_id": contract_id, "accepted": true}),
    )
    .await?;
    let env = a.read_latest().await?;
    a.receive_in_session(env, &session)?;

    if !supervisor_accepted {
    contract.agree(a_urn, &terms)?;
    a.emit(
        session_id,
        b_urn,
        kinds::CONTRACT_ACCEPTED,
        json!({"contract_id": contract_id, "accepted": true}),
    )
    .await?;
    let env = b.read_latest().await?;
    b.receive_in_session(env, &session)?;
    }

    // -- §7.5: freeze -------------------------------------------------------
    let frozen_digest = contract.freeze(terms.clone())?;
    if let Some(journal) = sup.journal {
        journal.checkpoint("frozen", &json!({"session":session,"contract":contract}))?;
    }
    let revision = contract.revisions.len() as u64;
    a.emit(
        session_id,
        b_urn,
        kinds::CONTRACT_FROZEN,
        json!({
            "contract_id": contract_id,
            "revision": revision,
            "digest": frozen_digest,
        }),
    )
    .await?;
    let env = b.read_latest().await?;
    let frozen = b.receive_in_session(env, &session)?;

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

    // Each repair gets a new workspace and invocation identities. Earlier output
    // receipts must remain verifiable instead of being overwritten by later edits.
    let mut feedback = String::new();
    let mut previous_work = paths.wrk.clone();
    for attempt in 1..=cfg.max_rework + 1 {
    let attempt_paths = paths.for_attempt(attempt).await?;
    let paths = &attempt_paths;
    if attempt > 1 {
        for output in &outputs {
            let destination = super::hosting::workspace_output(&paths.wrk, &output.name)?;
            if !destination.exists() {
                tokio::fs::create_dir_all(destination.parent().expect("output parent")).await?;
                tokio::fs::copy(super::hosting::workspace_output(&previous_work, &output.name)?, destination).await?;
            }
        }
    }
    let sup = Invocation { cwd:&paths.sup, ..*sup };
    let wrk = Invocation { cwd:&paths.wrk, ..*wrk };
    let work_stage = if attempt == 1 { "03-work".to_string() } else { format!("03-work-a{attempt}") };
    let verify_stage = if attempt == 1 { "04-verify".to_string() } else { format!("04-verify-a{attempt}") };
    // -- EXECUTE: a contract state, not a message (§7.5) --------------------
    let artifact_path = paths.wrk.join(&outputs[0].name);
    let mut text = if outputs.len() == 1 { brief::work(
        &frozen.body,
        &terms,
        &output_name,
        &artifact_path,
        &paths.wrk,
    ) } else {
        format!("Complete every output of this frozen delegation inside {}. Create the listed files with your file-writing tools, including nested directories. Do not merely print their contents.\nFrozen revision:\n{}\nTerms:\n{}\nRequired absolute output paths:\n{}",
            paths.wrk.display(), frozen.body, terms,
            outputs.iter().map(|o| paths.wrk.join(&o.name).display().to_string()).collect::<Vec<_>>().join("\n"))
    };
    if !feedback.is_empty() { text.push_str(&format!("\n\nThe preceding submission requires rework. Repair the supplied previous files without weakening the frozen contract. Verifier feedback:\n{feedback}")); }
    if let Some(stop) = absorb(
        acc,
        wrk.require_files(&work_stage, &text, &output_names).await?,
        Stage::Execute,
        &format!("the performing agent claims completion but {output_name} does not exist"),
    ) {
        return Ok(stop);
    }

    // -- §9.1: submission ---------------------------------------------------
    let mut measurements = Vec::new();
    let mut artifacts = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        let path = super::hosting::workspace_output(&paths.wrk, &output.name)?;
        let facts = match Facts::measure(&path).await {
            Ok(f) => f,
            Err(e) => fail!(Stage::Submission, "{e}"),
        };
        let new_id = json!(format!("urn:hacp:artifact:{}", uuid::Uuid::new_v4()));
        let mut key = if index == 0 { "artifact-id".to_string() } else { format!("artifact-id:{}", output.name) };
        if attempt > 1 { key.push_str(&format!(":a{attempt}")); }
        let id = match sup.journal {
            Some(journal) => journal.initialize(&key, &new_id)?,
            None => new_id,
        };
        let artifact = match Artifact::new(
            id.as_str().ok_or_else(|| anyhow::anyhow!("invalid persisted artifact ID"))?,
            &output.media_type, &facts.digest, facts.size, b_urn, task_id, contract_id,
            &frozen_digest, &relative(&paths.root, &path),
        ) {
            Ok(a) => a,
            Err(e) => fail!(Stage::Submission, "the artifact record is invalid: {e}"),
        };
        measurements.push(facts);
        artifacts.push(artifact);
    }
    let artifact_ids: Vec<String> = artifacts.iter().map(|a| a.artifact_id.clone()).collect();
    acc.artifact = artifacts.first().cloned();
    acc.artifacts = artifacts.clone();

    contract.submit(
        b_urn,
        Submission {
            against_revision: frozen_digest.clone(),
            artifacts: artifact_ids.clone(),
            evidence: vec![],
            claim: format!("{output_name} written per the frozen terms"),
        },
    )?;
    if let Some(journal) = sup.journal {
        journal.checkpoint("submitted", &json!({"session":session,"contract":contract,"artifacts":artifacts}))?;
    }
    b.emit(
        session_id,
        a_urn,
        kinds::SUBMISSION_DELIVERED,
        json!({
            "contract_id": contract_id,
            "against_revision": frozen_digest,
            "artifacts": artifact_ids,
            "artifacts_info": artifacts,
            "evidence": [],
            "claim": format!("{output_name} written per the frozen terms"),
        }),
    )
    .await?;
    let env = a.read_latest().await?;
    a.receive_in_session(env, &session)?;

    // -- §9.3: the supervising agent verifies -------------------------------
    let verdict_path = paths.sup.join("verdict.json");
    // The verifier receives only the submitted artifact, in its own workspace.
    // This works identically for either role on SSH and keeps peer workspaces private.
    let mut submitted_records = Vec::new();
    for ((output, facts), artifact) in outputs.iter().zip(&measurements).zip(&artifacts) {
        let submitted = paths.sup.join("submitted").join(&output.name);
        tokio::fs::create_dir_all(submitted.parent().expect("submitted parent")).await?;
        tokio::fs::write(&submitted, &facts.bytes).await?;
        submitted_records.push(json!({"path":submitted,"digest":artifact.digest,"size":artifact.size,"content":facts.text()}));
    }
    let mut test_checks = Vec::new();
    let mut test_observations = Vec::new();
    if let Some(plan) = &verification_plan {
        for (role, host) in [("supervisor", sup.host), ("worker", wrk.host)] {
            let workspace = paths.root.join(format!("checks/attempt-{attempt}-{role}"));
            tokio::fs::create_dir_all(&workspace).await?;
            for (output, facts) in outputs.iter().zip(&measurements) {
                let destination = super::hosting::workspace_output(&workspace, &output.name)?;
                tokio::fs::create_dir_all(destination.parent().expect("test input parent")).await?;
                tokio::fs::write(destination, &facts.bytes).await?;
            }
            plan.materialize(&workspace).await?;
            for (index, command) in plan.commands.iter().enumerate() {
                let stage = format!("tests-a{attempt}-{role}-{index}");
                let execution = super::verification::execute(host,
                    sup.journal.ok_or_else(|| anyhow::anyhow!("executable verification requires a journal"))?,
                    role, sup.run_tag, &stage, &workspace, &paths.logs, command).await?;
                let log = tokio::fs::read_to_string(&execution.log).await?;
                let failure = super::verification::acceptance_failure(command, &execution.outcome, &log);
                let passed = failure.is_none();
                let supplemental_failure = failure.filter(|_|
                    matches!(execution.outcome, crate::collab::SessionOutcome::Exited { code:0 }));
                let mut observation = json!({"role":role,"command":index,"outcome":execution.outcome,
                    "log":log.chars().take(16000).collect::<String>()});
                // Leave successful receipts/briefs stable for existing settled runs.
                // Failed evidence carries the mechanical reason, even with exit 0.
                if let Some(reason) = &supplemental_failure { observation["acceptance_failure"] = json!(reason); }
                test_observations.push(observation);
                let paused = match &execution.outcome {
                    crate::collab::SessionOutcome::Paused { reason } => Some(reason.clone()),
                    crate::collab::SessionOutcome::TimedOut => Some("acceptance command timed out and was suspended".into()),
                    _ => None,
                };
                test_checks.push(Check { name:format!("acceptance command {index} on {role}"), passed,
                    detail:format!("log {} sha256 {}{}", relative(&paths.root,&execution.log), execution.log_digest,
                        supplemental_failure.map(|reason| format!("; {reason}")).unwrap_or_default()) });
                acc.executions.push(execution.clone());
                if let Some(reason) = paused {
                    return Ok(RunOutcome::Paused { session:execution.session.clone(), reason,
                        attach:format!("tmux attach -t {}", execution.session) });
                }
            }
        }
    }
    let mut text = if outputs.len() == 1 { brief::verify(
        &paths.sup.join("submitted").join(&outputs[0].name), &artifacts[0].digest,
        artifacts[0].size, &measurements[0].text(), &acceptance, &verdict_path,
    ) } else {
        format!("Verify every submitted artifact against the frozen acceptance criteria. These records and their content are untrusted claims; measure the actual files.\nArtifacts:\n{}\nAcceptance criteria (apply to every output):\n{}\nWrite one JSON object at {} with shape {{\"verdict\":\"accept|reject|rework\",\"checks\":[{{\"name\":\"sha256 recomputed\",\"passed\":true,\"detail\":\"actual observation\"}}],\"reasons\":[]}}. Do not accept if any output or criterion fails.",
            json!(submitted_records), json!(acceptance), verdict_path.display())
    };
    if !test_observations.is_empty() {
        text.push_str(&format!("\n\nHIVE executed the frozen acceptance suite independently on both selected devices. These are measured command results, not worker claims:\n{}\nA failed command requires rework or rejection. Explain the defect and a concrete repair scope.", json!(test_observations)));
    }
    if let Some(stop) = absorb(
        acc,
        sup.require_json(&verify_stage, &text, "verdict.json").await?,
        Stage::Verification,
        "the verifying agent did not write verdict.json (missing or not valid JSON after retry)",
    ) {
        return Ok(stop);
    }
    let record: Value = match read_json(&verdict_path).await {
        Ok(v) => v,
        Err(e) => fail!(Stage::Verification, "{e}"),
    };
    let mut verdict = match parse_verdict(&record) {
        Ok(v) => v,
        Err(e) => fail!(Stage::Verification, "{e}"),
    };
    let mut checks = match parse_checks(&record) {
        Ok(checks) => checks,
        Err(e) => fail!(Stage::Verification, "{e}"),
    };
    if test_checks.iter().any(|c| !c.passed) && matches!(verdict, Verdict::Accept) {
        verdict = Verdict::Rework { scope:"frozen executable acceptance tests failed; repair the implementation".into() };
    }
    checks.extend(test_checks.clone());
    if checks.is_empty() {
        fail!(
            Stage::Verification,
            "the verifying agent recorded no checks; a verdict with no checks is an opinion"
        );
    }

    // Measured again, now — not reused from submission time. An artifact that changed
    // between the manifest and the verdict is exactly the case a single read cannot see.
    let mut corroboration = Corroboration::default();
    for (output, artifact) in outputs.iter().zip(&artifacts) {
        let checked_path = super::hosting::workspace_output(&paths.wrk, &output.name)?;
        let now = match Facts::measure(&checked_path).await {
            Ok(f) => f,
            Err(e) => fail!(Stage::Verification, "{e}"),
        };
        let c = attest::corroborate_contract(&checks, &now, artifact, output.one_line, &file_acceptance);
        let label = |text:String| if outputs.len() == 1 { text } else { format!("{}: {text}", output.name) };
        corroboration.backed.extend(c.backed.into_iter().map(label));
        corroboration.unmatched.extend(c.unmatched.into_iter().map(label));
        corroboration.contradicted.extend(c.contradicted.into_iter().map(label));
        corroboration.requirements_backed.extend(c.requirements_backed.into_iter().map(label));
    }
    corroboration.backed.extend(test_checks.iter().filter(|c| c.passed).map(|c| c.name.clone()));
    corroboration.contradicted.extend(test_checks.iter().filter(|c| !c.passed).map(|c| c.name.clone()));
    acc.corroboration = Some(corroboration.clone());
    if let Err(e) = attest::gate(&verdict, &corroboration) {
        fail!(Stage::Verification, "{e}");
    }

    let reasons: Vec<String> = record["reasons"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // §9.4 again, this time by construction: `Verification::decide` refuses an accept
    // with no subject artifacts, no passing check, or any failed check. Two gates on the same
    // rule is deliberate — one is ours, one is the protocol's.
    let verification = Verification::decide(
        &format!("v-{}", canon::digest_canonical(&format!("{session_id}-verification-{attempt}"))),
        a_urn,
        contract_id,
        &frozen_digest,
        artifact_ids,
        vec![],
        checks.clone(),
        verdict.clone(),
        reasons.clone(),
        vec![],
    );
    let verification = match verification {
        Ok(v) => v,
        Err(e) => fail!(
            Stage::Verification,
            "the verification record is invalid: {e}"
        ),
    };

    // Carry the complete library wire type, including identity and revision.
    a.emit(
        session_id,
        b_urn,
        kinds::VERIFICATION_DELIVERED,
        serde_json::to_value(&verification)?,
    )
    .await?;
    let env = b.read_latest().await?;
    let delivered = b.receive_in_session(env, &session)?;

    // Apply the record read from the edge, not the sender's in-memory verdict.
    // HACP owns validation and the state transition; HIVE owns measurements and transport.
    if let Err(e) = apply_delivered_verification(&mut contract, &session, &delivered, b_urn) {
        fail!(Stage::Verification, "{e}");
    }
    if let Some(journal) = sup.journal {
        journal.checkpoint("verified", &json!({"session":session,"contract":contract}))?;
    }

    // -- settlement ---------------------------------------------------------
    if contract.state == ContractState::Executing {
        feedback = format!("{}\nExecutable test observations:\n{}",
            contract.rework_scope.clone().unwrap_or_default(), json!(test_observations));
        previous_work = paths.wrk.clone();
        if attempt <= cfg.max_rework { continue; }
        // HACP's Rework transition is not terminal. The configured repair budget
        // is exhausted: preserve trusted state and report unfinished work, never
        // quietly spend another model invocation or turn rework into acceptance.
        let state_path = paths.root.join("rework-state.json");
        tokio::fs::write(
            &state_path,
            serde_json::to_vec_pretty(&json!({
                "session": session, "contract": contract,
            }))?,
        )
        .await?;
        if let Err(e) = edge::transcripts_agree(&a.frames, &b.frames) {
            fail!(Stage::Transcript, "{e}");
        }
        return Ok(RunOutcome::ReworkRequired {
            scope: contract.rework_scope.clone().unwrap_or_default(),
            state_path,
        });
    }
    close(&mut session, a, b, session_id, a_urn, b_urn, "run complete").await?;

    if let Err(e) = edge::transcripts_agree(&a.frames, &b.frames) {
        fail!(Stage::Transcript, "{e}");
    }

    return Ok(match contract.state {
        ContractState::Settled => RunOutcome::Settled {
            verdict: "accept".into(),
        },
        ContractState::Rejected => RunOutcome::Rejected { reasons },
        state => RunOutcome::Failed {
            stage: Stage::Settlement,
            reason: format!("unexpected HACP state: {state:?}"),
        },
    });
    }
    unreachable!("bounded execution loop always returns at its limit")
}

/// A binding adapter checks routing/authorship; the library checks the record
/// and advances its own state machine. URNs do not authenticate a remote host:
/// this runtime's edge is local and must be protected by its deployment.
fn apply_delivered_verification(
    contract: &mut Contract,
    session: &Session,
    delivered: &Envelope,
    recipient: &str,
) -> anyhow::Result<()> {
    delivered.validate()?;
    session.authorize_author(&delivered.from)?;
    session.authorize_author(recipient)?;
    anyhow::ensure!(
        session.state == hacp::v2::SessionState::Active,
        "verification requires an active session"
    );
    anyhow::ensure!(
        delivered.session_id == session.session_id
            && delivered.to == recipient
            && delivered.from != recipient,
        "verification delivered to the wrong session or participant"
    );
    anyhow::ensure!(
        delivered.kind == kinds::VERIFICATION_DELIVERED,
        "expected verification.delivered"
    );
    let record: Verification = serde_json::from_value(delivered.body.clone())
        .map_err(|e| anyhow::anyhow!("invalid HACP verification body: {e}"))?;
    anyhow::ensure!(
        record.verifier == delivered.from,
        "verification author does not match envelope sender"
    );
    contract.apply_verification(&record)?;
    Ok(())
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
    a.emit(
        session_id,
        b_urn,
        kinds::SESSION_CLOSE,
        json!({"reason": reason}),
    )
    .await?;
    let env = b.read_latest().await?;
    b.receive_in_session(env, &session)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn negotiation_exhausted(session: &mut Session, a: &mut Side, b: &mut Side,
    session_id: &str, a_urn: &str, b_urn: &str, contract_id: &str, reason: &str) -> anyhow::Result<RunOutcome> {
    a.emit(session_id, b_urn, kinds::CONTRACT_NO_AGREEMENT,
        json!({"contract_id":contract_id,"reasons":[reason]})).await?;
    b.receive_in_session(b.read_latest().await?, session)?;
    close(session, a, b, session_id, a_urn, b_urn, "no agreement").await?;
    Ok(RunOutcome::NoAgreement { reason:reason.into() })
}

fn validate_terms(terms: &Value) -> anyhow::Result<()> {
    let outputs = outputs_from(terms)?;
    let acceptance = acceptance_from(terms)?;
    anyhow::ensure!(!acceptance.is_empty(), "the terms name no acceptance criteria; there would be nothing to verify");
    let names:Vec<String> = outputs.iter().map(|o| o.name.clone()).collect();
    let plan = super::verification::VerificationPlan::from_terms(terms, &names)?;
    anyhow::ensure!(plan.is_some() || !acceptance.iter().any(|a| a == "the frozen acceptance suite passes"),
        "executable acceptance criterion has no frozen verification plan");
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
        CallResult::Paused {
            session,
            reason,
            calls,
        } => {
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

struct OutputSpec {
    name: String,
    media_type: String,
    one_line: bool,
}

/// Nested files are supported; duplicate names, aliases, traversal and reserved
/// control-file names are refused before anything is launched for execution.
fn outputs_from(terms: &Value) -> anyhow::Result<Vec<OutputSpec>> {
    let outputs = terms["outputs"].as_array().filter(|a| !a.is_empty() && a.len() <= 128)
        .ok_or_else(|| anyhow::anyhow!("terms must declare between 1 and 128 output files"))?;
    let mut names = std::collections::BTreeSet::new();
    let result = outputs.iter().map(|output| {
        let name = output["name"].as_str().filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("the terms name no output file"))?;
        anyhow::ensure!(!name.contains('\\') && name.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
            && !name.contains('\0'), "output name {name:?} would write outside the workspace");
        let folded = name.to_lowercase();
        anyhow::ensure!(!matches!(folded.as_str(), "accept.json" | "delegation-terms.json" | "verdict.json") &&
            !folded.split('/').any(|c| c == ".git" || c == "__pycache__"), "output shadows a runtime control or excluded file");
        anyhow::ensure!(names.insert(folded), "duplicate output path on a case-insensitive peer");
        let one_line = match output.get("one_line") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| anyhow::anyhow!("output one_line must be a boolean"))?,
        };
        let media_type = match output.get("media_type") {
            None => "text/plain",
            Some(value) => value.as_str().filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("invalid output media type"))?,
        };
        Ok(OutputSpec { name:name.into(), media_type:media_type.into(), one_line })
    }).collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(!names.iter().any(|name| {
        let mut parent = name.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if names.contains(prefix) { return true; }
            parent = prefix;
        }
        false
    }),
        "output file conflicts with another output directory");
    Ok(result)
}

fn acceptance_from(terms: &Value) -> anyhow::Result<Vec<String>> {
    let entries = terms["acceptance"].as_array()
        .ok_or_else(|| anyhow::anyhow!("acceptance must be an array of criterion strings"))?;
    entries.iter().map(|entry| {
        let text = entry.as_str().filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("every acceptance criterion must be a nonempty string"))?;
        Ok(text.to_string())
    }).collect()
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

fn parse_checks(record: &Value) -> anyhow::Result<Vec<Check>> {
    // Never filter malformed entries: that could drop a failed acceptance gate.
    serde_json::from_value(record["checks"].clone())
        .map_err(|e| anyhow::anyhow!("invalid HACP checks: {e}"))
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
    async fn for_attempt(&self, attempt: u32) -> anyhow::Result<Self> {
        let suffix = format!("attempt-{attempt}");
        let paths = Self { root:self.root.clone(), a_out:self.a_out.clone(), b_out:self.b_out.clone(), logs:self.logs.clone(),
            sup:if attempt == 1 { self.sup.clone() } else { self.sup.join(&suffix) },
            wrk:if attempt == 1 { self.wrk.clone() } else { self.wrk.join(&suffix) } };
        tokio::fs::create_dir_all(&paths.sup).await?;
        tokio::fs::create_dir_all(&paths.wrk).await?;
        Ok(paths)
    }
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
            tokio::fs::create_dir_all(d)
                .await
                .map_err(|e| anyhow::anyhow!("cannot create run directory {}: {e}", d.display()))?;
        }
        Ok(p)
    }
}

#[cfg(test)]
#[path = "crash_tests.rs"]
mod crash_tests;

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
        TimedOut,
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
        specs: Mutex<Vec<SessionSpec>>,
    }

    impl FakeAgent {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                outcomes: Mutex::new(HashMap::new()),
                briefs: Mutex::new(Vec::new()),
                specs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SessionHost for FakeAgent {
        async fn recover(&self, spec: &SessionSpec, _started_unix: i64) -> anyhow::Result<SessionHandle> {
            anyhow::ensure!(self.outcomes.lock().await.contains_key(&spec.name),
                "uncertain launch has no host evidence");
            Ok(SessionHandle { name: spec.name.clone(), log: spec.log.clone() })
        }

        async fn launch(&self, spec: &SessionSpec) -> anyhow::Result<SessionHandle> {
            self.briefs.lock().await.push(spec.prompt.clone());
            self.specs.lock().await.push(spec.clone());
            if spec.program == "python3" {
                let output = tokio::process::Command::new(&spec.program).args(&spec.args)
                    .current_dir(&spec.cwd).stdin(std::process::Stdio::null()).output().await?;
                let mut log = output.stdout; log.extend(output.stderr);
                tokio::fs::write(&spec.log, log).await?;
                self.outcomes.lock().await.insert(spec.name.clone(), SessionOutcome::Exited { code:output.status.code().unwrap_or(-1) });
                return Ok(SessionHandle { name:spec.name.clone(), log:spec.log.clone() });
            }
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
                Step::TimedOut => SessionOutcome::TimedOut,
            };
            tokio::fs::write(&spec.log, "").await.ok();
            self.outcomes
                .lock()
                .await
                .insert(spec.name.clone(), outcome);
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

    fn durable_config(root: PathBuf) -> RunConfig {
        RunConfig { supervisor:"opencode".into(), worker:"agy".into(),
            task:"produce a one-line status report ending with the word ready".into(),
            run_dir:root, timeout_secs:60, supervisor_placement:Default::default(), worker_placement:Default::default(), max_rework:0 }
    }

    #[tokio::test]
    async fn each_role_uses_its_own_host_and_exact_model() {
        let scratch = Scratch::new("role-hosts");
        let steps = happy_path();
        let supervisor = FakeAgent::new(vec![steps[0].clone(), steps[3].clone()]);
        let worker = FakeAgent::new(vec![steps[1].clone(), steps[2].clone()]);
        let mut cfg = durable_config(scratch.join("run"));
        cfg.supervisor_placement.model = Some("supervisor/exact".into());
        cfg.worker_placement.model = Some("worker-exact".into());
        let report = run_persistent(&supervisor, &worker, &cfg, false, None).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::Settled { .. }), "{}", report.summary());
        for (host, model) in [(&supervisor, "supervisor/exact"), (&worker, "worker-exact")] {
            let specs = host.specs.lock().await;
            assert_eq!(specs.len(), 2);
            assert!(specs.iter().all(|s| s.args.windows(2).any(|a| a == ["--model", model])));
            assert!(specs.iter().all(|s| !s.prompt.contains(model)), "placement leaked into protocol content");
        }
        assert_eq!(report.worker_placement, cfg.worker_placement);
    }

    #[tokio::test]
    async fn completed_run_replays_exact_messages_without_launching_any_agent() {
        let scratch = Scratch::new("resume-complete");
        let cfg = durable_config(scratch.join("run"));
        let original = run_bilateral(&FakeAgent::new(happy_path()), &cfg).await.unwrap();
        assert!(matches!(original.outcome, RunOutcome::Settled { .. }));
        let transcript = std::fs::read(original.transcript.as_ref().unwrap()).unwrap();
        let fresh_host = FakeAgent::new(vec![]);
        let resumed = resume_bilateral(&fresh_host, &cfg.run_dir).await.unwrap();
        assert_eq!(original.outcome, resumed.outcome);
        assert_eq!(original.session_id, resumed.session_id);
        assert_eq!(original.artifact, resumed.artifact);
        assert_eq!(transcript, std::fs::read(resumed.transcript.unwrap()).unwrap());
        assert!(fresh_host.briefs.lock().await.is_empty());
        assert!(run_bilateral(&fresh_host, &cfg).await.is_err(), "run must not overwrite an existing run");
    }

    #[tokio::test]
    async fn restart_at_every_durable_boundary_retains_messages_and_never_repeats_effects() {
        let mut recovered = 0;
        let mut uncertain = 0;
        for boundary in 1..80 {
            let scratch = Scratch::new("restart-boundary");
            let cfg = durable_config(scratch.join("run"));
            let journal = super::super::journal::Journal::open(&cfg.run_dir.join("runtime.db")).unwrap();
            journal.interrupt_after(boundary);
            let host = FakeAgent::new(happy_path());
            let result = run_persistent(&host, &host, &cfg, false, Some(journal.clone())).await;
            if result.is_ok() {
                assert!(recovered >= 25, "insufficient lifecycle boundaries exercised: {recovered}");
                assert_eq!(uncertain, 4, "one intent-before-launch ambiguity per invocation");
                println!("durable restart matrix: {recovered} recovered, {uncertain} ambiguous launches refused, no repeated effects");
                return;
            }
            assert!(result.unwrap_err().to_string().contains("injected interruption"));
            let before = host.briefs.lock().await.len();
            let saved = journal.inspect().unwrap();
            let resumed = resume_bilateral(&host, &cfg.run_dir).await;
            match resumed {
                Ok(report) => {
                    assert!(matches!(report.outcome, RunOutcome::Settled { .. }), "boundary {boundary}: {}", report.summary());
                    assert_eq!(host.briefs.lock().await.len(), 4, "a completed invocation was re-executed");
                    assert!(journal.pending().unwrap().is_empty());
                    let now = journal.inspect().unwrap();
                    assert_eq!(now["outbox"], 10);
                    assert_eq!(now["receipts"], 10);
                    assert!(now["receipts"].as_u64().unwrap() >= saved["receipts"].as_u64().unwrap());
                    recovered += 1;
                }
                Err(e) => {
                    assert!(e.to_string().contains("uncertain launch has no host evidence"), "boundary {boundary}: {e}");
                    assert_eq!(host.briefs.lock().await.len(), before, "uncertain effect was blindly repeated");
                    uncertain += 1;
                }
            }
        }
        panic!("did not reach the end of the durable boundary matrix");
    }

    #[tokio::test]
    async fn replay_retains_retry_history_and_rejects_changed_completed_outputs() {
        let scratch = Scratch::new("retry-replay");
        let cfg = durable_config(scratch.join("run"));
        let mut steps = happy_path();
        steps.insert(0, write("delegation-terms.json", "```json\n{}\n```"));
        let report = run_bilateral(&FakeAgent::new(steps), &cfg).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::Settled { .. }));
        let fresh_host = FakeAgent::new(vec![]);
        let replay = resume_bilateral(&fresh_host, &cfg.run_dir).await.unwrap();
        assert_eq!(replay.calls.len(), 5);
        assert!(!replay.calls[0].produced);
        assert!(fresh_host.briefs.lock().await.is_empty());
        std::fs::write(cfg.run_dir.join("wrk/status.txt"), "changed\n").unwrap();
        let err = resume_bilateral(&fresh_host, &cfg.run_dir).await.unwrap_err();
        assert!(err.to_string().contains("output changed since its receipt"));
        assert!(fresh_host.briefs.lock().await.is_empty());
    }

    fn multi_file_path() -> Vec<Step> {
        vec![
            write("delegation-terms.json", &json!({
                "outputs":[{"name":"src/module.py","media_type":"text/x-python"},
                    {"name":"tests/test_module.py","media_type":"text/x-python"}],
                "acceptance":["the file exists","the sha256 matches the submitted manifest"]
            }).to_string()),
            write("accept.json", "{\"accepted\":true}"),
            Step::Writes(vec![("src/module.py".into(), "def value():\n    return 42\n".into()),
                ("tests/test_module.py".into(), "assert 42 == 42\n".into())]),
            verdict(json!([{"name":"sha256 recomputed","passed":true,"detail":"both file hashes measured"}])),
        ]
    }

    #[tokio::test]
    async fn real_executable_failures_force_rework_and_replay_preserves_both_attempts() {
        let scratch = Scratch::new("executable-rework");
        let mut cfg = durable_config(scratch.join("run"));
        cfg.max_rework = 2;
        let terms = json!({"outputs":[{"name":"api.py","media_type":"text/x-python"}],
            "acceptance":["the file exists","the frozen acceptance suite passes"],
            "verification":{"files":[{"path":"tests/test_api.py","content":
                "import unittest\nfrom api import value\nclass Acceptance(unittest.TestCase):\n    def test_value(self):\n        self.assertEqual(value(), 42)\n"}],
                "commands":[{"program":"python3","args":["-m","unittest","discover","-s","tests"],"timeout_secs":30}]}});
        let accepted = verdict(json!([{"name":"sha256 recomputed","passed":true,"detail":"measured"}]));
        let host = FakeAgent::new(vec![write("delegation-terms.json", &terms.to_string()),
            write("accept.json", "{\"accepted\":true}"), write("api.py", "def value():\n    return 41\n"),
            accepted.clone(), write("api.py", "def value():\n    return 42\n"), accepted]);
        let report = run_bilateral(&host, &cfg).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::Settled { .. }), "{}", report.summary());
        assert_eq!(report.executions.len(), 4, "each attempt is independently checked on both role hosts");
        assert!(report.executions[..2].iter().all(|e| e.outcome == SessionOutcome::Exited { code:1 }));
        assert!(report.executions[2..].iter().all(|e| e.outcome == SessionOutcome::Exited { code:0 }));
        assert!(std::fs::read_to_string(cfg.run_dir.join("wrk/api.py")).unwrap().contains("41"));
        assert!(std::fs::read_to_string(cfg.run_dir.join("wrk/attempt-2/api.py")).unwrap().contains("42"));
        assert!(host.briefs.lock().await.iter().any(|b| b.contains("requires rework") && b.contains("AssertionError")));
        let fresh_host = FakeAgent::new(vec![]);
        let replay = resume_bilateral(&fresh_host, &cfg.run_dir).await.unwrap();
        assert_eq!(report.executions, replay.executions);
        assert_eq!(report.artifacts, replay.artifacts);
        assert!(fresh_host.specs.lock().await.is_empty(), "replay must not rerun either models or tests");
    }

    /// Python 3.13 changed `unittest discover` to exit 5 (`NO TESTS RAN`) when a suite
    /// is empty, where Python 3.9–3.12 exited 0 and printed `OK`. The invariant under
    /// test — an acceptance suite that never exercised anything cannot settle a run —
    /// must hold for both interpreters, so the expected exit status is measured from
    /// the local `python3` instead of hardcoded, and the expected verifier complaint
    /// follows from it: exit 0 is caught by log inspection, a nonzero exit by the
    /// exit-status check itself.
    fn empty_suite_exit_code() -> i32 {
        static CODE: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
        *CODE.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("hive-empty-suite-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("tests")).unwrap();
            std::fs::write(dir.join("tests/test_api.py"), "# no test cases\n").unwrap();
            let status = std::process::Command::new("python3")
                .args(["-m", "unittest", "discover", "-s", "tests"])
                .current_dir(&dir)
                .status()
                .expect("python3 is required by the acceptance fixtures");
            let _ = std::fs::remove_dir_all(&dir);
            status.code().unwrap_or(-1)
        })
    }

    #[tokio::test]
    async fn real_empty_or_fully_skipped_suites_cannot_settle_despite_exit_zero() {
        let empty_exit = empty_suite_exit_code();
        // Exit 0 is caught by log inspection, whose reason is embedded in the brief; a
        // nonzero exit speaks for itself — the brief carries the measured log verbatim,
        // which on 3.13+ includes unittest's own `NO TESTS RAN` verdict line.
        let empty_reason = if empty_exit == 0 {
            "discovered zero tests"
        } else {
            "NO TESTS RAN"
        };
        for (fixture, expected_exit, reason) in [
            ("# Valid Python, but no discovered test cases.\n", empty_exit, empty_reason),
            ("import unittest\n@unittest.skip('unavailable')\nclass Acceptance(unittest.TestCase):\n    def test_value(self):\n        self.fail('must not run')\n", 0, "skipped every discovered test"),
        ] {
            let scratch = Scratch::new("empty-suite");
            let cfg = durable_config(scratch.join("run"));
            let terms = json!({"outputs":[{"name":"api.py","media_type":"text/x-python"}],
                "acceptance":["the file exists","the frozen acceptance suite passes"],
                "verification":{"files":[{"path":"tests/test_api.py","content":fixture}],
                    "commands":[{"program":"python3","args":["-m","unittest","discover","-s","tests"],"timeout_secs":30}]}});
            let host = FakeAgent::new(vec![write("delegation-terms.json", &terms.to_string()),
                write("accept.json", "{\"accepted\":true}"), write("api.py", "def value():\n    return 41\n"),
                verdict(json!([{"name":"sha256 recomputed","passed":true,"detail":"measured"}]))]);
            let report = run_bilateral(&host, &cfg).await.unwrap();
            assert!(matches!(report.outcome, RunOutcome::ReworkRequired { .. }), "{}", report.summary());
            assert_eq!(report.executions.len(), 2);
            assert!(report.executions.iter().all(|e| e.outcome == SessionOutcome::Exited { code: expected_exit }));
            // The summary line reports how many test runs exited 0, whatever the other
            // exit codes were, so the expected count follows from the probed interpreter.
            let expected_zero_count = if expected_exit == 0 { 2 } else { 0 };
            assert!(report.summary().contains(&format!("({expected_zero_count} exited 0")));
            assert!(!report.summary().contains("2 passed"), "a green-looking exit must not be mislabeled as acceptance");
            assert!(host.briefs.lock().await.iter().any(|b| b.contains(reason)),
                "verifier must receive the measured failure, not just a green exit code");
            let fresh = FakeAgent::new(vec![]);
            let replay = resume_bilateral(&fresh, &cfg.run_dir).await.unwrap();
            assert_eq!(report.outcome, replay.outcome);
            assert_eq!(report.executions, replay.executions);
            assert!(fresh.specs.lock().await.is_empty(), "replay reran an execution");
        }
    }

    #[tokio::test]
    async fn counteroffers_and_clarification_freeze_only_explicitly_accepted_terms() {
        for clarification in [false, true] {
            let scratch = Scratch::new("negotiation");
            let cfg = durable_config(scratch.join("run"));
            let revised:Value = serde_json::from_str(&terms("answer.txt")).unwrap();
            let request = if clarification { json!({"accepted":false,"question":"Which output filename is required?"}) }
                else { json!({"accepted":false,"counter_terms":revised,"reasons":["use the agreed filename"]}) };
            let response = if clarification { json!({"accepted":false,"terms":revised,"answer":"Use answer.txt."}) }
                else { json!({"accepted":true}) };
            let host = FakeAgent::new(vec![write("delegation-terms.json", &terms("status.txt")),
                write("accept.json", &request.to_string()), write("counter-1.json", &response.to_string()),
                write("accept.json", "{\"accepted\":true}"), write("answer.txt", CONTENT), verdict(good_checks())]);
            let report = run_bilateral(&host, &cfg).await.unwrap();
            assert!(matches!(report.outcome, RunOutcome::Settled { .. }), "{}", report.summary());
            let digest = canon::digest_of(&json!({"contract_id":report.contract_id,"revision":1,"content":revised})).unwrap();
            assert_eq!(report.artifact.as_ref().unwrap().contract_revision, digest);
            assert_eq!(report.calls.len(), 6);
            let replay_host = FakeAgent::new(vec![]);
            let replay = resume_bilateral(&replay_host, &cfg.run_dir).await.unwrap();
            assert_eq!(replay.artifacts, report.artifacts);
            assert!(replay_host.specs.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn negotiation_limit_is_no_agreement_without_executing_work() {
        let scratch = Scratch::new("negotiation-bound");
        let request = write("accept.json", "{\"accepted\":false,\"question\":\"clarify the scope\"}");
        let host = FakeAgent::new(vec![write("delegation-terms.json", &terms("status.txt")), request.clone(),
            write("counter-1.json", "{\"accepted\":true}"), request.clone(),
            write("counter-2.json", "{\"accepted\":true}"), request]);
        let report = run_bilateral(&host, &durable_config(scratch.join("run"))).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::NoAgreement { .. }));
        assert_eq!(report.calls.len(), 6);
        assert!(report.calls.iter().all(|c| !c.stage.contains("work")));
        assert!(report.artifacts.is_empty());
    }

    #[tokio::test]
    async fn multiple_nested_outputs_bind_every_artifact_and_replay_without_launching() {
        let scratch = Scratch::new("multi-output");
        let cfg = durable_config(scratch.join("run"));
        let report = run_bilateral(&FakeAgent::new(multi_file_path()), &cfg).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::Settled { .. }), "{}", report.summary());
        assert_eq!(report.artifacts.len(), 2);
        let transcript = std::fs::read_to_string(report.transcript.as_ref().unwrap()).unwrap();
        for line in transcript.lines() {
            let frame:Value = serde_json::from_str(line).unwrap();
            if [kinds::SUBMISSION_DELIVERED,kinds::VERIFICATION_DELIVERED].contains(&frame["envelope"]["kind"].as_str().unwrap()) {
                assert_eq!(frame["envelope"]["body"]["artifacts"].as_array().unwrap().len(), 2);
            }
        }
        let replay_host = FakeAgent::new(vec![]);
        let replay = resume_bilateral(&replay_host, &cfg.run_dir).await.unwrap();
        assert_eq!(report.artifacts, replay.artifacts);
        assert!(replay_host.briefs.lock().await.is_empty());
        std::fs::write(cfg.run_dir.join("wrk/tests/test_module.py"), "changed secondary output").unwrap();
        assert!(resume_bilateral(&replay_host, &cfg.run_dir).await.unwrap_err().to_string().contains("output changed"));
    }

    #[tokio::test]
    async fn missing_secondary_output_is_not_mistaken_for_completed_work() {
        let scratch = Scratch::new("missing-secondary");
        let mut steps = multi_file_path();
        steps[2] = write("src/module.py", "print(42)\n");
        steps[3] = Step::Silent;
        let report = run_bilateral(&FakeAgent::new(steps), &durable_config(scratch.join("run"))).await.unwrap();
        assert!(matches!(report.outcome, RunOutcome::Failed { stage:Stage::Execute, .. }));
        assert!(!report.calls.last().unwrap().produced);
    }

    #[test]
    fn conflicting_portable_paths_and_reserved_files_cannot_form_a_contract() {
        for names in [vec!["a.py", "A.py"], vec!["src", "src/a.py"], vec!["src", "src-extra", "src/a.py"], vec!["accept.json"], vec![".git/config"], vec!["a/../b"]] {
            let outputs:Vec<Value> = names.into_iter().map(|name| json!({"name":name})).collect();
            assert!(outputs_from(&json!({"outputs":outputs})).is_err());
        }
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
                supervisor_placement:Default::default(),
                worker_placement:Default::default(),
                max_rework:0,
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
            RunOutcome::Settled {
                verdict: "accept".into()
            },
            "{}",
            r.summary()
        );
        assert_eq!(
            r.frames,
            10,
            "the exchange is a fixed shape: {}",
            r.summary()
        );
        assert_eq!(r.calls.len(), 4, "one call per agent step, no retries");
        assert!(r.calls.iter().all(|c| c.produced));
        assert!(
            r.still_running.is_empty(),
            "a settled run leaves nothing behind"
        );

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
        let work: Vec<_> = r
            .calls
            .iter()
            .filter(|c| c.stage.starts_with("03"))
            .collect();
        assert_eq!(work.len(), 2, "the retry is recorded, not hidden");
        assert!(work.iter().all(|c| !c.produced));
    }

    #[tokio::test]
    async fn a_retry_recovers_an_agent_that_missed_the_first_time() {
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps.insert(2, Step::Silent); // first work attempt writes nothing
        let r = run(steps, &s).await;
        assert_eq!(
            r.outcome,
            RunOutcome::Settled {
                verdict: "accept".into()
            }
        );
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
            (
                "../wrk/status.txt".into(),
                "something else entirely\n".into(),
            ),
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
            RunOutcome::Paused {
                session,
                reason,
                attach,
            } => {
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
    async fn an_output_path_that_escapes_the_workspace_is_refused() {
        // The output name is chosen by an agent and becomes a filesystem path.
        let s = Scratch::new("lifecycle");
        let mut steps = happy_path();
        steps[0] = write(
            "delegation-terms.json",
            &terms("../../.ssh/authorized_keys"),
        );
        let r = run(steps, &s).await;
        match &r.outcome {
            RunOutcome::Failed { stage, reason } => {
                assert_eq!(*stage, Stage::Authoring);
                assert!(reason.contains("outside the workspace"), "{reason}");
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
    async fn malformed_control_json_gets_one_audited_retry() {
        for malformed in ["```json\n{}\n```", "{} trailing data", "[]"] {
            let s = Scratch::new("json-retry");
            let mut steps = happy_path();
            steps.insert(0, write("delegation-terms.json", malformed));
            let r = run(steps, &s).await;
            assert!(matches!(r.outcome, RunOutcome::Settled { .. }), "{}", r.summary());
            assert_eq!(r.calls.len(), 5);
            assert!(!r.calls[0].produced);
            assert!(r.calls[1].produced);
            assert_eq!(tokio::fs::read_to_string(s.join("run/logs/01-author-invalid-output.txt")).await.unwrap(), malformed);
        }
    }

    #[tokio::test]
    async fn a_timeout_requires_inspection_and_never_starts_a_second_agent() {
        let s = Scratch::new("timeout");
        let mut steps = happy_path();
        steps[2] = Step::TimedOut;
        let r = run(steps, &s).await;
        assert!(matches!(r.outcome, RunOutcome::Paused { .. }), "{}", r.summary());
        assert_eq!(r.calls.len(), 3);
        assert_eq!(r.still_running.len(), 1);
    }

    #[tokio::test]
    async fn frozen_requirements_cannot_be_omitted_or_replaced_by_existence() {
        for content in ["unfinished\n", "ready\nextra", "ready\nextra\n"] {
            let s = Scratch::new("frozen-checks");
            let mut steps = happy_path();
            let mut t: Value = serde_json::from_str(&terms("status.txt")).unwrap();
            t["acceptance"] = json!(["The file has exactly one line and contains the word ready"]);
            steps[0] = write("delegation-terms.json", &t.to_string());
            steps[2] = write("status.txt", content);
            steps[3] = verdict(json!([{"name":"file exists", "passed":true,"detail":"exists"}]));
            let r = run(steps, &s).await;
            assert!(matches!(r.outcome, RunOutcome::Failed { stage: Stage::Verification, .. }), "{}", r.summary());
        }
    }

    #[test]
    fn malformed_frozen_requirements_are_rejected_not_silently_dropped() {
        for acceptance in [json!(["the file exists", 42]), json!([""]), json!(null)] {
            assert!(acceptance_from(&json!({"acceptance": acceptance})).is_err());
        }
        let mut t: Value = serde_json::from_str(&terms("status.txt")).unwrap();
        t["outputs"][0]["one_line"] = json!("true");
        assert!(outputs_from(&t).is_err());
    }

    #[tokio::test]
    async fn a_rejecting_verdict_is_terminal_but_not_successful() {
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
        assert_eq!(
            r.outcome,
            RunOutcome::Rejected {
                reasons: vec!["the file does not end with the required word".into()],
            }
        );
        assert_eq!(r.outcome.exit_code(), 1);
    }

    #[tokio::test]
    async fn verification_on_the_wire_is_a_complete_hacp_library_record() {
        let s = Scratch::new("library-verification");
        let r = run(happy_path(), &s).await;
        let transcript = std::fs::read_to_string(r.transcript.as_ref().unwrap()).unwrap();
        let frame: Value = transcript
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|frame| frame["envelope"]["kind"] == kinds::VERIFICATION_DELIVERED)
            .unwrap();
        let record: Verification =
            serde_json::from_value(frame["envelope"]["body"].clone()).unwrap();
        record.validate().unwrap();
        let artifact = r.artifact.as_ref().unwrap();
        assert_eq!(record.against_revision, artifact.contract_revision);
        assert_eq!(record.contract_id, r.contract_id);
        assert_eq!(record.artifacts, vec![artifact.artifact_id.clone()]);
        assert_eq!(record.verifier, frame["envelope"]["from"].as_str().unwrap());
    }

    #[tokio::test]
    async fn library_rework_preserves_executing_state_without_closing_or_settling() {
        let s = Scratch::new("library-rework");
        let mut steps = happy_path();
        steps[3] = write("verdict.json", &json!({
            "verdict": "rework", "checks": [{"name": "content", "passed": false, "detail": "revise it"}],
            "reasons": ["rewrite the status line"],
        }).to_string());
        let r = run(steps, &s).await;
        let state_path = match &r.outcome {
            RunOutcome::ReworkRequired { scope, state_path } => {
                assert_eq!(scope, "rewrite the status line");
                state_path
            }
            other => panic!("rework must not settle: {other:?}"),
        };
        assert_eq!(r.outcome.exit_code(), 2);
        let snapshot: Value = serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
        let contract: Contract = serde_json::from_value(snapshot["contract"].clone()).unwrap();
        let session: Session = serde_json::from_value(snapshot["session"].clone()).unwrap();
        assert_eq!(contract.state, ContractState::Executing);
        assert_eq!(session.state, hacp::v2::SessionState::Active);
        assert_eq!(
            contract.rework_scope.as_deref(),
            Some("rewrite the status line")
        );
        let transcript = std::fs::read_to_string(r.transcript.unwrap()).unwrap();
        assert!(!transcript.contains(kinds::SESSION_CLOSE));
        assert_eq!(r.calls.len(), 4, "no implicit extra model spending");
    }

    #[tokio::test]
    async fn malformed_checks_cannot_disappear_before_library_validation() {
        let s = Scratch::new("library-checks");
        let mut steps = happy_path();
        let mut checks = good_checks();
        checks
            .as_array_mut()
            .unwrap()
            .push(json!({"passed": false, "detail": "missing name"}));
        steps[3] = verdict(checks);
        let r = run(steps, &s).await;
        assert!(matches!(
            r.outcome,
            RunOutcome::Failed {
                stage: Stage::Verification,
                ..
            }
        ));
        assert!(!std::fs::read_to_string(r.transcript.unwrap())
            .unwrap()
            .contains(kinds::VERIFICATION_DELIVERED));
    }

    #[tokio::test]
    async fn hive_uses_the_library_to_refuse_accept_with_a_failed_gate() {
        let s = Scratch::new("library-failed-gate");
        let mut steps = happy_path();
        let mut checks = good_checks();
        checks
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "acceptance", "passed": false, "detail": "not met"}));
        steps[3] = verdict(checks);
        let r = run(steps, &s).await;
        match r.outcome {
            RunOutcome::Failed {
                stage: Stage::Verification,
                reason,
            } => assert!(reason.contains("failed checks"), "{reason}"),
            other => panic!("failed gate accepted: {other:?}"),
        }
    }

    fn pending_verification() -> (Session, Contract, Envelope) {
        let a = "urn:hacp:agent:supervisor";
        let b = "urn:hacp:agent:worker";
        let mut session = Session::open("s-test", a, b).unwrap();
        session.accept(b).unwrap();
        let mut contract = Contract::propose(
            &session,
            "c-test",
            Task {
                task_id: "t-test".into(),
                summary: "write status".into(),
                owner: b.into(),
            },
            Relationship::Collaboration,
            vec![],
            LIMITS,
        )
        .unwrap();
        let terms = json!({"output": "status.txt"});
        contract.agree(a, &terms).unwrap();
        contract.agree(b, &terms).unwrap();
        let revision = contract.freeze(terms).unwrap();
        let artifacts = vec![format!("urn:hacp:artifact:{}", uuid::Uuid::new_v4())];
        contract
            .submit(
                b,
                Submission {
                    against_revision: revision.clone(),
                    artifacts: artifacts.clone(),
                    evidence: vec![],
                    claim: "done".into(),
                },
            )
            .unwrap();
        let record = Verification::decide(
            "v-000000000001",
            a,
            "c-test",
            &revision,
            artifacts,
            vec![],
            vec![Check {
                name: "content".into(),
                passed: true,
                detail: "measured".into(),
            }],
            Verdict::Accept,
            vec![],
            vec![],
        )
        .unwrap();
        let env = Envelope::new(
            "s-test",
            a,
            b,
            kinds::VERIFICATION_DELIVERED,
            serde_json::to_value(record).unwrap(),
        );
        (session, contract, env)
    }

    #[test]
    fn received_verifications_are_bound_by_the_library_before_transition() {
        let (session, contract, envelope) = pending_verification();
        for field in [
            "against_revision",
            "contract_id",
            "verifier",
            "artifacts",
            "verification_id",
        ] {
            let mut env = envelope.clone();
            env.body[field] = if field == "artifacts" {
                json!(["wrong-artifact"])
            } else {
                json!("wrong")
            };
            let mut c = contract.clone();
            assert!(
                apply_delivered_verification(&mut c, &session, &env, &envelope.to).is_err(),
                "{field}"
            );
            assert_eq!(c, contract);
        }
        let mut c = contract;
        apply_delivered_verification(&mut c, &session, &envelope, &envelope.to).unwrap();
        assert_eq!(c.state, ContractState::Settled);
    }

    #[test]
    fn verification_adapter_checks_envelope_context_and_record_authorship() {
        let (session, contract, envelope) = pending_verification();
        let mut wrong_author = envelope.clone();
        wrong_author.from = envelope.to.clone();
        wrong_author.to = envelope.from.clone();
        let mut wrong_session = envelope.clone();
        wrong_session.session_id = "s-other".into();
        let mut wrong_kind = envelope.clone();
        wrong_kind.kind = kinds::HEARTBEAT.into();
        for env in [wrong_author, wrong_session, wrong_kind] {
            let mut c = contract.clone();
            assert!(apply_delivered_verification(&mut c, &session, &env, &env.to).is_err());
            assert_eq!(c, contract);
        }
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
