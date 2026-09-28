//! Feedback-driven chat with the master agent and approvals for flagged actions.
//!
//! The flow is deliberately two-legged. `POST /api/chat` plans and runs
//! everything the watchdog is happy with, then stops and reports anything it
//! flagged. `POST /api/chat/{run_id}/approve` resumes that same plan with the
//! user's decisions. The plan is held server-side between the two calls so the
//! browser cannot hand back a *different* command than the one it was shown.

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hive_core::agent::planner::PlanPhase;
use hive_core::agent::run::StepStatus;
use hive_core::agent::run::{Approvals, PlannedRun, RunResult};
use hive_core::agent::workflow::WorkflowState;
use hive_core::agent::MasterAgent;
use hive_core::memory::machines;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const PLANNING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(150);
const CONTINUATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Bounds how many planner calls run at once. The planner hits a single
/// upstream model endpoint; two concurrent plans is the most it serves
/// without timing out.
static PLANNER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

// Bound the whole planning phase, including sequential model and memory calls.
// This future never executes commands, so dropping it on timeout is safe.
// A permit is held for the whole call, bounding concurrent planner calls.
async fn bounded_plan<T>(
    plan: impl std::future::Future<Output = anyhow::Result<T>>,
    deadline: std::time::Duration,
) -> Result<T, Response> {
    let _permit = PLANNER_SLOTS
        .acquire()
        .await
        .expect("planner semaphore is never closed");
    match tokio::time::timeout(deadline, plan).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => {
            warn!(error = %e, "planning failed");
            Err((StatusCode::BAD_GATEWAY, format!("planning failed: {e}")).into_response())
        }
        Err(_) => {
            warn!("total planning deadline exceeded");
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                "Planning timed out. No commands were executed from this planning round. Earlier results, if any, remain saved. Please resend your message to try again.",
            )
                .into_response())
        }
    }
}

/// One automatic retry of the whole plan when its deadline fires: a transient
/// provider stall should not lose the user's message. A deadline failure on
/// the retry as well becomes a user-facing explanation instead of a bare
/// timeout.
pub(crate) async fn plan_with_retry<T, F, Fut>(
    make_plan: F,
    deadline: std::time::Duration,
) -> Result<T, Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match bounded_plan(make_plan(), deadline).await {
        Ok(plan) => Ok(plan),
        Err(response) if response.status() != StatusCode::GATEWAY_TIMEOUT => Err(response),
        Err(_) => {
            warn!("planning deadline exceeded; retrying the whole plan once");
            match bounded_plan(make_plan(), deadline).await {
                Ok(plan) => Ok(plan),
                Err(response) if response.status() == StatusCode::GATEWAY_TIMEOUT => Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "Planning timed out and one automatic retry also timed out. No commands were executed from this planning round. Earlier results, if any, remain saved. Please resend your message in a moment, and if it keeps timing out, try a shorter or simpler request.",
                )
                    .into_response()),
                Err(response) => Err(response),
            }
        }
    }
}

use hive_core::memory::chats::{ChatError, ChatStore, SavedTurn, StartTurn};

#[derive(Clone)]
pub struct AgentHandle {
    pub agent: Option<Arc<MasterAgent>>,
    pub history: Option<ChatStore>,
    pub master_name: String,
}

impl AgentHandle {
    pub fn disabled() -> Self {
        Self {
            agent: None,
            history: None,
            master_name: "master".into(),
        }
    }

    pub fn enabled(agent: Arc<MasterAgent>, master_name: String) -> anyhow::Result<Self> {
        let history = ChatStore::new(agent.memory.graph.shared_conn())?;
        history.recover_interrupted()?;
        Ok(Self {
            agent: Some(agent),
            history: Some(history),
            master_name,
        })
    }

    pub(crate) fn require(&self) -> Result<&Arc<MasterAgent>, Response> {
        self.agent.as_ref().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "No agent on this host — this instance serves terminals only.",
            )
                .into_response()
        })
    }
    fn store(&self) -> Result<&ChatStore, Response> {
        self.history.as_ref().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Chat storage is unavailable on this host.",
            )
                .into_response()
        })
    }
}

fn storage_error(e: anyhow::Error) -> Response {
    let status = match e.downcast_ref::<ChatError>() {
        Some(ChatError::NotFound) => StatusCode::NOT_FOUND,
        Some(ChatError::Busy | ChatError::RequestConflict) => StatusCode::CONFLICT,
        None => StatusCode::INTERNAL_SERVER_ERROR,
    };
    warn!(error = %e, "chat storage request failed");
    (status, e.to_string()).into_response()
}

#[derive(Default, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub offset: usize,
}

pub async fn list_chats(
    State(h): State<AgentHandle>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let store = match h.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.list(&query.q, query.offset.min(1_000_000)) {
        Ok(chats) => ([("cache-control", "no-store")], Json(chats)).into_response(),
        Err(e) => storage_error(e),
    }
}

#[derive(Default, Deserialize)]
pub struct CreateChat {
    pub project_id: Option<String>,
}

pub async fn create_chat(State(h): State<AgentHandle>, Json(req): Json<CreateChat>) -> Response {
    let store = match h.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.create(req.project_id.as_deref()) {
        Ok(chat) => (StatusCode::CREATED, Json(chat)).into_response(),
        Err(e) => storage_error(e),
    }
}

pub async fn get_chat(State(h): State<AgentHandle>, Path(id): Path<String>) -> Response {
    let store = match h.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.get(&id) {
        Ok(Some(chat)) => match store.messages(&id) {
            Ok(messages) => (
                [("cache-control", "no-store")],
                Json(serde_json::json!({"chat":chat,"messages":messages})),
            )
                .into_response(),
            Err(e) => storage_error(e),
        },
        Ok(None) => storage_error(ChatError::NotFound.into()),
        Err(e) => storage_error(e),
    }
}

#[derive(Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub background: bool,
    pub message: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ChatReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowState>,
    pub conversation_id: String,
    pub run: PlannedRun,
    pub result: RunResult,
}

fn reply_text(reply: &ChatReply) -> String {
    let mut text = reply.result.summary.clone();
    for outcome in &reply.result.outcomes {
        text.push_str(&format!("\n\n$ {}\n{}", outcome.command, outcome.output));
    }
    text
}

