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
        store::{Run, RunStore},
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

pub async fn process(h: AgentHandle, turn: SavedTurn) -> Response {
    let result=async {
        let agent=h.agent.as_ref().unwrap();
        let history=h.history.as_ref().unwrap();
        let context=history.context(&turn)?;
        let plan=tokio::time::timeout(std::time::Duration::from_secs(240),delegation::plan(agent,&turn.user_input,&context.join("\n"))).await??;
        let runs=store(&h)?.create(&turn.id,&turn.conversation_id,&plan)?;
        let reply=json!({"conversation_id":turn.conversation_id,"delegation":{"task_id":turn.id,"summary":plan.summary,"runs":runs}});
        // A detached run owns its own state. Finishing the receipt keeps this
        // conversation's composer available while the real agents work.
        history.finish(&turn.id,"completed",&plan.summary,Some(&reply))?;
        Ok::<_,anyhow::Error>(reply)
    }.await;
    match result {
        Ok(reply) => Json(reply).into_response(),
        Err(e) => {
            if let Some(history) = &h.history {
                let _ = history.finish(&turn.id, "failed", &e.to_string(), None);
            }
            error(e)
        }
    }
}

#[derive(Default, Deserialize)]
pub struct RunQuery {
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub after: i64,
    /// Latest N events in order, for opening a long session at its end.
    pub tail: Option<usize>,
}
pub async fn list(State(h): State<AgentHandle>, Query(q): Query<RunQuery>) -> Response {
    match store(&h).and_then(|s| s.list()) {
        Ok(runs) => Json(
            runs.into_iter()
                .filter(|r| {
                    q.conversation_id
                        .as_ref()
                        .is_none_or(|id| id == &r.conversation_id)
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
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
    let worker = agent
        .workers
        .find(&run.assignment.device)
        .ok_or_else(|| anyhow::anyhow!("Device removed from configured fleet"))?;
    let payload = request.map(|request| json!({"fingerprint":request.fingerprint,"reason":request.reason,"evidence":request.evidence,"acknowledge_ids":request.acknowledge_ids}));
    let result = transport::control(
        &worker.info,
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
                            tracing::warn!(error=%error,"coordinator review incomplete");
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
        loop {
            if let Ok(store) = store(&h) {
                if let Ok(runs) = store.list() {
                    for run in &runs {
                        if active.get(&run.id).is_some_and(|task| !task.is_finished()) {
                            continue;
                        }
                        let (h, store, run, runs) =
                            (h.clone(), store.clone(), run.clone(), runs.clone());
                        active.insert(
                            run.id.clone(),
                            tokio::spawn(async move {
                                if let Err(error) = sync_run(&h, &store, &run, &runs).await {
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
    *attrs != before
}

/// A completed run only changes again once Hive has something to deliver to
/// it: a coordinator follow-up, a user message or a decision. Polling it anyway
/// costs an SSH round trip and a remote runner process every loop.
fn idle(store: &RunStore, run: &Run) -> anyhow::Result<bool> {
    Ok(run.state == "completed"
        && store.pending_decisions(&run.id)?.is_empty()
        && store.pending_messages(&run.id)?.is_empty())
}

async fn sync_run(
    h: &AgentHandle,
    store: &RunStore,
    run: &Run,
    runs: &[Run],
) -> anyhow::Result<()> {
    if idle(store, run)? {
        return Ok(());
    }
    let agent = h.agent.as_ref().unwrap();
    let worker = agent
        .workers
        .find(&run.assignment.device)
        .ok_or_else(|| anyhow::anyhow!("Device removed from configured fleet"))?;
    if run.state == "superseded" {
        if run.runner_path.is_some() {
            let _ = transport::ssh(
                &worker.info,
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
    if run.state == "queued" {
        if !run.assignment.dependencies.iter().all(|key| {
            runs.iter().any(|r| {
                r.task_id == run.task_id && &r.assignment.key == key && r.state == "completed"
            })
        }) {
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
        let runner = transport::deploy(&worker.info).await?;
        // Isolated SDK install is a routine prerequisite, never a permission bypass.
        if run.assignment.agent == "claude" {
            let directory = std::path::Path::new(&runner)
                .parent()
                .unwrap()
                .to_string_lossy();
            let command=format!("cd {} && if python3 -c 'import sys; assert sys.version_info >= (3,10)' 2>/dev/null; then test -f .sdk/bin/python || python3 -m venv .sdk; env PIP_CONFIG_FILE=/dev/null PIP_EXTRA_INDEX_URL= .sdk/bin/python -m pip install --index-url https://pypi.org/simple --timeout 15 --retries 1 --disable-pip-version-check claude-agent-sdk==0.2.152; else test -d node_modules/@anthropic-ai/claude-agent-sdk || npm install --ignore-scripts --no-audit --no-fund --save-exact @anthropic-ai/claude-agent-sdk@0.3.268; fi",transport::quote(&directory));
            if let Err(error) = transport::ssh_timeout(&worker.info, &command, None, 180).await {
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
        // The probe starts every agent CLI to read versions and auth, which
        // takes close to a minute on a loaded worker.
        let raw = transport::ssh_timeout(
            &worker.info,
            &format!("python3 {} probe", transport::quote(&runner)),
            None,
            150,
        )
        .await?;
        inventory::project(
            &agent.memory.graph,
            &worker.info.name,
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
            &worker.info,
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
        transport::control(&worker.info, runner, "snapshot", &run.id, run.cursor, None).await?;
    let mut snapshot: Value = serde_json::from_str(&raw)?;
    enrich_approvals(&mut snapshot, &store.recent_events(&run.id, 1000)?);
    let peers=json!(runs.iter().filter(|r|r.task_id==run.task_id && r.id!=run.id && r.state!="superseded").map(|r|json!({"id":r.id,"key":r.assignment.key,"device":r.assignment.device,"agent":r.assignment.agent})).collect::<Vec<_>>());
    if snapshot["metadata"]["assignment"].is_object()
        && snapshot["metadata"]["assignment"]["peers"] != peers
    {
        transport::update_peers(&worker.info, &run.id, &peers).await?;
    }

    // Stage outgoing peer messages before advancing the source event cursor.
    for event in snapshot["events"].as_array().into_iter().flatten() {
        if event["kind"] == "peer" {
            let to = event["payload"]["to"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Peer destination missing"))?;
            let id = event["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Peer event ID missing"))?;
            let payload = json!({"id":id,"source":run.id,"kind":event["payload"]["kind"],"text":format!("Peer {} on {} ({}) says: {}",run.assignment.key,run.assignment.device,event["payload"]["kind"].as_str().unwrap_or("message"),event["payload"]["text"].as_str().unwrap_or(""))});
            if runs.iter().any(|r| r.id == to && r.state != "superseded") {
                store.message(id, &run.id, to, &payload)?;
            }
        }
    }
    store.sync(&run.id, &snapshot)?;
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
            &worker.info,
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
        transport::control(&worker.info, runner, "decide", &run.id, 0, Some(&decision)).await?;
        store.decision_delivered(&run.id, decision["id"].as_str().unwrap())?;
    }
    for message in store.pending_messages(&run.id)? {
        transport::control(&worker.info, runner, "enqueue", &run.id, 0, Some(&message)).await?;
        store.message_delivered(message["id"].as_str().unwrap())?;
    }
    // Only a completed native invocation proves a model works; the runtime's
    // own catalog is recorded even when that first call failed, so the user
    // can explicitly pick another listed model.
    let metadata = &snapshot["metadata"];
    if !metadata["invocation"].is_null() || metadata["available_models"].is_array() {
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
    fn completed_runs_are_polled_only_when_something_is_pending() {
        let path = std::env::temp_dir().join(format!("hive-idle-{}.db", uuid::Uuid::new_v4()));
        let graph = hive_core::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let plan = serde_json::from_value(json!({"summary":"work","assignments":[{"key":"a","device":"air","agent":"claude","model":null,"workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap();
        let id = store.create("task", "chat", &plan).unwrap().remove(0).id;
        assert!(!idle(&store, &store.get(&id).unwrap()).unwrap());
        store.state(&id, "completed", "").unwrap();
        assert!(idle(&store, &store.get(&id).unwrap()).unwrap());
        store
            .message("follow-up", "user", &id, &json!({"id":"follow-up","text":"verify"}))
            .unwrap();
        assert!(!idle(&store, &store.get(&id).unwrap()).unwrap());
        store.message_delivered("follow-up").unwrap();
        assert!(idle(&store, &store.get(&id).unwrap()).unwrap());
        drop(store);
        drop(graph);
        let _ = std::fs::remove_file(path);
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
}
