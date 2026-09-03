//! Hive Worker — daemon that runs on each worker machine.
//!
//! Receives task assignments from the master, executes their commands in a
//! local tmux session, tracks real state, and reports back.
//!
//! ## Why this exists alongside direct SSH delegation
//!
//! `hive-core` can drive a worker's tmux over SSH without this daemon, and that
//! remains the default path. This daemon covers what that path structurally
//! cannot: accepting work with no SSH session held open by the master, and
//! exposing `pause`/`resume`/`kill` as endpoints any supervisor can call
//! without an interactive connection. Sessions it creates are shaped exactly
//! like the SSH path's, so both are attachable and readable the same way.

mod executor;
mod registry;
mod reporter;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hive_common::{TaskAssignment, TaskState, TaskStatus};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use registry::Registry;
use reporter::Reporter;

#[derive(Clone)]
struct AppState {
    registry: Registry,
    reporter: Reporter,
    worker_name: String,
    /// Shared secret required on every endpoint that starts or controls work.
    ///
    /// `POST /task` runs arbitrary shell as this user, so an unauthenticated
    /// daemon is a remote code execution endpoint for anyone who can reach the
    /// port. The daemon refuses to start without a token unless explicitly
    /// overridden for local development.
    token: Option<Arc<String>>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Require the shared token on everything except `/health`.
///
/// `/health` stays open so the master's reachability probe does not need a
/// credential to answer "is this box up".
async fn require_token(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }
    let Some(expected) = &state.token else {
        return Ok(next.run(req).await);
    };

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);

    if presented {
        Ok(next.run(req).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "bad or missing worker token").into_response())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hive_worker=info".into()),
        )
        .init();

    let bind_addr: SocketAddr = std::env::var("HIVE_WORKER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9091".to_string())
        .parse()?;
    // Warn about the exposure, but only claim it is unauthenticated when it
    // actually is — the check below decides that, so it happens after.
    let non_loopback = !bind_addr.ip().is_loopback();

    let worker_name = std::env::var("HIVE_WORKER_NAME").unwrap_or_else(|_| hostname());

    let token = std::env::var("HIVE_WORKER_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    let allow_open = std::env::var("HIVE_WORKER_ALLOW_UNAUTHENTICATED").is_ok();
    if token.is_none() && !allow_open {
        anyhow::bail!(
            "HIVE_WORKER_TOKEN is not set — refusing to start. This daemon executes \
             arbitrary shell commands, so an unauthenticated listener is a remote code \
             execution endpoint. Set a token, or set HIVE_WORKER_ALLOW_UNAUTHENTICATED=1 \
             if you really want it open (loopback only)."
        );
    }
    if token.is_none() {
        warn!("running WITHOUT authentication — every endpoint is open to anyone who can connect");
    }
    if non_loopback {
        warn!(
            %bind_addr,
            authenticated = token.is_some(),
            "binding a non-loopback address — this endpoint executes arbitrary commands; \
             keep it on a trusted network"
        );
    }
    if token.as_deref().map(|t| t.len() < 16).unwrap_or(false) {
        anyhow::bail!("HIVE_WORKER_TOKEN must be at least 16 characters");
    }

    let state = AppState {
        registry: Registry::new(),
        reporter: Reporter::from_env(),
        worker_name: worker_name.clone(),
        token: token.map(Arc::new),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/task", post(receive_task))
        .route("/tasks", get(list_tasks))
        .route("/status/{task_id}", get(task_status))
        .route("/task/{task_id}/pause", post(pause_task))
        .route("/task/{task_id}/resume", post(resume_task))
        .route("/task/{task_id}/kill", post(kill_task))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, worker = %worker_name, "Hive Worker listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn hostname() -> String {
    // `uname -n` is portable; Arch Linux has no `hostname` binary.
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "worker".to_string())
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    worker: String,
    callbacks: bool,
}

async fn health(State(s): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        worker: s.worker_name.clone(),
        callbacks: s.reporter.is_enabled(),
    })
}