fn save_reply(store: &ChatStore, turn: &SavedTurn, reply: &ChatReply) -> anyhow::Result<()> {
    let status = match reply.workflow.as_ref().map(|w| w.status.as_str()) {
        Some("running") => "executing",
        Some("blocked") => "failed",
        Some("awaiting_approval") => "awaiting_approval",
        Some("complete") => "completed",
        _ if reply.result.is_complete() => "completed",
        _ => "awaiting_approval",
    };
    store.finish(
        &turn.id,
        status,
        &reply_text(reply),
        Some(&serde_json::to_value(reply)?),
    )
}

async fn save_failure(store: &ChatStore, turn: &SavedTurn, response: Response) -> Response {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&body).to_string();
    match store.finish(&turn.id, "failed", &text, None) {
        Ok(()) => (status, text).into_response(),
        Err(e) => storage_error(e),
    }
}

pub async fn chat(State(h): State<AgentHandle>, Json(req): Json<ChatRequest>) -> Response {
    let background = req.background;
    if let Err(r) = h.require() {
        return r;
    }
    let store = match h.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    if req.message.trim().is_empty() || req.message.len() > 65536 {
        return (
            StatusCode::BAD_REQUEST,
            "Message must contain 1–65536 bytes.",
        )
            .into_response();
    }
    let id = req
        .request_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if uuid::Uuid::parse_str(&id).is_err() {
        return (StatusCode::BAD_REQUEST, "Invalid request ID").into_response();
    }
    let conversation_id = match req.conversation_id {
        Some(id) => id,
        None => match store.turn(&id) {
            Ok(Some(turn)) => turn.conversation_id,
            Ok(None) => match store.create(req.project_id.as_deref()) {
                Ok(chat) => chat.id,
                Err(e) => return storage_error(e),
            },
            Err(e) => return storage_error(e),
        },
    };
    let turn = match store.begin(&conversation_id, &id, &req.message) {
        Ok(StartTurn::New(turn)) => turn,
        Ok(StartTurn::Existing(turn)) => {
            if background && matches!(turn.status.as_str(), "planning" | "executing") {
                return (StatusCode::ACCEPTED, Json(serde_json::json!({"conversation_id": turn.conversation_id, "request_id": turn.id, "status": turn.status}))).into_response();
            }
            return match turn.reply {
                Some(reply)
                    if matches!(turn.status.as_str(), "completed" | "awaiting_approval") =>
                {
                    Json(reply).into_response()
                }
                _ => (
                    StatusCode::CONFLICT,
                    "This request is already saved. Open the chat to see its status.",
                )
                    .into_response(),
            };
        }
        Err(e) => return storage_error(e),
    };
    // Browser requests return a durable receipt immediately. Progress and the
    // eventual reply are read from the saved chat, surviving disconnects.
    if background {
        let receipt = serde_json::json!({"conversation_id": turn.conversation_id, "request_id": turn.id, "status": "planning"});
        tokio::spawn(async move {
            process_chat(h, turn).await;
        });
        return (StatusCode::ACCEPTED, Json(receipt)).into_response();
    }
    // Synchronous API callers retain the existing response shape.
    match tokio::spawn(async move { process_chat(h, turn).await }).await {
        Ok(response) => response,
        Err(e) => storage_error(e.into()),
    }
}

async fn process_chat(h: AgentHandle, turn: SavedTurn) -> Response {
    if hive_core::delegation::enabled() {
        return crate::delegation::process(h, turn).await;
    }
    let agent = h.agent.as_ref().unwrap();
    let store = h.history.as_ref().unwrap();
    let history = match store.context(&turn) {
        Ok(history) => history,
        Err(e) => return save_failure(store, &turn, storage_error(e)).await,
    };
    let plan = match plan_with_retry(
        || agent.plan_chat_run(&turn.user_input, history.clone()),
        PLANNING_TIMEOUT,
    )
    .await
    {
        Ok(plan) => plan,
        Err(response) => return save_failure(store, &turn, response).await,
    };
    if let Err(e) = store.executing(&turn.id, &plan.id) {
        return save_failure(store, &turn, storage_error(e)).await;
    }
    info!(run = %plan.id, conversation = %turn.conversation_id, "executing saved chat plan");
    let result = RunResult {
        run_id: plan.id.clone(),
        summary: plan.summary.clone(),
        complexity: plan.complexity,
        provider: plan.provider,
        outcomes: vec![],
        sessions: vec![],
        awaiting_approval: vec![],
    };
    let reply = ChatReply {
        workflow: Some(WorkflowState::default()),
        conversation_id: turn.conversation_id.clone(),
        run: plan,
        result,
    };
    drive_workflow(h, turn, reply, Approvals::none()).await
}

/// Execute/observe/replan, checkpointing before and after every command. This
/// state is also the approval continuation; completed effects are never replayed.
fn repeats_completed_round(reply: &ChatReply, next: &PlannedRun) -> bool {
    let state = reply.workflow.as_ref().unwrap();
    let previous = &reply.run.steps[state.round_start..];
    !previous.is_empty()
        && next.phase == reply.run.phase
        && next.steps.len() == previous.len()
        && previous.iter().zip(&next.steps).all(|(a, b)| {
            a.target == b.target
                && a.command == b.command
                && reply
                    .result
                    .outcomes
                    .iter()
                    .any(|o| o.id == a.id && o.status == StepStatus::Executed)
        })
}

