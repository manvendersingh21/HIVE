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
        s.events(&id, q.after.max(0))
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
        .workers
        .iter()
        .find(|worker| worker.info.name == run.assignment.device)
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

async fn sync_run(
    h: &AgentHandle,
    store: &RunStore,
    run: &Run,
    runs: &[Run],
) -> anyhow::Result<()> {
    let agent = h.agent.as_ref().unwrap();
    let worker = agent
        .workers
        .workers
        .iter()
        .find(|w| w.info.name == run.assignment.device)
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
        let raw = transport::ssh(
            &worker.info,
            &format!("python3 {} probe", transport::quote(&runner)),
            None,
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
    // Evidence is learned from a completed native invocation, not installation.
    if !snapshot["metadata"]["invocation"].is_null() {
        let id = hive_core::memory::graph::entity_id(
            "device-agent",
            &format!("{}/{}", run.assignment.device, run.assignment.agent),
        );
        if let Some(mut record) = agent.memory.graph.entity(&id)? {
            record.attrs["invocation"] = snapshot["metadata"]["invocation"].clone();
            if snapshot["metadata"]["available_models"].is_array() {
                record.attrs["models"] = snapshot["metadata"]["available_models"].clone();
            }
            agent.memory.graph.upsert_entity(&record)?;
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
