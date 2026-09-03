//! Chat with the master agent, with an approval gate in front of local execution.
//!
//! The flow is deliberately two-legged. `POST /api/chat` plans and runs
//! everything the watchdog is happy with, then stops and reports anything it
//! flagged. `POST /api/chat/{run_id}/approve` resumes that same plan with the
//! user's decisions. The plan is held server-side between the two calls so the
//! browser cannot hand back a *different* command than the one it was shown.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hive_core::agent::run::{Approvals, PlannedRun, RunResult};
use hive_core::agent::MasterAgent;
use hive_core::memory::machines;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Plans awaiting an approval decision, keyed by run id.
///
/// Bounded: a plan is dropped as soon as its run completes, and the map is
/// trimmed when it grows past [`MAX_PENDING`], so an abandoned tab cannot leak
/// plans indefinitely.
pub type PendingRuns = Arc<Mutex<HashMap<String, PlannedRun>>>;

const MAX_PENDING: usize = 32;

/// The agent, if this deployment has one.
///
/// `hive-web` also runs on workers, which have no Ollama and no config — there
/// it serves only the terminal, and the chat routes report that plainly rather
/// than the binary refusing to start.
#[derive(Clone)]
pub struct AgentHandle {
    pub agent: Option<Arc<MasterAgent>>,
    pub pending: PendingRuns,
    /// Name the master is known by in the machine graph.
    pub master_name: String,
}

impl AgentHandle {
    pub fn disabled() -> Self {
        Self {
            agent: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            master_name: "master".into(),
        }
    }

    pub fn enabled(agent: Arc<MasterAgent>, master_name: String) -> Self {
        Self {
            agent: Some(agent),
            pending: Arc::new(Mutex::new(HashMap::new())),
            master_name,
        }
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
}

#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
    #[serde(default)]
    pub project_id: Option<String>,
}

/// What the browser gets back: the routing decision, the plan, and what
/// happened to each step.
#[derive(Serialize)]
pub struct ChatReply {
    pub run: PlannedRun,
    pub result: RunResult,
}

pub async fn chat(State(h): State<AgentHandle>, Json(req): Json<ChatRequest>) -> Response {
    let agent = match h.require() {
        Ok(a) => a,
        Err(r) => return r,
    };
    if req.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message").into_response();
    }

    let plan = match agent
        .plan_run(&req.message, req.project_id.as_deref())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "planning failed");
            return (StatusCode::BAD_GATEWAY, format!("planning failed: {e}")).into_response();
        }
    };
    info!(
        run = %plan.id,
        complexity = %plan.complexity,
        provider = %plan.provider,
        steps = plan.steps.len(),
        gated = plan.gated_steps().len(),
        "planned"
    );

    // Nothing is approved on the first pass — flagged steps come back for a
    // decision rather than running.
    let result = agent.execute_run(&plan, &Approvals::none()).await;

    if !result.is_complete() {
        let mut pending = h.pending.lock().await;
        if pending.len() >= MAX_PENDING {
            pending.clear();
        }
        pending.insert(plan.id.clone(), plan.clone());
    }

    Json(ChatReply { run: plan, result }).into_response()
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
    let agent = match h.require() {
        Ok(a) => a,
        Err(r) => return r,
    };

    let plan = {
        let pending = h.pending.lock().await;
        match pending.get(&run_id) {
            Some(p) => p.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "No such run awaiting approval — it may have already completed.",
                )
                    .into_response()
            }
        }
    };

    let mut approvals = Approvals::none();
    for id in req.approved {
        approvals.approve(id);
    }
    for id in req.denied {
        approvals.deny(id);
    }

    // Steps that already ran on the first pass run again here. That is correct
    // for this design — the first pass only executes ungated steps, and the
    // gated ones are what the user is deciding on — but it does mean a plan
    // mixing both re-runs its safe commands. They are, by definition, the ones
    // the watchdog considered safe to repeat.
    let result = agent.execute_run(&plan, &approvals).await;

    if result.is_complete() {
        h.pending.lock().await.remove(&run_id);
    }

    Json(ChatReply { run: plan, result }).into_response()
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