async fn drive_workflow(
    h: AgentHandle,
    turn: SavedTurn,
    mut reply: ChatReply,
    mut approvals: Approvals,
) -> Response {
    let agent = h.agent.as_ref().unwrap();
    let store = h.history.as_ref().unwrap();
    'workflow: loop {
        let state = reply.workflow.as_ref().unwrap().clone();
        if state.round > 24 {
            reply.workflow.as_mut().unwrap().status = "blocked".into();
            reply.result.summary = "Stopped after 24 work rounds without confirmed completion. The commands and results below are saved; the task is not complete.".into();
            break;
        }
        if reply.run.phase == PlanPhase::Blocked {
            reply.workflow.as_mut().unwrap().status = "blocked".into();
            break;
        }
        if reply.run.phase == PlanPhase::Complete {
            if state.can_finish(&reply.result) {
                reply.workflow.as_mut().unwrap().status = "complete".into();
                break;
            }
            // The model cannot promote a setup attempt into verified completion.
            reply.result.summary = "Completion was proposed without successful functional verification. Continuing to check the result.".into();
        } else {
            reply.workflow.as_mut().unwrap().status = "running".into();
            if let Err(e) = save_reply(store, &turn, &reply) {
                return storage_error(e);
            }
            let round_steps = reply.run.steps[state.round_start..].to_vec();
            for step in &round_steps {
                if reply.result.outcomes.iter().any(|o| {
                    o.id == step.id
                        && !matches!(o.status, StepStatus::AwaitingApproval | StepStatus::Pending)
                }) {
                    continue;
                }
                let mut single = reply.run.clone();
                single.steps = vec![step.clone()];
                single.conversation_id = None;
                // Persist the pending action before dispatch. A crash is marked
                // interrupted at startup, never automatically retried.
                if let Err(e) = save_reply(store, &turn, &reply) {
                    return storage_error(e);
                }
                let result = if !reply.run.targets.is_empty()
                    && !reply.run.targets.contains(&step.target)
                {
                    RunResult { run_id: single.id.clone(), summary: "Destination rejected".into(), complexity: single.complexity, provider: single.provider,
                        outcomes: vec![hive_core::agent::run::StepOutcome { id: step.id, command: step.command.clone(), status: StepStatus::Failed,
                            output: format!("NOT EXECUTED: destination {:?} is outside the task's fixed destinations {:?}. Use the named destination for that machine's work.", step.target, reply.run.targets) }],
                        sessions: vec![], awaiting_approval: vec![] }
                } else {
                    agent.execute_run(&single, &approvals).await
                };
                reply.result.outcomes.retain(|o| o.id != step.id);
                reply.result.outcomes.extend(result.outcomes);
                reply.result.sessions.extend(result.sessions);
                reply.result.awaiting_approval = result.awaiting_approval;
                if let Err(e) = save_reply(store, &turn, &reply) {
                    return storage_error(e);
                }
                if reply.result.outcomes.last().is_some_and(|o| {
                    !matches!(o.status, StepStatus::Executed | StepStatus::Skipped)
                }) {
                    for later in round_steps.iter().filter(|s| s.id > step.id) {
                        if !reply.result.outcomes.iter().any(|o| o.id == later.id) {
                            reply
                                .result
                                .outcomes
                                .push(hive_core::agent::run::StepOutcome {
                                    id: later.id,
                                    command: later.command.clone(),
                                    status: StepStatus::Pending,
                                    output: "Waiting for the preceding step.".into(),
                                });
                        }
                    }
                    break;
                }
            }
            reply.result.outcomes.sort_by_key(|o| o.id);
            if !reply.result.awaiting_approval.is_empty() {
                reply.workflow.as_mut().unwrap().status = "awaiting_approval".into();
                break;
            }
            if reply
                .result
                .outcomes
                .iter()
                .skip(state.round_start)
                .any(|o| matches!(o.status, StepStatus::Delegated | StepStatus::Denied))
            {
                reply.workflow.as_mut().unwrap().status = "blocked".into();
                reply.result.summary = "Work stopped for review. A denied action or an unconfirmed remote result prevents automatic continuation. See the recorded results below.".into();
                break;
            }
            let mut round_result = reply.result.clone();
            round_result.outcomes.retain(|o| o.id >= state.round_start);
            reply
                .workflow
                .as_mut()
                .unwrap()
                .observe(reply.run.phase, &round_result, &reply.run);
        }
        if let Err(e) = save_reply(store, &turn, &reply) {
            return storage_error(e);
        }
        let mut planning_attempts = 0;
        let next = loop {
            let proposed = bounded_plan(
                agent.continue_run(&reply.run, &reply.result, reply.workflow.as_ref().unwrap()),
                CONTINUATION_TIMEOUT,
            )
            .await.and_then(|next| {
                if repeats_completed_round(&reply, &next) {
                    Err((StatusCode::UNPROCESSABLE_ENTITY, "No progress: this entire round already succeeded with the same commands on the same machines. Nothing was executed again. Use the existing output to implement the missing behavior with a file write or choose a different necessary check. Do not list the same directories again.").into_response())
                } else { Ok(next) }
            });
            match proposed {
                Ok(next) => break next,
                Err(response) => {
                    let bytes = axum::body::to_bytes(response.into_body(), 65536)
                        .await
                        .unwrap_or_default();
                    planning_attempts += 1;
                    let error = String::from_utf8_lossy(&bytes).to_string();
                    reply.workflow.as_mut().unwrap().planning_error = Some(error.clone());
                    if planning_attempts < 2 {
                        reply.result.summary = format!("The next planning attempt failed: {error} Retrying once with a smaller action; completed work remains saved.");
                        if let Err(e) = save_reply(store, &turn, &reply) {
                            return storage_error(e);
                        }
                        continue;
                    }
                    reply.workflow.as_mut().unwrap().status = "blocked".into();
                    reply.result.summary = format!("Work is saved but not complete: {error}");
                    break 'workflow;
                }
            }
        };
        let offset = reply.run.steps.len();
        let state = reply.workflow.as_mut().unwrap();
        state.planning_error = None;
        state.round += 1;
        state.round_start = offset;
        reply.run.phase = next.phase;
        reply.run.model = next.model;
        reply.run.provider = next.provider;
        reply
            .run
            .steps
            .extend(next.steps.into_iter().map(|mut step| {
                step.id += offset;
                step
            }));
        reply.result.summary = next.summary;
        reply.result.provider = next.provider;
        // Approval applies to reviewed actions only, never a newly generated step.
        approvals = Approvals::none();
    }
    match save_reply(store, &turn, &reply) {
        Ok(()) => Json(reply).into_response(),
        Err(e) => storage_error(e),
    }
}

#[derive(Deserialize)]
pub struct ApprovalRequest {
    #[serde(default)]
    pub approved: Vec<usize>,
    #[serde(default)]
    pub denied: Vec<usize>,
}

