//! Feedback-driven chat with the master agent and approvals for flagged actions.
//!
//! The flow is deliberately two-legged. `POST /api/chat` plans and runs
//! everything the watchdog is happy with, then stops and reports anything it
//! flagged. `POST /api/chat/{run_id}/approve` resumes that same plan with the
//! user's decisions. The plan is held server-side between the two calls so the
//! browser cannot hand back a *different* command than the one it was shown.

use std::sync::Arc;

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

// Bound the whole planning phase, including sequential model and memory calls.
// This future never executes commands, so dropping it on timeout is safe.
async fn bounded_plan(
    plan: impl std::future::Future<Output = anyhow::Result<PlannedRun>>,
    deadline: std::time::Duration,
) -> Result<PlannedRun, Response> {
    match tokio::time::timeout(deadline, plan).await {
        Ok(Ok(plan)) => Ok(plan),
        Ok(Err(e)) => {
            warn!(error = %e, "planning failed");
            Err((StatusCode::BAD_GATEWAY, format!("planning failed: {e}")).into_response())
        }
        Err(_) => {
            warn!("total planning deadline exceeded");
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                "Planning timed out. No commands were executed from this planning round. Earlier results, if any, remain saved.",
            )
                .into_response())
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

    fn require(&self) -> Result<&Arc<MasterAgent>, Response> {
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
    let plan = match bounded_plan(
        agent.plan_chat_run(&turn.user_input, history),
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

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

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
        let response = bounded_plan(
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
