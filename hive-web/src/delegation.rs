//! Website delegation API and durable SSH synchronization.
use crate::chat::AgentHandle;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hive_core::{
    delegation::{
        self, inventory,
        store::{MessageRejected, Run, RunStore},
        transport,
    },
    memory::chats::SavedTurn,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// How long a run waits after a failed sync before it is attempted again.
const SYNC_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

pub fn store(h: &AgentHandle) -> anyhow::Result<RunStore> {
    let agent = h
        .agent
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Delegation unavailable"))?;
    RunStore::new(agent.memory.graph.shared_conn())
}
fn error(e: anyhow::Error) -> Response {
    (StatusCode::BAD_REQUEST, e.to_string()).into_response()
}

/// Overall deadline for one delegation planning attempt. A deadline failure
/// is retried once by [`crate::chat::plan_with_retry`]. The planner-slot
/// wait is bounded by the whole-request budget (PLANNING_DEADLINE + retry),
/// and NVIDIA's own overall deadline (120s by default) fits inside this budget.
const PLANNING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(240);

pub async fn process(h: AgentHandle, turn: SavedTurn) -> Response {
    process_with_deadline_slots(h, turn, PLANNING_DEADLINE, &crate::chat::PLANNER_SLOTS).await
}

pub(crate) async fn process_with_deadline_slots(
    h: AgentHandle,
    turn: SavedTurn,
    deadline: std::time::Duration,
    slots: &tokio::sync::Semaphore,
) -> Response {
    let agent = match h.agent.as_ref() {
        Some(a) => a,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No agent configured").into_response(),
    };
    let history = match h.history.as_ref() {
        Some(hist) => hist,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No chat store configured").into_response(),
    };
    let context = match history.context(&turn) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(conversation_id = %turn.conversation_id, turn_id = %turn.id, error = %e, "delegation context lookup failed");
            let _ = history.finish(&turn.id, "failed", &e.to_string(), None);
            return error(e);
        }
    };
    let context_text = context.join("\n");
    let plan = match crate::chat::plan_with_retry_slots(
        slots,
        || delegation::plan(agent, &turn.user_input, &context_text),
        deadline,
    )
    .await
    {
        Ok(plan) => plan,
        Err(response) => {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&body).to_string();
            // Every planning failure is recorded with the conversation it
            // belongs to, so a lost plan is traceable from the logs alone.
            tracing::warn!(conversation_id = %turn.conversation_id, turn_id = %turn.id, status = %status, error = %text, "delegation planning failed");
            let _ = history.finish(&turn.id, "failed", &text, None);
            return (status, text).into_response();
        }
    };
    let store = match store(&h) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(conversation_id = %turn.conversation_id, turn_id = %turn.id, error = %e, "delegation store lookup failed");
            let _ = history.finish(&turn.id, "failed", &e.to_string(), None);
            return error(e);
        }
    };
    let reply = match start_plan(agent, &store, &turn.id, &turn.conversation_id, &plan).await {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(conversation_id = %turn.conversation_id, turn_id = %turn.id, error = %e, "delegation plan startup failed");
            let _ = history.finish(&turn.id, "failed", &e.to_string(), None);
            return error(e);
        }
    };
    // A detached run owns its own state. Finishing the receipt keeps this
    // conversation's composer available while the real agents work.
    if let Err(e) = history.finish(&turn.id, "completed", &plan.summary, Some(&reply)) {
        return error(e);
    }
    Json(reply).into_response()
}

/// How long Hive gives one container to be created, image build included.
const CONTAINER_CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);
/// How long to wait for a new container's agents to be probed.
const CONTAINER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3 * 60);

/// Create the plan's containers, then its runs. A container that can't be
/// created stops the whole plan before any agent starts; nothing is retried.
async fn start_plan(
    agent: &hive_core::agent::MasterAgent,
    store: &RunStore,
    task_id: &str,
    conversation_id: &str,
    plan: &delegation::DelegationPlan,
) -> anyhow::Result<Value> {
    let created = create_containers(agent, plan).await?;
    let runs = store.create(task_id, conversation_id, plan)?;
    Ok(json!({"conversation_id":conversation_id,"delegation":{
        "task_id":task_id,"summary":plan.summary,"runs":runs,"containers_created":created}}))
}

async fn create_containers(
    agent: &hive_core::agent::MasterAgent,
    plan: &delegation::DelegationPlan,
) -> anyhow::Result<Vec<String>> {
    let mut created = Vec::new();
    for planned in &plan.containers {
        let result = async {
            let machine = delegation::machine(agent, &planned.host)
                .ok_or_else(|| anyhow::anyhow!("{} is not a configured machine", planned.host))?;
            let taken: Vec<String> =
                delegation::targets(agent).into_iter().map(|t| t.name).collect();
            tokio::time::timeout(
                CONTAINER_CREATE_TIMEOUT,
                delegation::containers::create(&machine, &planned.name, &taken),
            )
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))?
        }
        .await;
        if let Err(e) = result {
            anyhow::bail!(
                "Could not create container {} on {}: {e}. No agents were started.",
                planned.name,
                planned.host
            );
        }
        tracing::info!(container = %planned.name, host = %planned.host, "planner created a container");
        created.push(planned.name.clone());
    }
    if !created.is_empty() {
        // Setup checks read the inventory, so learn the new agents first.
        let names: Vec<&str> = created.iter().map(String::as_str).collect();
        match tokio::time::timeout(
            CONTAINER_PROBE_TIMEOUT,
            delegation::inventory::refresh_devices(agent, &names),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "probing new containers failed"),
            Err(_) => tracing::warn!("probing new containers timed out"),
        }
    }
    Ok(created)
}