pub async fn approve(
    State(h): State<AgentHandle>,
    Path(run_id): Path<String>,
    Json(req): Json<ApprovalRequest>,
) -> Response {
    if let Err(r) = h.require() {
        return r;
    }
    let store = match h.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let turn = match store.turn_for_run(&run_id) {
        Ok(Some(turn)) => turn,
        Ok(None) => return (StatusCode::NOT_FOUND, "No such saved run.").into_response(),
        Err(e) => return storage_error(e),
    };
    let reply: ChatReply = match turn
        .reply
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
    {
        Some(reply) => reply,
        None => {
            return (
                StatusCode::CONFLICT,
                "This run cannot be resumed. Open the chat to see its status.",
            )
                .into_response()
        }
    };
    if req.approved.is_empty() && req.denied.is_empty()
        || req.approved.iter().any(|id| req.denied.contains(id))
        || req
            .approved
            .iter()
            .chain(&req.denied)
            .any(|id| !reply.result.awaiting_approval.contains(id))
    {
        return (
            StatusCode::BAD_REQUEST,
            "Choose only steps currently awaiting approval.",
        )
            .into_response();
    }
    match store.claim_approval(&turn.id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                "This run is already processing or finished. Refresh the chat.",
            )
                .into_response()
        }
        Err(e) => return storage_error(e),
    }
    match tokio::spawn(async move { process_approval(h, turn, reply, req).await }).await {
        Ok(response) => response,
        Err(e) => storage_error(e.into()),
    }
}

async fn process_approval(
    h: AgentHandle,
    turn: SavedTurn,
    mut reply: ChatReply,
    req: ApprovalRequest,
) -> Response {
    let mut approvals = Approvals::none();
    for id in req.approved {
        approvals.approve(id);
    }
    for id in req.denied {
        approvals.deny(id);
    }
    if reply.workflow.is_some() {
        return drive_workflow(h, turn, reply, approvals).await;
    }
    let mut remaining = reply.run.clone();
    remaining.conversation_id = None;
    remaining
        .steps
        .retain(|s| reply.result.awaiting_approval.contains(&s.id));
    let result = h
        .agent
        .as_ref()
        .unwrap()
        .execute_run(&remaining, &approvals)
        .await;
    for outcome in result.outcomes {
        if let Some(old) = reply
            .result
            .outcomes
            .iter_mut()
            .find(|o| o.id == outcome.id)
        {
            *old = outcome;
        }
    }
    reply.result.sessions.extend(result.sessions);
    reply.result.awaiting_approval = result.awaiting_approval;
    match save_reply(h.history.as_ref().unwrap(), &turn, &reply) {
        Ok(()) => Json(reply).into_response(),
        Err(e) => storage_error(e),
    }
}

