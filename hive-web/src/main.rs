//! Hive Web — browser-based terminal server.
//!
//! Serves a dashboard of live tmux sessions and bridges each one to xterm.js
//! over a WebSocket, so any session is attachable from a phone or laptop.
//!
//! Deployment shape: bind loopback, and let Tailscale Serve terminate TLS and
//! publish it on the tailnet. Binding 0.0.0.0 on a box with a public IP would
//! put a root-capable shell on the internet behind one password.

mod auth;
mod chat;
mod incidents;
mod sessions;
mod terminal;
mod workers;

use std::net::SocketAddr;

use axum::{
    extract::{ws::WebSocketUpgrade, FromRef, Path, Query},
    http::StatusCode,
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hive_common::config::{HiveConfig, WorkersConfig};
use hive_core::agent::MasterAgent;
use hive_core::llm::LlmRouter;
use hive_core::memory::MemorySystem;
use hive_core::skills::SkillRegistry;
use hive_core::workers::WorkerPool;
use serde::Deserialize;
use tower_http::services::ServeDir;
use tracing::{info, warn};

/// Router state: the password gate plus the agent (absent on worker hosts).
/// How often the master re-probes worker reachability and machine facts.
///
/// Short enough that a worker coming back is usable quickly, long enough that a
/// fleet of unreachable hosts is not a steady stream of SSH timeouts.
const WORKER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone)]
struct AppState {
    auth: auth::Auth,
    agent: chat::AgentHandle,
    workers: workers::WorkerIngest,
    incidents: incidents::IncidentReview,
}

impl FromRef<AppState> for auth::Auth {
    fn from_ref(s: &AppState) -> Self {
        s.auth.clone()
    }
}

impl FromRef<AppState> for chat::AgentHandle {
    fn from_ref(s: &AppState) -> Self {
        s.agent.clone()
    }
}

impl FromRef<AppState> for workers::WorkerIngest {
    fn from_ref(s: &AppState) -> Self {
        s.workers.clone()
    }
}

impl FromRef<AppState> for incidents::IncidentReview {
    fn from_ref(s: &AppState) -> Self {
        s.incidents.clone()
    }
}