#[derive(Default, Deserialize)]
pub struct RunQuery {
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub after: i64,
    /// Latest N events in order, for opening a long session at its end.
    pub tail: Option<usize>,
    /// Comma-separated states to keep, for callers that only count some.
    pub state: Option<String>,
    /// A run ID: return only the runs of that run's task, for a session page.
    pub task_of: Option<String>,
}
pub async fn list(State(h): State<AgentHandle>, Query(q): Query<RunQuery>) -> Response {
    match store(&h).and_then(|s| s.list()) {
        Ok(runs) => {
            let task = q.task_of.as_ref().map(|id| {
                runs.iter().find(|r| &r.id == id).map(|r| r.task_id.clone())
            });
            let states = q.state.as_deref().map(|s| s.split(',').collect::<Vec<_>>());
            Json(
                runs.iter()
                    .filter(|r| {
                        q.conversation_id
                            .as_ref()
                            .is_none_or(|id| id == &r.conversation_id)
                            && states.as_ref().is_none_or(|s| s.contains(&r.state.as_str()))
                            && task.as_ref().is_none_or(|t| t.as_ref() == Some(&r.task_id))
                    })
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(e) => error(e),
    }
}
/// Task-scoped public roster; shares the router's API authentication middleware.
pub async fn team(State(h): State<AgentHandle>, Path(id): Path<String>) -> Response {
    match store(&h).and_then(|s| s.list()) {
        Ok(runs) => Json(runs.iter().filter(|r| r.task_id == id)
            .map(delegation::team_profile).collect::<Vec<_>>()).into_response(),
        Err(e) => error(e),
    }
}

pub async fn events(
    State(h): State<AgentHandle>,
    Path(id): Path<String>,
    Query(q): Query<RunQuery>,
) -> Response {
    match store(&h).and_then(|s| {
        s.get(&id)?;
        match q.tail {
            Some(n) => s.recent_events(&id, n),
            None => s.events(&id, q.after.max(0)),
        }
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(e),
    }
}
#[derive(Deserialize, Default)]
pub struct AuditQuery {
    /// `?full=1` re-verifies the whole chain instead of only new rows.
    #[serde(default)]
    pub full: Option<String>,
}
pub async fn audit(
    State(h): State<AgentHandle>,
    Path(id): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let full = matches!(q.full.as_deref(), Some("1" | "true"));
    match store(&h).and_then(|s| if full { s.audit_full(&id) } else { s.audit(&id) }) {
        Ok(report) => Json(report).into_response(),
        Err(e) => error(e),
    }
}

pub async fn autonomy() -> Json<Value> {
    Json(json!({ "mode": delegation::autonomy() }))
}
pub async fn set_autonomy(Json(body): Json<Value>) -> Response {
    let mode = match serde_json::from_value::<delegation::Autonomy>(body["mode"].clone()) {
        Ok(mode) => mode,
        Err(_) => return (StatusCode::BAD_REQUEST, "mode must be yolo or ask").into_response(),
    };
    match delegation::set_autonomy(mode) {
        Ok(()) => Json(json!({ "mode": mode })).into_response(),
        Err(e) => error(e),
    }
}
#[derive(Deserialize)]
pub struct Decision {
    pub id: String,
    pub fingerprint: String,
    pub decision: String,
}
pub async fn decide(
    State(h): State<AgentHandle>,
    Path(id): Path<String>,
    Json(d): Json<Decision>,
) -> Response {
    match store(&h).and_then(|s| s.decide(&id, &d.id, &d.fingerprint, &d.decision)) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"saved":true}))).into_response(),
        Err(e) => error(e),
    }
}
#[derive(Deserialize)]
pub struct Message {
    pub id: String,
    pub text: String,
}
pub async fn message(
    State(h): State<AgentHandle>,
    Path(id): Path<String>,
    Json(m): Json<Message>,
) -> Response {
    if uuid::Uuid::parse_str(&m.id).is_err() || m.text.trim().is_empty() || m.text.len() > 16000 {
        return (StatusCode::BAD_REQUEST, "Invalid message").into_response();
    }
    match store(&h).and_then(|s| {
        s.message(
            &m.id,
            "user",
            &id,
            &json!({"id":m.id,"text":m.text,"source":"user"}),
        )
    }) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"saved":true}))).into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
pub struct Replacement {
    pub device: String,
    pub agent: Option<String>,
    pub context: Option<String>,
}
pub async fn replace(
    State(h): State<AgentHandle>,
    Path(id): Path<String>,
    Json(request): Json<Replacement>,
) -> Response {
    let result = (|| -> anyhow::Result<Run> {
        let store = store(&h)?;
        let old = store.get(&id)?;
        let mut assignment = old.assignment.clone();
        assignment.objective = assignment
            .objective
            .replace(&assignment.device, &request.device);
        for criterion in &mut assignment.acceptance_criteria {
            *criterion = criterion.replace(&assignment.device, &request.device);
        }
        assignment.device = request.device;
        if let Some(context) = request.context {
            anyhow::ensure!(context.len() <= 16000, "Context too long");
            assignment.objective.push_str("\n");
            assignment.objective.push_str(&context);
        }
        if let Some(agent) = request.agent {
            assignment.agent = agent;
        }
        let plan = delegation::DelegationPlan {
            summary: "Move assignment while retaining peer work".into(),
            assignments: vec![assignment.clone()],
            containers: vec![],
        };
        delegation::validate(&plan, h.agent.as_ref().unwrap())?;
        store.replace(&id, &assignment)
    })();
    match result {
        Ok(run) => Json(run).into_response(),
        Err(e) => error(e),
    }
}

pub async fn retry_setup(State(h): State<AgentHandle>, Path(id): Path<String>) -> Response {
    match store(&h).and_then(|s| s.retry_setup(&id)) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"saved":true}))).into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reconciliation {
    pub fingerprint: String,
    pub reason: String,
    pub evidence: String,
    pub acknowledge_ids: Vec<String>,
}

impl Reconciliation {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.fingerprint.len() == 64 && self.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid recovery fingerprint"
        );
        for text in [&self.reason, &self.evidence] {
            anyhow::ensure!(
                !text.trim().is_empty() && text.len() <= 16000,
                "Recovery reason and inspected side-effect evidence are required"
            );
        }
        anyhow::ensure!(
            self.acknowledge_ids.len() <= 1000
                && self
                    .acknowledge_ids
                    .iter()
                    .all(|id| !id.is_empty() && id.len() <= 256),
            "Invalid uncertain message acknowledgments"
        );
        Ok(())
    }
}

async fn recovery_control(
    h: &AgentHandle,
    id: &str,
    request: Option<&Reconciliation>,
) -> anyhow::Result<Value> {
    let store = store(h)?;
    let run = store.get(id)?;
    anyhow::ensure!(
        matches!(
            run.state.as_str(),
            "failed" | "disconnected" | "needs-setup"
        ),
        "Only failed or disconnected native runs can be reconciled"
    );
    let runner = run
        .runner_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Run never launched; use setup retry"))?;
    let agent = h
        .agent
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Delegation unavailable"))?;
    let worker = delegation::target(agent, &run.assignment.device)
        .ok_or_else(|| anyhow::anyhow!("Device removed from configured fleet"))?;
    let payload = request.map(|request| json!({"fingerprint":request.fingerprint,"reason":request.reason,"evidence":request.evidence,"acknowledge_ids":request.acknowledge_ids}));
    let result = transport::control(
        &worker,
        runner,
        if request.is_some() {
            "reconcile"
        } else {
            "reconcile-inspect"
        },
        id,
        0,
        payload.as_ref(),
    )
    .await?;
    // The remote journal remains authoritative. Periodic synchronization imports
    // its audit event and state; a lost response never queues a second launch.
    Ok(serde_json::from_str(&result)?)
}

pub async fn recovery(State(h): State<AgentHandle>, Path(id): Path<String>) -> Response {
    match recovery_control(&h, &id, None).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(e) => error(e),
    }
}

pub async fn reconcile(
    State(h): State<AgentHandle>,
    Path(id): Path<String>,
    Json(request): Json<Reconciliation>,
) -> Response {
    if let Err(e) = request.validate() {
        return error(e);
    }
    match recovery_control(&h, &id, Some(&request)).await {
        Ok(receipt) => (StatusCode::ACCEPTED, Json(receipt)).into_response(),
        Err(e) => error(e),
    }
}