/// The machine knowledge graph, for the UI's fleet view.
pub async fn machine_graph(State(h): State<AgentHandle>) -> Response {
    let agent = match h.require() {
        Ok(a) => a,
        Err(r) => return r,
    };
    match agent.memory.graph.snapshot() {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Re-probe every machine and rewrite the graph.
pub async fn refresh_machines(State(h): State<AgentHandle>) -> Response {
    let agent = match h.require() {
        Ok(a) => a,
        Err(r) => return r,
    };
    match agent.refresh_machine_graph().await {
        Ok(count) => Json(serde_json::json!({"machines": count})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Human-readable fleet summary — the same text the planner is given.
pub async fn machines_prompt(State(h): State<AgentHandle>) -> Response {
    let agent = match h.require() {
        Ok(a) => a,
        Err(r) => return r,
    };
    match machines::describe_for_prompt(&agent.memory.graph) {
        Ok(text) => text.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Lets the UI decide which tabs to show.
#[derive(Serialize)]
pub struct Capabilities {
    pub chat: bool,
    pub terminal: bool,
    pub master_name: String,
}

pub async fn capabilities(State(h): State<AgentHandle>) -> Json<Capabilities> {
    Json(Capabilities {
        chat: h.agent.is_some(),
        terminal: true,
        master_name: h.master_name.clone(),
    })
}

// ---------------------------------------------------------------------------
// Master-agent model selection — local (Qwen/Ollama) or an API-key cloud
// provider (Z.ai, NVIDIA). Picking a cloud provider fully disables fallback
// to the local model (see `LlmRouter::set_provider`); the choice, and any
// Z.ai key entered through this endpoint, are persisted to
// `~/.hive/master-agent.json` (override the location with
// `HIVE_MASTER_AGENT_FILE`) using an atomic write with 0600 permissions,
// and re-applied at startup — always below whatever the environment and
// `hive.toml` configure. The key never appears in an API response, a log
// line, or a prompt.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ProviderOption {
    pub id: &'static str,
    pub label: &'static str,
    pub requires_api_key: bool,
    pub configured: bool,
}

#[derive(Serialize)]
pub struct MasterAgentSettings {
    pub provider: &'static str,
    pub options: Vec<ProviderOption>,
    pub local_model: String,
    pub local_available: bool,
}

#[derive(Deserialize)]
pub struct SetMasterAgentRequest {
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

fn provider_id(provider: hive_common::AiProvider) -> &'static str {
    match provider {
        hive_common::AiProvider::Local => "local",
        hive_common::AiProvider::Zai => "zai",
        hive_common::AiProvider::Nvidia => "nvidia",
        _ => "local",
    }
}

fn parse_persisted_provider(id: &str) -> Option<hive_common::AiProvider> {
    match id {
        "local" => Some(hive_common::AiProvider::Local),
        "zai" => Some(hive_common::AiProvider::Zai),
        "nvidia" => Some(hive_common::AiProvider::Nvidia),
        _ => None,
    }
}

fn parse_provider_id(id: &str) -> Result<hive_common::AiProvider, Response> {
    parse_persisted_provider(id).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown provider '{id}' (expected local, zai, or nvidia)"),
        )
            .into_response()
    })
}

async fn master_agent_settings_view(agent: &MasterAgent) -> MasterAgentSettings {
    MasterAgentSettings {
        provider: provider_id(agent.llm.current_provider()),
        options: vec![
            ProviderOption {
                id: "local",
                label: "Local (Qwen via Ollama)",
                requires_api_key: false,
                configured: true,
            },
            ProviderOption {
                id: "zai",
                label: "Z.ai (GLM)",
                requires_api_key: true,
                configured: agent.llm.zai_configured(),
            },
            ProviderOption {
                id: "nvidia",
                label: "NVIDIA",
                requires_api_key: true,
                configured: agent.llm.nvidia_configured(),
            },
        ],
        local_model: agent.llm.local_model_name(),
        local_available: agent.llm.local_available().await,
    }
}

pub async fn master_agent_settings(
    State(h): State<AgentHandle>,
) -> Result<Json<MasterAgentSettings>, Response> {
    let agent = h.require()?;
    Ok(Json(master_agent_settings_view(agent).await))
}

pub async fn set_master_agent(
    State(h): State<AgentHandle>,
    Json(req): Json<SetMasterAgentRequest>,
) -> Result<Json<MasterAgentSettings>, Response> {
    let agent = h.require()?;
    let provider = parse_provider_id(&req.provider)?;

    agent
        .llm
        .set_provider(provider, req.api_key.clone())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;

    if let Err(e) = save_master_agent_selection(&req.provider, req.api_key.as_deref()) {
        warn!(error = %e, "failed to persist master-agent selection; it will revert to the configured default on restart");
    }

    info!(provider = %req.provider, "master agent provider switched");
    Ok(Json(master_agent_settings_view(agent).await))
}

/// Env var overriding where the master-agent state lives (tests, parallel
/// installs on one host).
const MASTER_AGENT_FILE_ENV: &str = "HIVE_MASTER_AGENT_FILE";

/// Where the master-agent provider selection (and any Z.ai key entered
/// through the settings API) is persisted: `~/.hive/master-agent.json`, or
/// `$HIVE_MASTER_AGENT_FILE` when set. Never inside the project directory —
/// this can be a shared working tree.
fn master_agent_state_path() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os(MASTER_AGENT_FILE_ENV).filter(|p| !p.is_empty()) {
        return Some(std::path::PathBuf::from(path));
    }
    std::env::var_os("HOME").map(|home| {
        std::path::Path::new(&home)
            .join(".hive")
            .join("master-agent.json")
    })
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedMasterAgent {
    provider: String,
    #[serde(default)]
    zai_api_key: Option<String>,
}

/// Serializes the read-modify-write in `save_master_agent_selection`.
/// POSTs to the settings API can run concurrently on different Tokio
/// workers; two interleaved saves would read the same state and one update
/// (provider or key) would be lost. Poisoning only means a saver panicked
/// midway — the next save still takes the lock and rewrites the file.
static MASTER_AGENT_SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Write the state file: serialize, write a uniquely-named sibling temp
/// file created fresh with 0600 (`create_new` — a leftover temp from an
/// earlier crash can never be written through, so its looser permissions
/// are irrelevant), sync, then atomically rename over the destination. The
/// chmod-on-destination afterwards repairs a looser mode left by an older
/// version; the mode-on-create closes the umask window.
fn write_master_agent_state(
    path: &std::path::Path,
    state: &PersistedMasterAgent,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(state)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("master-agent.json");
    // Unique per attempt: concurrent savers (or a stale temp from a crashed
    // one) can never collide on the temp path, which would make the rename
    // race or clobber another writer's bytes.
    let temp = path.with_file_name(format!(
        "{file_name}.tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let write = || -> anyhow::Result<()> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?
        };
        #[cfg(not(unix))]
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        use std::io::Write as _;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    };
    let result = write();
    if result.is_err() {
        // A half-written temp file helps nobody.
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn save_master_agent_selection(provider: &str, api_key: Option<&str>) -> anyhow::Result<()> {
    let path = master_agent_state_path().ok_or_else(|| {
        anyhow::anyhow!(
            "HOME is not set and {MASTER_AGENT_FILE_ENV} is unset; cannot persist master-agent selection"
        )
    })?;

    // The whole read-modify-write is serialized: merge-on-save reads what is
    // on disk, so two concurrent saves must not interleave.
    let _serialized = MASTER_AGENT_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Merge with whatever is already on disk so switching to `local` (no
    // key) doesn't discard a previously entered Z.ai key. An empty key is
    // treated as absent — it would only ever construct a broken client.
    let mut state = load_master_agent_state().unwrap_or_default();
    state.provider = provider.to_string();
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        state.zai_api_key = Some(key.to_string());
    }

    write_master_agent_state(&path, &state)
}

fn load_master_agent_state() -> Option<PersistedMasterAgent> {
    let path = master_agent_state_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&contents) {
        Ok(state) => Some(state),
        Err(e) => {
            warn!(error = %e, "master-agent state file is corrupt; ignoring it (the next save rewrites it)");
            None
        }
    }
}

/// Apply a persisted master-agent selection to a freshly built router, if
/// one was saved by an earlier run. Called once at startup, before the
/// router is shared behind an `Arc`.
///
/// Precedence — the environment and `hive.toml` always win over the file:
/// - Z.ai key: a key already resolved from `[llm.zai]`/`Z_AI` by
///   `from_config` is never replaced; the persisted key only fills a router
///   that has none.
/// - Provider: an explicit `single_provider` in `hive.toml` beats the
///   persisted selection; with none configured, the persisted provider is
///   restored.
///
/// Failures are logged, not fatal — the process falls back to whatever
/// `hive.toml` configured.
pub fn apply_persisted_master_agent(
    llm: &hive_core::llm::LlmRouter,
    cfg: &hive_common::config::LlmConfig,
) {
    let Some(state) = load_master_agent_state() else {
        return;
    };
    // Make the persisted Z.ai key available first (a no-op when env/config
    // already provided one — exactly the precedence above). Seeding happens
    // regardless of the persisted provider so a key entered while running
    // on `local` still works after a restart.
    if let Some(key) = state
        .zai_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        llm.seed_zai_key_if_unconfigured(key.to_string());
    }
    let Some(provider) = parse_persisted_provider(&state.provider) else {
        warn!(provider = %state.provider, "ignoring unrecognized persisted master-agent provider");
        return;
    };
    if cfg.single_provider.is_some() {
        if cfg.single_provider != Some(provider) {
            info!(
                configured = ?cfg.single_provider,
                persisted = %state.provider,
                "hive.toml single_provider wins over the persisted master-agent selection"
            );
        }
        return;
    }
    if let Err(e) = llm.set_provider(provider, None) {
        warn!(error = %e, provider = %state.provider, "could not restore persisted master-agent selection");
    } else {
        info!(provider = %state.provider, "restored persisted master-agent selection");
    }
}

// ---------------------------------------------------------------------------
// Fleet management — add/list/remove SSH worker machines from the UI,
// without hand-editing `config/workers.toml`. UI-added workers are persisted
// separately (`~/.hive/fleet-ui.json`) and merged with the configured fleet
// at startup: `config/workers.toml` is hand-maintained (its comments carry
// reasoning that matters — see docs/ROADMAP.md), and this never rewrites it.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct FleetWorker {
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: Option<u16>,
    pub tags: Vec<String>,
    pub status: &'static str,
    /// Whether this worker can be removed through the API — only true for
    /// ones added here, never for `config/workers.toml` entries.
    pub removable: bool,
}

#[derive(Deserialize)]
pub struct AddWorkerRequest {
    pub name: String,
    pub host: String,
    pub user: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn worker_status_label(status: hive_common::WorkerStatus) -> &'static str {
    match status {
        hive_common::WorkerStatus::Online => "online",
        hive_common::WorkerStatus::Busy => "busy",
        hive_common::WorkerStatus::Offline => "offline",
        hive_common::WorkerStatus::Unhealthy => "unhealthy",
    }
}

async fn fleet_view(agent: &MasterAgent) -> Vec<FleetWorker> {
    let ui_added = load_fleet_ui_state();
    agent
        .workers
        .snapshot()
        .into_iter()
        .map(|w| FleetWorker {
            name: w.info.name.clone(),
            host: w.info.host.clone(),
            user: w.info.user.clone(),
            port: w.info.port,
            tags: w.info.tags.clone(),
            status: worker_status_label(w.status()),
            removable: ui_added.iter().any(|added| added.name == w.info.name),
        })
        .collect()
}

pub async fn list_fleet(State(h): State<AgentHandle>) -> Result<Json<Vec<FleetWorker>>, Response> {
    let agent = h.require()?;
    Ok(Json(fleet_view(agent).await))
}

pub async fn add_fleet_worker(
    State(h): State<AgentHandle>,
    Json(req): Json<AddWorkerRequest>,
) -> Result<Json<Vec<FleetWorker>>, Response> {
    let agent = h.require()?;

    let name = req.name.trim().to_string();
    let host = req.host.trim().to_string();
    let user = req.user.trim().to_string();
    if name.is_empty() || host.is_empty() || user.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "name, host, and user are all required",
        )
            .into_response());
    }
    let valid_name = name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !name.starts_with('-');
    if !valid_name {
        return Err((
            StatusCode::BAD_REQUEST,
            "Name may contain only letters, digits and . - _",
        )
            .into_response());
    }
    crate::ssh_setup::validate_host(&host)
        .and(crate::ssh_setup::validate_user(&user))
        .and(crate::ssh_setup::validate_port(req.port))
        .map_err(|e| (StatusCode::BAD_REQUEST, e).into_response())?;

    let info = hive_common::WorkerInfo {
        name,
        host,
        user,
        port: req.port,
        tags: req.tags,
        local: false,
        container: None,
    };

    agent
        .workers
        .add_worker(info.clone())
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()).into_response())?;

    if let Err(e) = save_fleet_ui_addition(&info) {
        warn!(error = %e, worker = %info.name, "failed to persist added worker; it will not survive a restart");
    }
    sync_ssh_config().await;

    // Probe the new worker (and re-probe the rest — the pool has no
    // single-worker refresh) so the response reflects real reachability
    // instead of the "Offline" every worker starts at.
    agent.workers.refresh_health().await;

    info!(worker = %info.name, host = %info.host, "worker added to fleet from settings UI");
    Ok(Json(fleet_view(agent).await))
}