/// Accept a task and start it in the background.
///
/// Returns as soon as the task is registered, not when it finishes: the work
/// may run for hours, and holding the master's connection open for that would
/// make the daemon's whole point moot.
async fn receive_task(State(s): State<AppState>, Json(task): Json<TaskAssignment>) -> Response {
    if task.commands.is_empty() {
        return (StatusCode::BAD_REQUEST, "task has no commands").into_response();
    }
    if executor::session_exists(&task.tmux_session_name).await {
        return (
            StatusCode::CONFLICT,
            format!("tmux session '{}' already exists", task.tmux_session_name),
        )
            .into_response();
    }

    let log_path = executor::log_path_for(&task.task_id);
    if let Err(e) = s.registry.accept(&task, log_path).await {
        return (StatusCode::CONFLICT, e.to_string()).into_response();
    }

    info!(task = %task.task_id, commands = task.commands.len(), "task accepted");

    let (registry, reporter, worker) = (s.registry.clone(), s.reporter.clone(), s.worker_name.clone());
    let accepted = task.clone();
    tokio::spawn(async move {
        executor::run_task(accepted, registry, reporter, worker).await;
    });

    let mut status = TaskStatus::running(&task.task_id, &task.tmux_session_name);
    status.worker_name = Some(s.worker_name.clone());
    (StatusCode::ACCEPTED, Json(status)).into_response()
}

#[derive(Deserialize)]
struct StatusQuery {
    /// Lines of output to include.
    #[serde(default = "default_lines")]
    lines: usize,
}

fn default_lines() -> usize {
    50
}

/// Real status for a task — or 404 if this daemon has never seen it.
///
/// The previous implementation answered `Running` for any id at all, which made
/// the endpoint actively misleading: the master could not tell a finished task
/// from a typo.
async fn task_status(
    State(s): State<AppState>,
    Path(task_id): Path<String>,
    Query(q): Query<StatusQuery>,
) -> Response {
    let Some(record) = s.registry.get(&task_id).await else {
        return (StatusCode::NOT_FOUND, format!("unknown task '{task_id}'")).into_response();
    };

    // Prefer the log (lossless) and fall back to the pane (only what is still
    // on screen) if the log is missing.
    let output = match executor::tail_log(&record.log_path, q.lines).await {
        Some(text) if !text.trim().is_empty() => Some(text),
        _ => executor::capture_pane(&record.tmux_session, q.lines as u32)
            .await
            .ok(),
    };

    Json(record.to_status(&s.worker_name, output)).into_response()
}

#[derive(Serialize)]
struct TaskSummary {
    task_id: String,
    description: String,
    state: String,
    tmux_session: String,
    exit_code: Option<i32>,
    accepted_at: String,
}

async fn list_tasks(State(s): State<AppState>) -> Json<Vec<TaskSummary>> {
    Json(
        s.registry
            .list()
            .await
            .into_iter()
            .map(|r| TaskSummary {
                task_id: r.task_id,
                description: r.description,
                state: r.state.to_string(),
                tmux_session: r.tmux_session,
                exit_code: r.exit_code,
                accepted_at: r.accepted_at.to_rfc3339(),
            })
            .collect(),
    )
}

/// Shared shape for the three control endpoints.
async fn control(
    s: &AppState,
    task_id: &str,
    action: &str,
) -> Response {
    let Some(record) = s.registry.get(task_id).await else {
        return (StatusCode::NOT_FOUND, format!("unknown task '{task_id}'")).into_response();
    };
    if record.is_terminal() {
        return (
            StatusCode::CONFLICT,
            format!("task '{task_id}' already {}", record.state),
        )
            .into_response();
    }

    let session = &record.tmux_session;
    let result = match action {
        "pause" => executor::pause(session).await.map(|pgid| Some(pgid)),
        "resume" => executor::resume(session, record.paused_pgid).await.map(|_| None),
        "kill" => executor::kill(session, record.paused_pgid).await.map(|_| None),
        _ => unreachable!("control actions are a closed set"),
    };

    let pgid = match result {
        Ok(pgid) => pgid,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    s.registry.set_paused_pgid(task_id, pgid).await;

    let new_state = match action {
        // Paused by an external supervisor — the same state the in-process
        // watchdog uses, so both look identical to whoever reviews it.
        "pause" => TaskState::PausedByWatchdog,
        "resume" => TaskState::Running,
        "kill" => TaskState::Cancelled,
        _ => unreachable!(),
    };
    s.registry.set_state(task_id, new_state).await;
    s.reporter.report(&s.registry, task_id, &s.worker_name).await;
    info!(task = task_id, action, "control applied");

    match s.registry.get(task_id).await {
        Some(r) => Json(r.to_status(&s.worker_name, None)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn pause_task(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    control(&s, &id, "pause").await
}

async fn resume_task(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    control(&s, &id, "resume").await
}

async fn kill_task(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    control(&s, &id, "kill").await
}