pub fn start(h: AgentHandle) {
    // Full re-verification once per process start; polls are then incremental.
    match store(&h).and_then(|s| s.verify_audit_chain()) {
        Ok(true) => {}
        Ok(false) => tracing::error!("relay audit chain failed full verification"),
        Err(error) => tracing::warn!(error=%error, "relay audit verification unavailable"),
    }
    let reviewer = h.clone();
    tokio::spawn(async move {
        loop {
            if let (Some(agent), Ok(store)) = (&reviewer.agent, store(&reviewer)) {
                if let Ok(runs) = store.list() {
                    let mut tasks: std::collections::BTreeMap<String, Vec<Run>> =
                        std::collections::BTreeMap::new();
                    for run in runs {
                        if run.state != "superseded" {
                            tasks.entry(run.task_id.clone()).or_default().push(run);
                        }
                    }
                    for runs in tasks.values() {
                        if let Err(error) = delegation::review::task(agent, &store, runs).await {
                            tracing::warn!(task_id=%runs[0].task_id,error=%error,"coordinator review incomplete");
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    tokio::spawn(async move {
        let mut active: std::collections::HashMap<String, tokio::task::JoinHandle<()>> =
            std::collections::HashMap::new();
        let unacknowledged = Unacknowledged::default();
        loop {
            if let Ok(store) = store(&h) {
                if let Ok(runs) = store.list() {
                    // A run that no longer exists can never acknowledge anything.
                    unacknowledged
                        .lock()
                        .unwrap()
                        .retain(|id, _| runs.iter().any(|run| &run.id == id));
                    for run in &runs {
                        if active.get(&run.id).is_some_and(|task| !task.is_finished()) {
                            continue;
                        }
                        let (h, store, run, runs, unacknowledged) = (
                            h.clone(),
                            store.clone(),
                            run.clone(),
                            runs.clone(),
                            unacknowledged.clone(),
                        );
                        active.insert(
                            run.id.clone(),
                            tokio::spawn(async move {
                                if let Err(error) =
                                    sync_run(&h, &store, &run, &runs, &unacknowledged).await
                                {
                                    let _ =
                                        store.state(&run.id, "disconnected", &error.to_string());
                                    // Holding the task open keeps this run out of the
                                    // loop, so an unreachable or overloaded worker is
                                    // not hit with a fresh SSH session every 3s.
                                    tokio::time::sleep(SYNC_RETRY_BACKOFF).await;
                                }
                            }),
                        );
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
}

/// Copies runtime evidence into a device-agent record; returns whether it changed.
fn apply_runtime_evidence(attrs: &mut Value, metadata: &Value) -> bool {
    let before = attrs.clone();
    if !metadata["invocation"].is_null() {
        attrs["invocation"] = metadata["invocation"].clone();
    }
    if metadata["available_models"].is_array() {
        attrs["models"] = metadata["available_models"].clone();
    }
    // The latest provider usage the run saw is the placement's usage snapshot.
    if metadata["usage"].is_object() {
        attrs["usage"] = metadata["usage"].clone();
    }
    *attrs != before
}

/// A completed run only changes again once Hive has something to deliver to
/// it: a coordinator follow-up, a user message or a decision. Polling it anyway
/// costs an SSH round trip and a remote runner process every loop.
///
/// A run whose agent session has ended can't change at all: its journal still
/// reports the last state, so syncing it would only revive that stale state.
fn idle(
    store: &RunStore,
    run: &Run,
    unacknowledged: &Unacknowledged,
) -> anyhow::Result<bool> {
    if run.state == "disconnected" && run.metadata["reason"] == SESSION_ENDED {
        return Ok(true);
    }
    let delivered = unacknowledged.lock().unwrap();
    Ok(run.state == "completed"
        && store.pending_decisions(&run.id)?.is_empty()
        && !store.has_pending_messages(&run.id)?
        && delivered.get(&run.id).is_none_or(|ids| ids.is_empty()))
}

/// Delivered message IDs per run whose journal acknowledgment is not synced yet.
///
/// Handing a message to a runner only queues it in the remote inbox. The turn it
/// provokes emits an `acknowledgment` event carrying that message ID, and the
/// state it finishes in arrives after it. A completed run that received a
/// message therefore still has events to import, and is not idle until its
/// journal has confirmed every message Hive delivered.
type Unacknowledged =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>>;

/// Remembers a delivered message so its run keeps syncing until the journal
/// acknowledges it.
fn deliver(unacknowledged: &Unacknowledged, run: &str, message: &str) {
    let mut unacknowledged = unacknowledged.lock().unwrap();
    let delivered = unacknowledged.entry(run.to_string()).or_default();
    if !delivered.iter().any(|id| id == message) {
        delivered.push(message.to_string());
    }
}

/// Retires the delivered messages that a synced snapshot acknowledges. Only the
/// exact message IDs count, so an unrelated event never ends a revival.
fn acknowledge(unacknowledged: &Unacknowledged, run: &str, snapshot: &Value) {
    let acknowledged: std::collections::HashSet<&str> = snapshot["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["kind"] == "acknowledgment")
        .filter_map(|event| event["payload"]["message_id"].as_str())
        .collect();
    if acknowledged.is_empty() {
        return;
    }
    let mut unacknowledged = unacknowledged.lock().unwrap();
    if let Some(delivered) = unacknowledged.get_mut(run) {
        delivered.retain(|id| !acknowledged.contains(id.as_str()));
        if delivered.is_empty() {
            unacknowledged.remove(run);
        }
    }
}

const SESSION_ENDED: &str = "The agent's session has ended, so this run can't continue.";

/// A dependency releases its dependent when it completes or waits for a peer
/// with a pending message addressed to that dependent. Otherwise an implementer
/// asking its queued verifier a question could wait forever for its own finish.
/// Each prerequisite is checked independently; a message cannot bypass another
/// working or failed prerequisite. `messages` is the dependent's durable inbox.
///
/// A dependency paused on its provider quota has not failed: it resumes by
/// itself after the reset, so its dependents stay queued. If it already handed
/// its work over, a message naming a branch or commit, they start now.
fn dependency_ready(runs: &[Run], run: &Run, messages: &[Value]) -> Result<bool, String> {
    let mut ready = true;
    for key in &run.assignment.dependencies {
        let mut completed = false;
        let mut failed = false;
        let mut waiting_for_us = false;
        for dependency in runs.iter().filter(|r| {
            r.task_id == run.task_id && &r.assignment.key == key && r.state != "superseded"
        }) {
            let from_dependency = |message: &&Value| message["source"] == dependency.id;
            completed |= dependency.state == "completed";
            failed |= dependency.state == "failed";
            waiting_for_us |= dependency.state == "waiting-for-peer"
                && messages.iter().any(|message| from_dependency(&message));
            waiting_for_us |= dependency.state == "paused-quota"
                && messages
                    .iter()
                    .filter(from_dependency)
                    .any(|message| {
                        delegation::names_handoff(message["text"].as_str().unwrap_or(""))
                    });
        }
        if completed {
            continue;
        }
        if failed {
            return Err(format!("dependency {key} failed and can never complete"));
        }
        ready &= waiting_for_us;
    }
    Ok(ready)
}

/// States in which a launched run's tmux session must still exist.
const LIVE_STATES: [&str; 5] =
    ["working", "awaiting-approval", "waiting-for-peer", "reviewing", "paused-quota"];

/// Whether the run's tmux session exists. `None` when the device couldn't be
/// asked: an unreachable machine says nothing about the session.
async fn session_alive(worker: &hive_common::protocol::WorkerInfo, run: &Run) -> Option<bool> {
    let command = format!(
        "tmux has-session -t {} 2>/dev/null && echo alive || echo gone",
        transport::quote(&format!("={}", run.tmux_name))
    );
    match transport::ssh(worker, &command, None).await.ok()?.trim() {
        "alive" => Some(true),
        "gone" => Some(false),
        _ => None,
    }
}

async fn sync_run(
    h: &AgentHandle,
    store: &RunStore,
    run: &Run,
    runs: &[Run],
    unacknowledged: &Unacknowledged,
) -> anyhow::Result<()> {
    if idle(store, run, unacknowledged)? {
        return Ok(());
    }
    let agent = h.agent.as_ref().unwrap();
    let worker = delegation::target(agent, &run.assignment.device)
        .ok_or_else(|| anyhow::anyhow!("Device removed from configured fleet"))?;
    if run.state == "superseded" {
        if run.runner_path.is_some() {
            let _ = transport::ssh(
                &worker,
                &format!(
                    "tmux kill-session -t {} 2>/dev/null || true",
                    transport::quote(&format!("={}", run.tmux_name))
                ),
                None,
            )
            .await;
        }
        return Ok(());
    }
    // Checked before the snapshot, which would rewrite the state from the
    // journal of a runner that no longer exists.
    if run.runner_path.is_some()
        && LIVE_STATES.contains(&run.state.as_str())
        && session_alive(&worker, run).await == Some(false)
    {
        store.state(&run.id, "disconnected", SESSION_ENDED)?;
        return Ok(());
    }
    if run.state == "queued" {
        match dependency_ready(runs, run, &store.pending_messages(&run.id)?) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(reason) => {
                store.state(&run.id, "failed", &reason)?;
                return Ok(());
            }
        }
        // Launching now would only pause at once; the run starts after the reset.
        if let Some(until) = delegation::placement_quota(agent, &run.assignment)? {
            let reason = format!(
                "{}: {} {}",
                run.assignment.device,
                run.assignment.agent,
                delegation::quota_note(until)
            );
            if run.metadata["reason"] != reason.as_str() {
                store.state(&run.id, "queued", &reason)?;
            }
            return Ok(());
        }
        let record = agent
            .memory
            .graph
            .entity(&hive_core::memory::graph::entity_id(
                "device-agent",
                &format!("{}/{}", run.assignment.device, run.assignment.agent),
            ))?;
        if let Some(record) = record {
            if !record.attrs["executable"].is_string()
                || record.attrs["authentication"] == "login-required"
            {
                store.state(
                    &run.id,
                    "needs-setup",
                    &format!(
                        "{}: {} needs installation or login",
                        run.assignment.device, run.assignment.agent
                    ),
                )?;
                return Ok(());
            }
        }
        let runner = transport::deploy(&worker).await?;
        // Isolated SDK install is a routine prerequisite, never a permission bypass.
        if run.assignment.agent == "claude" {
            let directory = std::path::Path::new(&runner)
                .parent()
                .unwrap()
                .to_string_lossy();
            let command=format!("cd {} && if python3 -c 'import sys; assert sys.version_info >= (3,10)' 2>/dev/null; then test -f .sdk/bin/python || python3 -m venv .sdk; env PIP_CONFIG_FILE=/dev/null PIP_EXTRA_INDEX_URL= .sdk/bin/python -m pip install --index-url https://pypi.org/simple --timeout 15 --retries 1 --disable-pip-version-check claude-agent-sdk==0.2.152; else test -d node_modules/@anthropic-ai/claude-agent-sdk || npm install --ignore-scripts --no-audit --no-fund --save-exact @anthropic-ai/claude-agent-sdk@0.3.268; fi",transport::quote(&directory));
            if let Err(error) = transport::ssh_timeout(&worker, &command, None, 180).await {
                store.state(
                    &run.id,
                    "needs-setup",
                    &format!(
                        "{}: Claude SDK runtime setup failed: {error}",
                        run.assignment.device
                    ),
                )?;
                return Ok(());
            }
        }
        let raw = transport::ssh_timeout(
            &worker,
            &format!("python3 {} probe", transport::quote(&runner)),
            None,
            inventory::PROBE_SECONDS,
        )
        .await?;
        inventory::project(
            &agent.memory.graph,
            &worker.name,
            &serde_json::from_str::<Vec<Value>>(&raw)?,
        )?;
        if let Some(reason) = delegation::setup_reason(agent, &run.assignment)? {
            store.state(&run.id, "needs-setup", &reason)?;
            return Ok(());
        }
        if !store.claim(&run.id, &runner)? {
            return Ok(());
        }
        let entity = agent
            .memory
            .graph
            .entity(&hive_core::memory::graph::entity_id(
                "device-agent",
                &format!("{}/{}", run.assignment.device, run.assignment.agent),
            ))?;
        let executable = entity.as_ref().and_then(|e| e.attrs["executable"].as_str());
        let peers = runs
            .iter()
            .filter(|r| r.task_id == run.task_id && r.state != "superseded")
            .cloned()
            .collect::<Vec<_>>();
        let assignment = delegation::remote_assignment(run, &peers, executable);
        transport::control(
            &worker,
            &runner,
            "launch",
            &run.id,
            0,
            Some(&assignment),
        )
        .await?;
        return Ok(());
    }
    let Some(runner) = &run.runner_path else {
        return Ok(());
    };
    let raw =
        transport::control(&worker, runner, "snapshot", &run.id, run.cursor, None).await?;
    let mut snapshot: Value = serde_json::from_str(&raw)?;
    enrich_approvals(&mut snapshot, &store.recent_events(&run.id, 1000)?);
    let peers = delegation::team_view(run, runs);
    if snapshot["metadata"]["assignment"].is_object()
        && snapshot["metadata"]["assignment"]["peers"] != peers
    {
        transport::update_peers(&worker, &run.id, &peers).await?;
        snapshot["metadata"]["assignment"]["peers"] = peers;
    }

    sync_peer_snapshot(store, run, runs, &mut snapshot)?;
    // The journal's report is authoritative, so a message it acknowledged in
    // this snapshot no longer needs a run that keeps syncing for it.
    acknowledge(unacknowledged, &run.id, &snapshot);
    // Re-evaluate pending actions with the current deterministic policy. This
    // permits routine controls when an older persistent runner asked too broadly.
    // The policy only inspects arguments and paths; it never executes the action.
    let saved_decisions = store.pending_decisions(&run.id)?;
    for approval in snapshot["approvals"].as_array().into_iter().flatten() {
        if saved_decisions
            .iter()
            .any(|decision| decision["id"] == approval["id"])
        {
            continue;
        }
        if !approval["decision"].is_null() || approval["consumed"] != 0 {
            continue;
        }
        let mut action: Value = serde_json::from_str(approval["action"].as_str().unwrap_or("{}"))?;
        if approval["details"]["changes"].is_array() {
            action["arguments"]["changes"] = approval["details"]["changes"].clone();
        }
        let source = format!(
            "__file__ = {}\n{}",
            serde_json::to_string(runner)?,
            transport::RUNNER
        );
        let raw = transport::ssh(
            &worker,
            &format!("python3 -c {} assess", transport::quote(&source)),
            Some(&action),
        )
        .await?;
        let assessment: Value = serde_json::from_str(&raw)?;
        if assessment["reason"].is_null() {
            store.decide(
                &run.id,
                approval["id"].as_str().unwrap(),
                approval["fingerprint"].as_str().unwrap(),
                "continue",
            )?;
        }
    }
    for decision in store.pending_decisions(&run.id)? {
        transport::control(&worker, runner, "decide", &run.id, 0, Some(&decision)).await?;
        store.decision_delivered(&run.id, decision["id"].as_str().unwrap())?;
    }
    while let Some(delivery) = store.next_delivery(&run.id)? {
        if let Err(error) = transport::control(&worker, runner, "enqueue", &run.id, 0, Some(&delivery.payload)).await {
            store.delivery_failed(&delivery)?;
            return Err(error);
        }
        store.message_delivered(&delivery)?;
        deliver(unacknowledged, &run.id, &delivery.id);
    }
    // Only a completed native invocation proves a model works; the runtime's
    // own catalog is recorded even when that first call failed, so the user
    // can explicitly pick another listed model.
    let metadata = &snapshot["metadata"];
    if !metadata["invocation"].is_null()
        || metadata["available_models"].is_array()
        || metadata["usage"].is_object()
    {
        let id = hive_core::memory::graph::entity_id(
            "device-agent",
            &format!("{}/{}", run.assignment.device, run.assignment.agent),
        );
        if let Some(mut record) = agent.memory.graph.entity(&id)? {
            if apply_runtime_evidence(&mut record.attrs, metadata) {
                agent.memory.graph.upsert_entity(&record)?;
            }
        }
    }
    Ok(())
}

/// Import outgoing messages before advancing the journal cursor, and attach a
/// coordinator reason to every waiting snapshot. The durable inbox, rather than
/// just this page of new events, keeps the reason visible across sync cycles.
fn sync_peer_snapshot(
    store: &RunStore,
    run: &Run,
    runs: &[Run],
    snapshot: &mut Value,
) -> anyhow::Result<()> {
    for event in snapshot["events"].as_array().into_iter().flatten() {
        if event["kind"] == "peer" {
            let to = event["payload"]["to"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Peer destination missing"))?;
            let id = event["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Peer event ID missing"))?;
            let kind = event["payload"]["kind"].as_str().unwrap_or("message");
            let text = event["payload"]["text"].as_str().unwrap_or("");
            let agreement = if kind == "agreement" {
                Some(match store.record_agreement(&run.id, to, text) {
                    Ok(digest) => format!(" HACP v2 contract digest: {digest}. Echo this digest in an agreement message to accept."),
                    Err(error) => format!(" HACP v2 rejected this unilateral change: {error}."),
                })
            } else { None };
            let payload = json!({"id":id,"source":run.id,"kind":kind,"text":format!("Peer {} on {} ({kind}) says: {}{}",run.assignment.key,run.assignment.device,text,agreement.as_deref().unwrap_or(""))});
            if runs.iter().any(|r| r.id == to && r.state != "superseded") {
                // A rejected event (changed envelope under a reused ID, or a
                // forbidden route) is recorded once as an incident; skipping it
                // lets the cursor advance instead of retrying it forever.
                if let Err(error) = store.message(id, &run.id, to, &payload) {
                    if !error.is::<MessageRejected>() {
                        return Err(error);
                    }
                    tracing::warn!(run=%run.id, message=%id, error=%error, "peer event rejected");
                }
            }
        }
    }
    if snapshot["metadata"]["state"] == "waiting-for-peer" {
        let mut queued_behind = std::collections::HashSet::from([run.assignment.key.as_str()]);
        loop {
            let before = queued_behind.len();
            for peer in runs
                .iter()
                .filter(|peer| peer.task_id == run.task_id && peer.state == "queued")
            {
                if peer.assignment.dependencies.iter().any(|key| queued_behind.contains(key.as_str())) {
                    queued_behind.insert(peer.assignment.key.as_str());
                }
            }
            if queued_behind.len() == before {
                break;
            }
        }
        for peer in runs.iter().filter(|peer| {
            peer.task_id == run.task_id
                && peer.state == "queued"
                && queued_behind.contains(peer.assignment.key.as_str())
        }) {
            if store
                .pending_messages(&peer.id)?
                .iter()
                .any(|message| message["source"] == run.id)
            {
                snapshot["metadata"]["reason"] = json!(format!(
                    "waiting for {}, which is queued behind this run",
                    peer.assignment.key
                ));
                break;
            }
        }
    }
    store.sync(&run.id, snapshot)
}

// Codex approval requests reference a previously emitted file-change item.
// Retain its exact diff for review without altering the native grant fingerprint.
fn enrich_approvals(snapshot: &mut Value, stored: &[Value]) {
    let events: Vec<Value> = stored
        .iter()
        .chain(snapshot["events"].as_array().into_iter().flatten())
        .cloned()
        .collect();
    for approval in snapshot["approvals"].as_array_mut().into_iter().flatten() {
        let Ok(action) = serde_json::from_str::<Value>(approval["action"].as_str().unwrap_or(""))
        else {
            continue;
        };
        if action["tool"] != "item/fileChange/requestApproval" {
            continue;
        }
        let item_id = &action["arguments"]["itemId"];
        for event in events.iter().rev() {
            let native = &event["payload"];
            if event["kind"] == "native"
                && native["method"] == "item/started"
                && &native["params"]["item"]["id"] == item_id
                && native["params"]["item"]["changes"].is_array()
            {
                approval["details"] = native["params"]["item"].clone();
                break;
            }
        }
    }
}

#[cfg(test)]
#[path = "delegation_tests.rs"]
mod peer_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> AgentHandle {
        let agent = hive_core::agent::MasterAgent::new(
            hive_core::llm::LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            hive_core::workers::WorkerPool::new(vec![]),
            hive_core::skills::SkillRegistry::new(),
            hive_core::memory::MemorySystem::new(),
        );
        AgentHandle {
            agent: Some(std::sync::Arc::new(agent)),
            history: None,
            master_name: "master".into(),
        }
    }

    fn handle_with_history() -> AgentHandle {
        let agent = std::sync::Arc::new(hive_core::agent::MasterAgent::new(
            hive_core::llm::LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            hive_core::workers::WorkerPool::new(vec![]),
            hive_core::skills::SkillRegistry::new(),
            hive_core::memory::MemorySystem::new(),
        ));
        AgentHandle::enabled(agent, "master".into()).unwrap()
    }

    #[tokio::test]
    async fn planning_deadline_failure_preserves_504_status() {
        let h = handle_with_history();
        let history = h.history.as_ref().unwrap();
        let chat = history.create(None).unwrap();
        let start_turn = history.begin(&chat.id, "req-1", "test prompt").unwrap();
        let turn = match start_turn {
            hive_core::memory::chats::StartTurn::New(t) => t,
            _ => unreachable!(),
        };
        let slots = tokio::sync::Semaphore::new(2);
        let response = process_with_deadline_slots(
            h.clone(),
            turn.clone(),
            std::time::Duration::from_millis(20),
            &slots,
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), 8192).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("Planning timed out"), "{text}");
        assert!(text.contains("retry also timed out"), "{text}");

        let saved = history.turn(&turn.id).unwrap().unwrap();
        assert_eq!(saved.status, "failed");
    }
    #[tokio::test]
    async fn team_endpoint_and_prompt_are_scoped_public_projections() {
        let h = handle();
        let id = run_with_events(&h, 1);
        let store = store(&h).unwrap();
        store.sync(&id, &json!({"metadata":{"state":"working", "secret":"secret-sentinel", "transcript":"transcript-sentinel", "last_seen":"untrusted"}, "events":[], "approvals":[]})).unwrap();
        let run = store.get(&id).unwrap();
        let mut peer = run.clone();
        peer.id = "peer".into();
        peer.assignment.key = "backend".into();
        peer.assignment.owned_paths = vec!["src/**".into()];
        peer.assignment.dependencies = vec!["frontend".into()];
        peer.metadata = json!({"secret":"never expose", "transcript":"private", "last_seen":"later"});
        let view = delegation::team_view(&run, &[run.clone(), peer.clone()]);
        assert_eq!(view[0]["role"], "backend");
        assert_eq!(view[0]["owned_paths"], json!(["src/**"]));
        assert_eq!(view[0]["dependencies"], json!(["frontend"]));
        assert_eq!(view[0]["status"], "working");
        assert_eq!(delegation::remote_assignment(&run, &[peer.clone()], None)["peers"], view);
        peer.metadata["last_seen"] = json!("new heartbeat");
        assert_eq!(delegation::team_view(&run, &[peer.clone()]), view);
        peer.state = "completed".into();
        assert_ne!(delegation::team_view(&run, &[peer.clone()]), view);
        peer.state = "superseded".into();
        assert_eq!(delegation::team_view(&run, &[peer.clone()]), json!([]));
        peer.state = "working".into();
        peer.task_id = "another-task".into();
        assert_eq!(delegation::team_view(&run, &[peer]), json!([]));
        assert!(!view.to_string().contains("private"));
        assert!(!view.to_string().contains("secret"));
        let response = team(State(h.clone()), Path(run.task_id.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let profiles: Vec<Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(profiles.len(), 1);
        let profile = &profiles[0];
        let mut expected = vec!["agent_id","key","role","agent","model","device","owned_paths","current_task","status","dependencies","last_seen","relay_fingerprint"];
        expected.sort();
        assert_eq!(profile.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(), expected);
        assert_eq!(profile["agent_id"], id);
        assert_eq!(profile["relay_fingerprint"], run.identity.fingerprint);
        assert!(profile["last_seen"].as_str().is_some());
        assert_ne!(profile["last_seen"], "untrusted");
        assert!(!profile.to_string().contains("secret-sentinel"));
        assert!(!profile.to_string().contains("transcript-sentinel"));
        let response = team(State(h), Path("other-task".into())).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!([]));
    }

    #[tokio::test]
    async fn relay_audit_api_contains_only_public_evidence() {
        let h = handle();
        let id = run_with_events(&h, 0);
        let store = store(&h).unwrap();
        store.message("audit-test", "user", &id, &json!({"id":"audit-test","text":"hello"})).unwrap();
        let response = audit(State(h.clone()), Path(id.clone()), Query(AuditQuery::default())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["chain_valid"], true);
        assert_eq!(value["entries"][0]["record"]["event"], "stage");
        let raw = String::from_utf8(body.to_vec()).unwrap();
        assert!(!raw.contains("seed"));
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("hello"));
        assert!(raw.contains("public_key"));
        let full = audit(State(h.clone()), Path(id.clone()), Query(AuditQuery { full: Some("1".into()) })).await;
        let body = axum::body::to_bytes(full.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["verification"], "full");
        assert_eq!(value["chain_valid"], true);
        assert_eq!(audit(State(h), Path("unknown".into()), Query(AuditQuery::default())).await.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn run_list_filters_by_state_and_task() {
        let h = handle();
        let id = run_with_events(&h, 0);
        store(&h).unwrap().state(&id, "awaiting-approval", "").unwrap();
        let ids = |q: RunQuery| {
            let h = h.clone();
            async move {
                let response = list(State(h), Query(q)).await;
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                serde_json::from_slice::<Vec<Value>>(&body).unwrap().len()
            }
        };
        let q = |state: Option<&str>, task_of: Option<&str>| RunQuery {
            state: state.map(Into::into),
            task_of: task_of.map(Into::into),
            ..Default::default()
        };
        assert_eq!(ids(q(Some("needs-setup,awaiting-approval"), None)).await, 1);
        assert_eq!(ids(q(Some("working"), None)).await, 0);
        assert_eq!(ids(q(None, Some(&id))).await, 1);
        assert_eq!(ids(q(None, Some("unknown-run"))).await, 0);
    }

    #[tokio::test]
    async fn a_container_that_cant_be_created_starts_no_agents() {
        let h = handle();
        let agent = h.agent.as_ref().unwrap();
        let store = store(&h).unwrap();
        let mut plan: delegation::DelegationPlan = serde_json::from_value(json!({"summary":"work","assignments":[
            {"key":"a","device":"box","agent":"codex","model":null,"workspace":"~/hive-workspaces/t","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap();
        plan.containers = vec![delegation::NewContainer { name: "box".into(), host: "master".into() }];
        let err = start_plan(agent, &store, "task", "chat", &plan).await.unwrap_err().to_string();
        assert!(err.starts_with("Could not create container box on "), "{err}");
        assert!(err.ends_with("No agents were started."), "{err}");
        assert!(store.list().unwrap().is_empty());
        // A plan without containers creates its runs as before.
        plan.containers.clear();
        let reply = start_plan(agent, &store, "task", "chat", &plan).await.unwrap();
        assert_eq!(reply["delegation"]["containers_created"], json!([]));
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_container_is_unknown_not_gone() {
        // No such container (or no Docker at all): docker exec fails, which
        // says nothing about the agent's session inside it.
        let h = handle();
        let run = store(&h).unwrap().get(&run_with_events(&h, 0)).unwrap();
        let worker = hive_common::protocol::WorkerInfo {
            name: "dev-box".into(),
            host: "localhost".into(),
            user: "u".into(),
            port: None,
            tags: vec![],
            local: true,
            container: Some(format!("hive-test-missing-{}", uuid::Uuid::new_v4().simple())),
        };
        assert_eq!(session_alive(&worker, &run).await, None);
    }

    #[test]
    fn runs_whose_session_ended_are_never_synced_again() {
        let h = handle();
        let store = store(&h).unwrap();
        let unacknowledged = Unacknowledged::default();
        let id = run_with_events(&h, 0);
        store.state(&id, "awaiting-approval", "").unwrap();
        assert!(!idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap());
        // A runner-reported disconnect can still be reconciled, so keep syncing.
        store.state(&id, "disconnected", "Prior runner exited").unwrap();
        assert!(!idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap());
        store.state(&id, "disconnected", SESSION_ENDED).unwrap();
        assert!(idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap());
    }
    fn run_with_events(h: &AgentHandle, count: i64) -> String {
        let store = store(h).unwrap();
        let plan = serde_json::from_value(json!({"summary":"work","assignments":[{"key":"a","device":"air","agent":"codex","model":null,"workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap();
        let id = store.create("task", "chat", &plan).unwrap().remove(0).id;
        let events: Vec<Value> = (1..=count)
            .map(|seq| json!({"id":format!("e{seq}"),"seq":seq,"kind":"native","payload":{"seq":seq}}))
            .collect();
        store
            .sync(&id, &json!({"metadata":{"state":"working"},"events":events,"approvals":[]}))
            .unwrap();
        id
    }
    async fn fetch(h: &AgentHandle, id: &str, q: RunQuery) -> (StatusCode, Value) {
        let response = events(State(h.clone()), Path(id.to_string()), Query(q)).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
    fn seqs(v: &Value) -> Vec<i64> {
        v.as_array().unwrap().iter().map(|e| e["seq"].as_i64().unwrap()).collect()
    }

    #[tokio::test]
    async fn events_tail_opens_a_long_session_at_its_latest_activity() {
        let h = handle();
        let id = run_with_events(&h, 450);
        let (status, body) = fetch(&h, &id, RunQuery { tail: Some(5), ..Default::default() }).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(seqs(&body), vec![446, 447, 448, 449, 450]);
        // Without tail the API still pages forward from `after`, 300 at a time.
        let (_, first) = fetch(&h, &id, RunQuery::default()).await;
        assert_eq!(seqs(&first).len(), 300);
        assert_eq!(seqs(&first)[0], 1);
        let (_, next) = fetch(&h, &id, RunQuery { after: 300, ..Default::default() }).await;
        assert_eq!(seqs(&next), (301..=450).collect::<Vec<_>>());
        // A tail larger than the history returns all of it, in order.
        let (_, all) = fetch(&h, &id, RunQuery { tail: Some(1000), ..Default::default() }).await;
        assert_eq!(seqs(&all), (1..=450).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn events_for_an_unknown_run_are_refused() {
        let h = handle();
        let (status, _) = fetch(&h, "missing", RunQuery { tail: Some(5), ..Default::default() }).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = fetch(&AgentHandle::disabled(), "missing", RunQuery::default()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn runtime_catalog_is_kept_even_when_the_invocation_failed() {
        let mut attrs = json!({"models":[],"invocation":null});
        let failed = json!({"invocation":null,"available_models":["zai/glm-5.2","zai/glm-5.3"]});
        assert!(apply_runtime_evidence(&mut attrs, &failed));
        assert_eq!(attrs["models"], json!(["zai/glm-5.2","zai/glm-5.3"]));
        assert!(attrs["invocation"].is_null());
        assert!(!apply_runtime_evidence(&mut attrs, &failed));
        let succeeded = json!({"invocation":{"model":"zai/glm-5.2"},"available_models":["zai/glm-5.2","zai/glm-5.3"]});
        assert!(apply_runtime_evidence(&mut attrs, &succeeded));
        assert_eq!(attrs["invocation"]["model"], "zai/glm-5.2");
        assert!(!apply_runtime_evidence(&mut attrs, &json!({"invocation":null})));
        assert_eq!(attrs["invocation"]["model"], "zai/glm-5.2");
    }

    #[test]
    fn placement_inventory_records_the_latest_usage_snapshot() {
        let mut attrs = json!({"models":["gpt-5.5"],"invocation":null});
        let first = json!({"usage":{"agent":"codex","used_percent":82,"resets_at":1_790_000_000,"exhausted":false}});
        assert!(apply_runtime_evidence(&mut attrs, &first));
        assert_eq!(attrs["usage"]["used_percent"], 82);
        let exhausted = json!({"usage":{"agent":"codex","used_percent":100,"resets_at":1_790_003_600,"exhausted":true}});
        assert!(apply_runtime_evidence(&mut attrs, &exhausted));
        assert_eq!((attrs["usage"]["used_percent"].as_i64(), attrs["usage"]["resets_at"].as_i64()), (Some(100), Some(1_790_003_600)));
        assert_eq!(delegation::quota_exhausted_until(&attrs, 1_790_000_000), Some(1_790_003_600));
        // A snapshot without usage keeps the last one.
        assert!(!apply_runtime_evidence(&mut attrs, &json!({"invocation":null})));
        assert_eq!(attrs["usage"]["exhausted"], true);
        assert_eq!(attrs["models"], json!(["gpt-5.5"]));
    }

    #[test]
    fn completed_runs_are_polled_only_when_something_is_pending() {
        let path = std::env::temp_dir().join(format!("hive-idle-{}.db", uuid::Uuid::new_v4()));
        let graph = hive_core::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let unacknowledged = Unacknowledged::default();
        let plan = serde_json::from_value(json!({"summary":"work","assignments":[{"key":"a","device":"air","agent":"claude","model":null,"workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap();
        let id = store.create("task", "chat", &plan).unwrap().remove(0).id;
        assert!(!idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap());
        store.state(&id, "completed", "").unwrap();
        // Nothing queued and nothing delivered: the journal cannot change, so
        // the SSH round trip and remote runner process are skipped.
        assert!(idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap());
        store
            .message("follow-up", "user", &id, &json!({"id":"follow-up","text":"verify"}))
            .unwrap();
        assert!(!idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap());
        let delivery = store.next_delivery(&id).unwrap().unwrap();
        store.message_delivered(&delivery).unwrap();
        drop(store);
        drop(graph);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_revived_completed_run_is_synced_until_its_message_is_acknowledged() {
        let h = handle();
        let store = store(&h).unwrap();
        let unacknowledged = Unacknowledged::default();
        let id = run_with_events(&h, 0);
        store.state(&id, "completed", "").unwrap();
        let wake = |message: &str| {
            store
                .message(message, "user", &id, &json!({"id":message,"text":"verify"}))
                .unwrap();
        };
        let idle_now = || idle(&store, &store.get(&id).unwrap(), &unacknowledged).unwrap();
        assert!(idle_now());
        // A follow-up wakes the finished run, and delivering it only queues a
        // turn in the remote inbox.
        wake("first");
        assert!(!idle_now());
        let delivery = store.next_delivery(&id).unwrap().unwrap();
        store.message_delivered(&delivery).unwrap();
        deliver(&unacknowledged, &id, "first");
        // The journal has not reported that turn yet, so its acknowledgment and
        // the state it finishes in are still to be imported.
        assert!(!idle_now());
        // A second message revives the run again, and each is tracked on its own.
        wake("second");
        assert!(!idle_now());
        let delivery = store.next_delivery(&id).unwrap().unwrap();
        store.message_delivered(&delivery).unwrap();
        deliver(&unacknowledged, &id, "second");
        // Events that acknowledge nothing, and other messages' acknowledgments,
        // leave the run waiting.
        for event in [
            json!({"kind":"output","payload":{"text":"working on it"}}),
            json!({"kind":"state","payload":{"state":"working"}}),
            json!({"kind":"acknowledgment","payload":{"message_id":"initial"}}),
        ] {
            acknowledge(&unacknowledged, &id, &json!({"events":[event]}));
        }
        assert!(!idle_now());
        // Syncing the acknowledgment and the later state retires it, and the
        // optimization for truly idle completed runs applies again.
        let snapshot = json!({"metadata":{"state":"completed"},"approvals":[],"events":[
            {"id":"ack-1","seq":1,"kind":"acknowledgment","payload":{"message_id":"first"}},
            {"id":"state-1","seq":2,"kind":"state","payload":{"state":"working"}},
            {"id":"ack-2","seq":3,"kind":"acknowledgment","payload":{"message_id":"second"}},
            {"id":"state-2","seq":4,"kind":"state","payload":{"state":"completed"}}]});
        store.sync(&id, &snapshot).unwrap();
        acknowledge(&unacknowledged, &id, &snapshot);
        assert_eq!(store.events(&id, 0).unwrap().len(), 4);
        assert!(idle_now());
    }

    #[test]
    fn dependents_never_start_on_a_failed_dependency() {
        let h = handle();
        let store = store(&h).unwrap();
        let plan = serde_json::from_value(json!({"summary":"work","assignments":[
            {"key":"a","device":"air","agent":"opencode","model":"zai-coding-plan/glm-5.3","workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]},
            {"key":"b","device":"air","agent":"claude","model":null,"workspace":"~/hive-workspaces/review","objective":"review","dependencies":["a"],"acceptance_criteria":["verified"]}]})).unwrap();
        let runs = store.create("task", "chat", &plan).unwrap();
        let (a, b) = (&runs[0], &runs[1]);
        // While the dependency works the dependent just waits.
        store.sync(&a.id, &json!({"metadata":{"state":"working"},"events":[],"approvals":[]})).unwrap();
        assert_eq!(dependency_ready(&store.list().unwrap(), b, &[]), Ok(false));
        // A failed dependency (e.g. 'turn produced no actions') never releases it.
        store.sync(&a.id, &json!({"metadata":{"state":"failed"},"events":[],"approvals":[]})).unwrap();
        let runs = store.list().unwrap();
        let b = runs.iter().find(|r| r.id == b.id).unwrap();
        match dependency_ready(&runs, b, &[]) {
            Err(reason) => assert!(reason.contains("dependency a failed"), "{reason}"),
            other => panic!("expected failure reason, got {other:?}"),
        }
        // Without a peer message, only completion releases the dependent.
        store.state(&a.id, "completed", "").unwrap();
        let runs = store.list().unwrap();
        assert_eq!(dependency_ready(&runs, runs.iter().find(|r| r.id == b.id).unwrap(), &[]), Ok(true));
        // An unknown or superseded dependency keeps waiting, never starts.
        let mut orphan = plan.clone();
        orphan.assignments[1].dependencies = vec!["missing".into()];
        let superseded = store.create("task2", "chat", &orphan).unwrap();
        assert_eq!(dependency_ready(&superseded, &superseded[1], &[]), Ok(false));
    }

    #[test]
    fn approval_details_match_native_item_without_changing_grant() {
        let action =
            json!({"tool":"item/fileChange/requestApproval","arguments":{"itemId":"change-a"}})
                .to_string();
        let mut snapshot = json!({"approvals":[{"id":"pending","fingerprint":"original","action":action}],"events":[]});
        let events = vec![
            json!({"kind":"native","payload":{"method":"item/started","params":{"item":{"id":"change-a","changes":[{"path":"/workspace/app.py","diff":"+print(1)"}]}}}}),
            json!({"kind":"native","payload":{"method":"item/started","params":{"item":{"id":"other","changes":[{"path":"/outside"}]}}}}),
        ];
        enrich_approvals(&mut snapshot, &events);
        assert_eq!(
            snapshot["approvals"][0]["details"]["changes"][0]["path"],
            "/workspace/app.py"
        );
        assert_eq!(snapshot["approvals"][0]["action"], action);
        assert_eq!(snapshot["approvals"][0]["fingerprint"], "original");
        let mut unknown = json!({"approvals":[{"action":json!({"tool":"item/fileChange/requestApproval","arguments":{"itemId":"absent"}}).to_string()}]});
        enrich_approvals(&mut unknown, &events);
        assert!(unknown["approvals"][0]["details"].is_null());
    }

    #[test]
    fn rejected_peer_event_is_one_incident_and_cursor_advances() {
        let h = handle();
        let store = store(&h).unwrap();
        let plan = serde_json::from_value(json!({"summary":"work","assignments":[
            {"key":"a","device":"air","agent":"codex","model":null,"workspace":"~/hive-workspaces/a","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]},
            {"key":"b","device":"air","agent":"codex","model":null,"workspace":"~/hive-workspaces/b","objective":"review","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap();
        let runs = store.create("task", "chat", &plan).unwrap();
        let (a, b) = (&runs[0], &runs[1]);
        let peer = |seq: i64, text: &str| json!({"metadata":{"state":"working"},"approvals":[],"events":[
            {"id":"peer-1","seq":seq,"kind":"peer","payload":{"to":b.id,"kind":"message","text":text}}]});
        sync_peer_snapshot(&store, a, &runs, &mut peer(1, "original")).unwrap();
        assert_eq!(store.get(&a.id).unwrap().cursor, 1);
        // The untrusted journal re-emits the same ID with different content.
        for attempt in 0..5 {
            sync_peer_snapshot(&store, a, &runs, &mut peer(2 + attempt, "changed")).unwrap();
        }
        let run = store.get(&a.id).unwrap();
        assert_eq!(run.cursor, 6, "cursor advances past the rejected event");
        assert_eq!(run.relay["incidents"].as_array().unwrap().len(), 1);
        assert_eq!(run.relay["incidents"][0]["message_id"], "peer-1");
        let rejects = store.audit_full(&a.id).unwrap()["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["record"]["event"] == "reject")
            .count();
        assert_eq!(rejects, 1);
        // The original, verified message still reaches its destination.
        let delivery = store.next_delivery(&b.id).unwrap().unwrap();
        assert!(delivery.payload["text"].as_str().unwrap().ends_with("says: original"));
    }
}