pub async fn remove_fleet_worker(
    State(h): State<AgentHandle>,
    Path(name): Path<String>,
) -> Result<Json<Vec<FleetWorker>>, Response> {
    let agent = h.require()?;

    let ui_added = load_fleet_ui_state();
    if !ui_added.iter().any(|w| w.name == name) {
        return Err((
            StatusCode::FORBIDDEN,
            "only workers added through this API can be removed here; edit config/workers.toml for the rest",
        )
            .into_response());
    }

    agent.workers.remove_worker(&name);
    if let Err(e) = save_fleet_ui_removal(&name) {
        warn!(error = %e, worker = %name, "failed to persist worker removal; it will reappear on restart");
    }
    sync_ssh_config().await;

    info!(worker = %name, "worker removed from fleet from settings UI");
    Ok(Json(fleet_view(agent).await))
}

/// Give machines added here Hive's SSH key where the user's own ssh config
/// doesn't already cover them.
async fn sync_ssh_config() {
    let Some(paths) = crate::ssh_setup::SshPaths::from_env() else {
        return;
    };
    let hosts: Vec<String> = load_fleet_ui_state().into_iter().map(|w| w.host).collect();
    if let Err(e) = crate::ssh_setup::sync_managed_config(&paths, &hosts).await {
        warn!(error = %e, "could not update Hive's managed ssh config");
    }
}

fn fleet_ui_state_path() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| std::path::Path::new(&home).join(".hive").join("fleet-ui.json"))
}