/// Build the master agent, if this host is configured to be one.
///
/// Returns a disabled handle rather than an error when there is no config or
/// no reachable local model: the same binary runs on workers, where it should
/// still serve terminals.
async fn build_agent(master_name: &str) -> chat::AgentHandle {
    let root = std::env::var("HIVE_CONFIG_ROOT").unwrap_or_else(|_| ".".to_string());
    let root = std::path::Path::new(&root);

    let config = match HiveConfig::from_project_root(root) {
        Ok(c) => c,
        Err(e) => {
            info!(error = %e, "no hive config found — serving terminals only, chat disabled");
            return chat::AgentHandle::disabled();
        }
    };

    let workers_config =
        WorkersConfig::from_project_root(root).unwrap_or(WorkersConfig { workers: vec![] });

    let llm = LlmRouter::from_config(&config.llm);

    // The same binary runs on workers, which ship a copy of the config but no
    // Ollama. Probe rather than trusting the config, so a host without a local
    // model serves terminals and says so, instead of offering a chat that
    // fails on the first message.
    if !llm.local_available().await {
        info!("local model unreachable — serving terminals only, chat disabled");
        return chat::AgentHandle::disabled();
    }

    // Health checks and the machine probe both reach workers over SSH, and an
    // SSH connect can stall well past its timeout (a half-open tunnel, a wedged
    // ControlMaster). Neither is needed to answer a request, so nothing here
    // blocks the listener — a stalled worker must not stop the master from
    // serving.
    let workers = WorkerPool::new(workers_config.workers);

    let memory = MemorySystem::open(config.database.resolved_path());
    let agent = MasterAgent::with_watchdog_config(
        llm,
        workers,
        SkillRegistry::new(),
        memory,
        config.watchdog,
    )
    .with_master_name(master_name);

    let agent = std::sync::Arc::new(agent);

    // Health and the machine graph both reach workers over SSH, and both are
    // refreshed on a timer rather than at startup: an SSH connect can stall
    // well past its timeout, and neither is needed to bind the listener.
    //
    // The timer is not optional. Workers start `Offline` and only a health
    // refresh moves them to `Online`, so without this loop the master would
    // never place remote work at all — it would report "no worker is online"
    // forever, with the worker sitting there perfectly reachable.
    let background = std::sync::Arc::clone(&agent);
    tokio::spawn(async move {
        let mut first = true;
        loop {
            background.workers.refresh_health().await;

            match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                background.refresh_machine_graph(),
            )
            .await
            {
                Ok(Ok(n)) if first => info!(machines = n, "machine knowledge graph seeded"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!(error = %e, "could not refresh machine knowledge graph"),
                Err(_) => warn!("machine graph refresh timed out"),
            }
            first = false;
            tokio::time::sleep(WORKER_REFRESH_INTERVAL).await;
        }
    });

    chat::AgentHandle::enabled(agent, master_name.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hive_web=info".into()),
        )
        .init();

    let password = std::env::var("HIVE_WEB_PASSWORD").map_err(|_| {
        anyhow::anyhow!(
            "HIVE_WEB_PASSWORD is not set — refusing to start an unauthenticated terminal server"
        )
    })?;
    if password.len() < 8 {
        anyhow::bail!("HIVE_WEB_PASSWORD must be at least 8 characters");
    }

    let bind_addr: SocketAddr = std::env::var("HIVE_WEB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;
    if !bind_addr.ip().is_loopback() {
        warn!(
            %bind_addr,
            "binding a non-loopback address — this exposes a shell beyond the local host"
        );
    }

    let static_dir =
        std::env::var("HIVE_WEB_STATIC").unwrap_or_else(|_| "hive-web/static".to_string());
    let master_name = std::env::var("HIVE_MASTER_NAME").unwrap_or_else(|_| hostname_or("master"));
    let state = AppState {
        auth: auth::Auth::new(password),
        agent: build_agent(&master_name).await,
        workers: workers::WorkerIngest::from_env(),
        incidents: incidents::IncidentReview::from_env(),
    };

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/sessions", get(sessions_page))
        .route("/machines", get(machines_page))
        .route("/incidents", get(incidents_page))
        .route("/login", get(login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/terminal/{name}", get(terminal_page))
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/capabilities", get(chat::capabilities))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{name}", axum::routing::delete(kill_session))
        .route("/api/chat", post(chat::chat))
        .route("/api/chat/{run_id}/approve", post(chat::approve))
        .route("/api/machines", get(chat::machine_graph))
        .route("/api/machines/refresh", post(chat::refresh_machines))
        .route("/api/machines/prompt", get(chat::machines_prompt))
        // Deciding an incident reaches a suspended process, so these must stay
        // above the `require_auth` layer with every other browser route — see
        // the module docs in `incidents.rs`.
        .route("/api/incidents", get(incidents::list))
        .route("/api/incidents/{id}", get(incidents::get_one))
        .route("/api/incidents/{id}/decide", post(incidents::decide))
        .route("/api/worker/status", post(workers::ingest))
        .route("/api/worker/tasks", get(workers::list))
        .route("/ws/{name}", get(ws_handler))
        // The static fallback is registered *above* the auth layer on purpose.
        // `Router::layer` wraps only what has been added when it is called, so
        // a `fallback_service` attached afterwards sits outside the gate — which
        // is where it was, serving every page shell (`/index.html`,
        // `/incidents.html`, …) to anyone who could reach the port. No incident
        // data leaked, since each page fetches its contents through a gated
        // `/api/` route, but the markup was public and the review page made that
        // worth fixing rather than noting. `require_auth` keeps `.css`/`.js` and
        // `/assets/` open, so the login page still styles itself.
        .fallback_service(ServeDir::new(&static_dir))
        .layer(middleware::from_fn_with_state(
            state.auth.clone(),
            auth::require_auth,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, static_dir = %static_dir, "Hive Web listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------- pages

/// The chat UI is the front door; the terminal list moved to /sessions.
async fn dashboard() -> Html<&'static str> {
    Html(include_str!("../static/chat.html"))
}

async fn sessions_page() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn machines_page() -> Html<&'static str> {
    Html(include_str!("../static/machines.html"))
}

async fn incidents_page() -> Html<&'static str> {
    Html(incidents::PAGE)
}

/// Short hostname, for naming the master in the machine graph.
fn hostname_or(fallback: &str) -> String {
    // `uname -n` is portable; Arch Linux has no `hostname` binary.
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

async fn login_page() -> Html<&'static str> {
    Html(include_str!("../static/login.html"))
}

async fn terminal_page() -> Html<&'static str> {
    Html(include_str!("../static/terminal.html"))
}

// ------------------------------------------------------------------ api

async fn list_sessions() -> Response {
    match sessions::list().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!(error = %e, "failed to list tmux sessions");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    name: String,
    #[serde(default = "default_kind")]
    kind: sessions::Kind,
    #[serde(default)]
    working_dir: Option<String>,
}

fn default_kind() -> sessions::Kind {
    sessions::Kind::Shell
}

async fn create_session(Json(req): Json<CreateRequest>) -> Response {
    if !sessions::valid_name(&req.name) {
        return (
            StatusCode::BAD_REQUEST,
            "name must be 1–64 chars of [A-Za-z0-9_-]",
        )
            .into_response();
    }
    match sessions::create(&req.name, req.kind, req.working_dir.as_deref()).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"name": req.name})),
        )
            .into_response(),
        Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

async fn kill_session(Path(name): Path<String>) -> Response {
    if !sessions::valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    match sessions::kill(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct TermSize {
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

async fn ws_handler(
    Path(name): Path<String>,
    Query(size): Query<TermSize>,
    ws: WebSocketUpgrade,
) -> Response {
    if !sessions::valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    if !sessions::exists(&name).await {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    let cols = size.cols.clamp(20, 500);
    let rows = size.rows.clamp(5, 200);
    ws.on_upgrade(move |socket| terminal::bridge(socket, name, cols, rows))
}
