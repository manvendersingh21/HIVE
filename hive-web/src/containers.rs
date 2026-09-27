//! Settings → Containers: register Docker containers as delegation devices.
//!
//! Adding only records which container on which machine a device name means;
//! removing only forgets it. Neither ever starts, stops or deletes a container.
use crate::chat::AgentHandle;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hive_core::delegation::{self, containers, transport};
use serde::Deserialize;
use serde_json::{json, Value};

fn bad(message: impl ToString) -> Response {
    (StatusCode::BAD_REQUEST, message.to_string()).into_response()
}

/// What an agent in a container needs, and which of it is there.
const CHECK: &str = r#"for t in python3 tmux node claude codex opencode; do command -v "$t" >/dev/null 2>&1 && echo "have=$t"; done
python3 -c 'import sys; print("python_ok=%d" % (sys.version_info >= (3, 10)))' 2>/dev/null
exit 0"#;

/// Missing requirements, or the error that kept the container from answering.
async fn check(worker: &hive_common::protocol::WorkerInfo) -> Value {
    match transport::ssh_timeout(worker, CHECK, None, 15).await {
        Ok(out) => {
            let have: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("have=")).collect();
            let mut missing: Vec<&str> = ["python3", "tmux"]
                .into_iter()
                .filter(|t| !have.contains(t))
                .collect();
            if have.contains(&"python3") && !out.contains("python_ok=1") {
                missing.push("python3 ≥ 3.10");
            }
            let agents: Vec<&str> = ["claude", "codex", "opencode"]
                .into_iter()
                .filter(|a| have.contains(a))
                .collect();
            json!({"reachable": true, "missing": missing, "agents": agents})
        }
        Err(e) => json!({"reachable": false, "error": e.to_string()}),
    }
}

/// Registered containers with a live check of each.
pub async fn list(State(h): State<AgentHandle>) -> Response {
    let agent = match h.require() {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    let entries = containers::load();
    let checks = futures::future::join_all(entries.iter().map(|c| async {
        match delegation::target(agent, &c.name) {
            Some(worker) if worker.container.is_some() => check(&worker).await,
            _ => json!({"reachable": false, "error": format!("{} is no longer a configured machine", c.host)}),
        }
    }))
    .await;
    Json(
        entries
            .iter()
            .zip(checks)
            .map(|(c, mut status)| {
                status["name"] = json!(c.name);
                status["host"] = json!(c.host);
                status["container"] = json!(c.container);
                status["managed"] = json!(c.managed);
                status
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// Machines whose Docker can hold containers: the fleet and the coordinator.
pub async fn hosts(State(h): State<AgentHandle>) -> Response {
    let agent = match h.require() {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    Json(
        delegation::targets(agent)
            .iter()
            .filter(|t| t.container.is_none())
            .map(|t| t.name.clone())
            .collect::<Vec<_>>(),
    )
    .into_response()
}

#[derive(Deserialize)]
pub struct HostQuery {
    host: String,
}

/// `docker ps -a` on one machine, so a person can pick what to add.
pub async fn available(State(h): State<AgentHandle>, Query(q): Query<HostQuery>) -> Response {
    let agent = match h.require() {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    let Some(machine) = delegation::machine(agent, &q.host) else {
        return bad(format!("Unknown machine {}", q.host));
    };
    let listing = transport::ssh_timeout(
        &machine,
        "docker ps -a --no-trunc --format '{{json .}}'",
        None,
        20,
    )
    .await;
    match listing {
        Ok(out) => Json(
            out.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .map(|c| {
                    json!({
                        "container": c["Names"],
                        "image": c["Image"],
                        "status": c["Status"],
                        "running": c["State"] == "running",
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        // Docker missing, stopped, or refused to this user: say so, don't hang.
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct AddRequest {
    name: String,
    host: String,
    container: String,
}

pub async fn add(State(h): State<AgentHandle>, Json(req): Json<AddRequest>) -> Response {
    let agent = match h.require() {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    if delegation::machine(agent, &req.host).is_none() {
        return bad(format!("Unknown machine {}", req.host));
    }
    let taken: Vec<String> = delegation::targets(agent).into_iter().map(|t| t.name).collect();
    let entry = containers::Container {
        name: req.name.trim().to_string(),
        host: req.host,
        container: req.container.trim().to_string(),
        managed: false,
        image: None,
    };
    if let Err(e) = containers::add(entry.clone(), &taken) {
        return bad(e);
    }
    // Record it as a machine and learn its agents now, not at the next sweep.
    let agent = agent.clone();
    let entry_name = entry.name.clone();
    tokio::spawn(async move {
        if let Err(e) = agent.refresh_machine_graph().await {
            tracing::warn!(error = %e, "machine refresh after adding a container failed");
        }
        if let Err(e) = delegation::inventory::refresh_devices(&agent, &[&entry_name]).await {
            tracing::warn!(error = %e, "agent inventory after adding a container failed");
        }
    });
    tracing::info!(container = %entry.name, host = %entry.host, "container added");
    (StatusCode::CREATED, Json(json!({"name": entry.name}))).into_response()
}

pub async fn remove(Path(name): Path<String>) -> Response {
    match containers::remove(&name) {
        Ok(removed) => {
            tracing::info!(container = %removed.name, "container removed");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}