fn load_fleet_ui_state() -> Vec<hive_common::WorkerInfo> {
    let Some(path) = fleet_ui_state_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn write_fleet_ui_state(workers: &[hive_common::WorkerInfo]) -> anyhow::Result<()> {
    let path = fleet_ui_state_path()
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot persist the fleet-UI state"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(workers)?)?;
    Ok(())
}

fn save_fleet_ui_addition(info: &hive_common::WorkerInfo) -> anyhow::Result<()> {
    let mut workers = load_fleet_ui_state();
    workers.retain(|w| w.name != info.name);
    workers.push(info.clone());
    write_fleet_ui_state(&workers)
}

fn save_fleet_ui_removal(name: &str) -> anyhow::Result<()> {
    let mut workers = load_fleet_ui_state();
    workers.retain(|w| w.name != name);
    write_fleet_ui_state(&workers)
}

/// Merge any UI-added workers into a freshly built pool. Called once at
/// startup, before the pool is shared behind an `Arc`. A worker whose name
/// now collides with one in `config/workers.toml` (the file always wins) is
/// skipped with a warning rather than failing startup.
pub fn apply_persisted_fleet(workers: &hive_core::workers::WorkerPool) {
    for info in load_fleet_ui_state() {
        let name = info.name.clone();
        if let Err(e) = workers.add_worker(info) {
            warn!(error = %e, worker = %name, "skipping persisted fleet-UI worker");
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    fn empty_plan() -> PlannedRun {
        serde_json::from_value(serde_json::json!({
            "id": "plan-1",
            "user_input": "hi",
            "summary": "nothing",
            "complexity": "simple",
            "routed_provider": "local",
            "provider": "local",
            "steps": []
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_planning_deadline_failure_is_retried_once_and_can_succeed() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let plan = empty_plan();
        let result = plan_with_retry(
            move || {
                let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
                let plan = plan.clone();
                async move {
                    if first {
                        // The first attempt hangs past the deadline.
                        std::future::pending::<()>().await;
                    }
                    Ok(plan)
                }
            },
            Duration::from_millis(30),
        )
        .await
        .unwrap();
        assert_eq!(result.id, "plan-1");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_second_deadline_failure_tells_the_user_what_to_do() {
        let response = plan_with_retry(
            || std::future::pending::<anyhow::Result<PlannedRun>>(),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("automatic retry"), "{text}");
        assert!(text.contains("resend"), "{text}");
        assert!(text.contains("No commands were executed"), "{text}");
    }

    #[tokio::test]
    async fn planner_calls_are_bounded_to_two_concurrent() {
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (inside, peak) = (inside.clone(), peak.clone());
                tokio::spawn(bounded_plan(
                    async move {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                        anyhow::Ok(())
                    },
                    Duration::from_secs(30),
                ))
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        assert_eq!(inside.load(Ordering::SeqCst), 0);
        // The semaphore allows at most two planner calls at once, and the
        // waiting calls above really did run two at a time.
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn planning_deadline_cancels_work_before_execution() {
        struct PendingWork(Arc<AtomicBool>);
        impl Drop for PendingWork {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = PendingWork(cancelled.clone());
        let response = bounded_plan(
            async move {
                let _guard = guard;
                std::future::pending::<anyhow::Result<PlannedRun>>().await
            },
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(cancelled.load(Ordering::SeqCst));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("No commands were executed"));
    }

    #[tokio::test]
    async fn provider_failure_is_reported_without_waiting_for_total_deadline() {
        let response = bounded_plan::<()>(
            async { anyhow::bail!("NVIDIA request deadline exceeded") },
            Duration::from_secs(150),
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("NVIDIA request deadline exceeded"));
    }
}

#[cfg(test)]
mod master_agent_state_tests {
    use super::*;
    use crate::{AppState, app_router};
    use axum::body::Body;
    use axum::http::{header, Request};
    use hive_common::config::{CloudLlmConfig, LlmConfig, LocalLlmConfig, NvidiaConfig};
    use hive_core::llm::LlmRouter;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Every test that touches `HIVE_MASTER_AGENT_FILE` or `HOME` holds this
    /// lock: `std::env` is process-global and tests run in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restores the previous value (or absence) of an env var on drop.
    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[cfg(unix)]
    fn assert_private(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{path:?} was mode {mode:o}");
    }

    /// A fresh temp directory whose `master-agent.json` is wired up as the
    /// state file via the env override.
    fn isolated_state_file() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hive-master-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("master-agent.json");
        (file, dir)
    }

    fn quiet_llm_config() -> LlmConfig {
        LlmConfig {
            single_provider: None,
            nvidia: NvidiaConfig::default(),
            local: LocalLlmConfig::default(),
            gemini: None,
            claude: None,
            codex: None,
            zai: None,
        }
    }

    #[test]
    fn save_roundtrips_atomically_and_privately_via_the_env_override() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());

        save_master_agent_selection("zai", Some("secret-zai-key-1")).unwrap();
        assert!(file.exists(), "state must land at $HIVE_MASTER_AGENT_FILE");
        #[cfg(unix)]
        assert_private(&file);
        assert!(
            !file.with_extension("json.tmp").exists(),
            "no temp file may survive the atomic rename"
        );

        let state = load_master_agent_state().expect("saved state must load back");
        assert_eq!(state.provider, "zai");
        assert_eq!(state.zai_api_key.as_deref(), Some("secret-zai-key-1"));

        // Switching to local must not discard the stored key (merge-on-save).
        save_master_agent_selection("local", None).unwrap();
        let state = load_master_agent_state().unwrap();
        assert_eq!(state.provider, "local");
        assert_eq!(state.zai_api_key.as_deref(), Some("secret-zai-key-1"));
        #[cfg(unix)]
        assert_private(&file);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_state_path_is_dot_hive_in_the_home_directory() {
        let _env = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("hive-master-agent-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let _no_override = EnvGuard::remove(MASTER_AGENT_FILE_ENV);
        let _guard = EnvGuard::set("HOME", home.to_str().unwrap());

        save_master_agent_selection("local", None).unwrap();
        let file = home.join(".hive").join("master-agent.json");
        assert!(file.exists(), "default path is $HOME/.hive/master-agent.json");
        #[cfg(unix)]
        assert_private(&file);

        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn corrupt_state_file_is_ignored_and_self_heals_on_the_next_save() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        std::fs::write(&file, "{\"provider\": \"zai\", \"zai_api_key\": \"truncat").unwrap();

        assert!(load_master_agent_state().is_none(), "corrupt JSON loads as nothing");

        // Startup application survives the corrupt file as a no-op.
        let cfg = quiet_llm_config();
        let router = LlmRouter::from_config(&cfg);
        apply_persisted_master_agent(&router, &cfg);
        assert_eq!(router.current_provider(), hive_common::AiProvider::Local);
        assert!(!router.zai_configured());

        // The next save rewrites the corrupt file with valid state.
        save_master_agent_selection("zai", Some("fresh-key")).unwrap();
        let state = load_master_agent_state().expect("file must be valid again");
        assert_eq!(state.provider, "zai");
        assert_eq!(state.zai_api_key.as_deref(), Some("fresh-key"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn startup_restores_a_persisted_selection_when_config_is_silent() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        save_master_agent_selection("zai", Some("persisted-key")).unwrap();

        let cfg = quiet_llm_config();
        let router = LlmRouter::from_config(&cfg);
        apply_persisted_master_agent(&router, &cfg);
        assert_eq!(router.current_provider(), hive_common::AiProvider::Zai);
        assert!(router.zai_configured());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn configured_single_provider_wins_over_the_persisted_selection() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        save_master_agent_selection("zai", Some("persisted-key")).unwrap();

        let mut cfg = quiet_llm_config();
        cfg.single_provider = Some(hive_common::AiProvider::Local);
        let router = LlmRouter::from_config(&cfg);
        apply_persisted_master_agent(&router, &cfg);
        assert_eq!(
            router.current_provider(),
            hive_common::AiProvider::Local,
            "hive.toml single_provider beats the persisted provider"
        );
        // The persisted key is still made available — precedence decides
        // which key wins when both exist, not whether the file is read.
        assert!(router.zai_configured());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_resolved_keys_win_over_the_persisted_one() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        save_master_agent_selection("zai", Some("file-key")).unwrap();

        let mut cfg = quiet_llm_config();
        cfg.zai = Some(CloudLlmConfig {
            api_key: Some("config-key".into()),
            model: "glm-test".into(),
            ..CloudLlmConfig::default()
        });
        let router = LlmRouter::from_config(&cfg);
        assert!(router.zai_configured(), "config key already wired up");
        apply_persisted_master_agent(&router, &cfg);
        assert_eq!(router.current_provider(), hive_common::AiProvider::Zai);
        // The client built from `hive.toml` survives: seeding is a no-op
        // when a key is already configured (asserted against the live
        // client in the hive-core llm tests; here, provider and configured
        // state are the observable contract).
        assert!(router.zai_configured());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_persisted_key_never_builds_a_broken_client() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        std::fs::write(&file, "{\"provider\": \"zai\", \"zai_api_key\": \"  \"}").unwrap();

        let cfg = quiet_llm_config();
        let router = LlmRouter::from_config(&cfg);
        apply_persisted_master_agent(&router, &cfg);
        assert!(!router.zai_configured(), "blank key must not seed a client");
        assert_eq!(
            router.current_provider(),
            hive_common::AiProvider::Local,
            "restoring zai without any key fails soft and keeps the default"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn settings_api_persists_the_key_but_never_echoes_it() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        let secret = "super-secret-zai-key-e2e";

        let agent = MasterAgent::new(
            LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            hive_core::workers::WorkerPool::new(vec![]),
            hive_core::skills::SkillRegistry::new(),
            hive_core::memory::MemorySystem::new(),
        );
        let state = AppState {
            auth: crate::auth::Auth::new("test-password".into()),
            agent: AgentHandle {
                agent: Some(Arc::new(agent)),
                history: None,
                master_name: "test".into(),
            },
            workers: crate::workers::WorkerIngest::from_env(),
            incidents: crate::incidents::IncidentReview::new(
                hive_core::watchdog::incidents::IncidentStore::in_memory().unwrap(),
            ),
        };
        let static_dir =
            std::env::temp_dir().join(format!("hive-web-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(static_dir.join("login")).unwrap();
        std::fs::write(static_dir.join("index.html"), "<p>ok</p>").unwrap();
        let app = app_router(state, static_dir.to_str().unwrap());

        let login = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=test-password"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER, "password login succeeds");
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let post = app
            .clone()
            .oneshot(
                Request::post("/api/settings/master-agent")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"provider": "zai", "api_key": secret}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post.status(), StatusCode::OK);
        let body = axum::body::to_bytes(post.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            !body.contains(secret),
            "POST response must never echo the API key: {body}"
        );
        let view: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(view["provider"], "zai");
        assert_eq!(view["options"][1]["configured"], true, "zai now configured");

        let get = app
            .clone()
            .oneshot(
                Request::get("/api/settings/master-agent")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let body = axum::body::to_bytes(get.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            !body.contains(secret),
            "GET response must never include the API key: {body}"
        );
        assert!(
            !body.contains("\"api_key\""),
            "settings view carries configured booleans only: {body}"
        );

        // The key did land in the (private) state file.
        assert!(file.exists());
        #[cfg(unix)]
        assert_private(&file);
        let state = load_master_agent_state().unwrap();
        assert_eq!(state.zai_api_key.as_deref(), Some(secret));

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&static_dir).ok();
    }

    #[test]
    fn concurrent_saves_never_lose_the_key_or_leave_temp_files() {
        let _env = ENV_LOCK.lock().unwrap();
        let (file, dir) = isolated_state_file();
        let _guard = EnvGuard::set(MASTER_AGENT_FILE_ENV, file.to_str().unwrap());
        save_master_agent_selection("zai", Some("keeper-key")).unwrap();

        // Eight threads interleaving provider switches and key-carrying
        // saves: without the save lock one update is lost or the temp-file
        // renames collide (ENOOENT/clobber); with it every save observes
        // the previous one's result.
        let threads: Vec<_> = (0..8usize)
            .map(|i| {
                std::thread::spawn(move || {
                    for round in 0..25usize {
                        let provider = if (i + round) % 2 == 0 { "local" } else { "zai" };
                        let key = if (i + round) % 3 == 0 { None } else { Some("keeper-key") };
                        save_master_agent_selection(provider, key).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        let state = load_master_agent_state().expect("final file must parse");
        assert_eq!(
            state.zai_api_key.as_deref(),
            Some("keeper-key"),
            "merge-on-save must never lose the stored key"
        );
        assert!(state.provider == "local" || state.provider == "zai");
        #[cfg(unix)]
        assert_private(&file);
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
