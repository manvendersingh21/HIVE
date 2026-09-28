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
mod containers;
mod delegation;
mod incidents;
mod sessions;
mod ssh_setup;
mod terminal;
mod workers;

use std::net::SocketAddr;

use axum::{
    extract::{ws::WebSocketUpgrade, FromRef, Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, get_service, post, MethodRouter},
    Json, Router,
};
use hive_common::config::{HiveConfig, WorkersConfig};
use hive_core::agent::MasterAgent;
use hive_core::llm::LlmRouter;
use hive_core::memory::MemorySystem;
use hive_core::skills::SkillRegistry;
use hive_core::workers::WorkerPool;
use serde::Deserialize;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

/// Router state: the password gate plus the agent (absent on worker hosts).
/// How often the master re-probes worker reachability and machine facts.
///
/// Short enough that a worker coming back is usable quickly, long enough that a
/// fleet of unreachable hosts is not a steady stream of SSH timeouts.
/// Read once at startup, then removed from the process environment.
const SECRET_ENV: [&str; 2] = ["HIVE_WEB_PASSWORD", "HIVE_WORKER_TOKEN"];
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
/// Workers without master configuration still serve terminals. Configured
/// masters keep saved chats available when inference is temporarily offline.
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
    chat::apply_persisted_master_agent(&llm, &config.llm);

    // History remains readable even when the configured inference server is down.
    if llm.uses_local_startup() && !llm.local_available().await {
        info!(
            "local model unreachable — saved chats remain available; planning will report errors"
        );
    }

    // Health checks and the machine probe both reach workers over SSH, and an
    // SSH connect can stall well past its timeout (a half-open tunnel, a wedged
    // ControlMaster). Neither is needed to answer a request, so nothing here
    // blocks the listener — a stalled worker must not stop the master from
    // serving.
    let workers = WorkerPool::new(workers_config.workers);
    chat::apply_persisted_fleet(&workers);

    // Web messages must be durable; never advertise saved chats over an
    // in-memory fallback when opening the configured database fails.
    let memory = match MemorySystem::open_for_reindex(config.database.resolved_path(), &config) {
        Ok(memory) => memory,
        Err(e) => {
            warn!(error = %e, "chat database unavailable");
            return chat::AgentHandle::disabled();
        }
    };
    // Loaded, not empty: the web chat resolves skills at request time; a
    // `require_confirmation` skill gates its steps through the same approval
    // flow (POST /api/chat/{run_id}/approve) as the Tier-1 rules.
    let skills = SkillRegistry::load(&config.skills);
    let agent = MasterAgent::with_watchdog_config(llm, workers, skills, memory, config.watchdog)
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
            if let Err(e) = hive_core::delegation::inventory::refresh(&background).await {
                warn!(error=%e, "agent inventory refresh failed");
            }
            first = false;
            tokio::time::sleep(WORKER_REFRESH_INTERVAL).await;
        }
    });

    let handle = chat::AgentHandle::enabled(agent, master_name.to_string()).unwrap_or_else(|e| {
        warn!(error = %e, "could not initialize saved chats");
        chat::AgentHandle::disabled()
    });
    if handle.agent.is_some() {
        delegation::start(handle.clone());
    }
    handle
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
    let workers = workers::WorkerIngest::from_env();
    // Both secrets are held in memory now. Drop them from the environment
    // before anything spawns ssh, tmux or agents, which would inherit them;
    // a tmux server started from here keeps them for every pane it opens.
    for key in SECRET_ENV {
        std::env::remove_var(key);
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
        workers,
        incidents: incidents::IncidentReview::from_env(),
    };

    let app = app_router(state, &static_dir);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, static_dir = %static_dir, "Hive Web listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn app_router(state: AppState, static_dir: &str) -> Router {
    Router::new()
        // Next.js owns the browser page shells. The Rust handlers below remain
        // the JSON, auth, and WebSocket API.
        .route("/", page_shell(static_dir, "index.html"))
        .route("/sessions", page_shell(static_dir, "sessions/index.html"))
        .route("/machines", page_shell(static_dir, "machines/index.html"))
        .route("/incidents", page_shell(static_dir, "incidents/index.html"))
        .route("/terminal", page_shell(static_dir, "terminal/index.html"))
        .route("/session", page_shell(static_dir, "session/index.html"))
        .route(
            "/login",
            page_shell(static_dir, "login/index.html").post(auth::login),
        )
        .route("/logout", post(auth::logout))
        .route("/api/session-hosts", get(session_hosts))
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/capabilities", get(chat::capabilities))
        .route(
            "/api/settings/master-agent",
            get(chat::master_agent_settings).post(chat::set_master_agent),
        )
        .route("/api/containers", get(containers::list).post(containers::add))
        .route("/api/containers/hosts", get(containers::hosts))
        .route("/api/containers/available", get(containers::available))
        .route("/api/containers/{name}", axum::routing::delete(containers::remove))
        .route(
            "/api/settings/autonomy",
            get(delegation::autonomy).post(delegation::set_autonomy),
        )
        .route(
            "/api/fleet",
            get(chat::list_fleet).post(chat::add_fleet_worker),
        )
        .route(
            "/api/fleet/ssh/key",
            get(ssh_setup::get_key).post(ssh_setup::create_key),
        )
        .route("/api/fleet/ssh/test", post(ssh_setup::test_connection))
        .route("/api/fleet/ssh/host-key", post(ssh_setup::scan_host_key))
        .route("/api/fleet/ssh/trust", post(ssh_setup::trust_host_key))
        .route("/api/fleet/ssh/install-key", post(ssh_setup::install_key))
        .route(
            "/api/fleet/{name}",
            axum::routing::delete(chat::remove_fleet_worker),
        )
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{name}", axum::routing::delete(kill_session))
        .route("/api/chats", get(chat::list_chats).post(chat::create_chat))
        .route("/api/chats/{id}", get(chat::get_chat))
        .route("/api/chat", post(chat::chat))
        .route("/api/chat/{run_id}/approve", post(chat::approve))
        .route("/api/runs", get(delegation::list))
        .route("/api/runs/{id}/events", get(delegation::events))
        .route("/api/runs/{id}/decisions", post(delegation::decide))
        .route("/api/runs/{id}/messages", post(delegation::message))
        .route("/api/runs/{id}/replace", post(delegation::replace))
        .route("/api/runs/{id}/retry-setup", post(delegation::retry_setup))
        .route(
            "/api/runs/{id}/recovery",
            get(delegation::recovery).post(delegation::reconcile),
        )
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
        // `/incidents/index.html`, …) to anyone who could reach the port. No incident
        // data leaked, since each page fetches its contents through a gated
        // `/api/` route, but the markup was public and the review page made that
        // worth fixing rather than noting. `require_auth` keeps `.css`/`.js` and
        // `/assets/` open, so the login page still styles itself.
        .fallback_service(ServeDir::new(static_dir))
        .layer(middleware::from_fn_with_state(
            state.auth.clone(),
            auth::require_auth,
        ))
        .with_state(state)
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request},
    };
    use hive_core::watchdog::incidents::IncidentStore;
    use tower::ServiceExt;

    #[test]
    fn coordinator_runs_attach_to_local_tmux_and_workers_keep_ssh() {
        let agent = hive_core::agent::MasterAgent::new(
            hive_core::llm::LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            hive_core::workers::WorkerPool::new(vec![hive_common::WorkerInfo {
                name: "air".into(),
                host: "air-ssh".into(),
                user: "test".into(),
                port: None,
                tags: vec![],
                local: false,
                container: None,
            }]),
            hive_core::skills::SkillRegistry::new(),
            hive_core::memory::MemorySystem::new(),
        )
        .with_master_name("mac-mini");
        let h = chat::AgentHandle {
            agent: Some(std::sync::Arc::new(agent)),
            history: None,
            master_name: "mac-mini".into(),
        };
        assert!(session_worker(&h, "local").unwrap().is_none());
        assert!(session_worker(&h, "mac-mini").unwrap().is_none());
        assert_eq!(session_worker(&h, "air").unwrap().unwrap().host, "air-ssh");
        assert_eq!(session_worker(&h, "nowhere").unwrap_err().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn actual_router_gates_static_pages_and_apis() {
        let auth = auth::Auth::new("test-password".into());
        let state = AppState {
            auth,
            agent: chat::AgentHandle::disabled(),
            workers: workers::WorkerIngest::from_env(),
            incidents: incidents::IncidentReview::new(IncidentStore::in_memory().unwrap()),
        };
        // The frontend export isn't in git; stand in for the pages it ships.
        let dir = std::env::temp_dir().join(format!("hive-web-gate-{}", uuid::Uuid::new_v4()));
        for file in ["index.html", "login/index.html", "incidents/index.html", "session/index.html"] {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("<p>{file}</p>")).unwrap();
        }
        let app = app_router(state, dir.to_str().unwrap());
        for path in [
            "/",
            "/sessions",
            "/incidents",
            "/terminal",
            "/session",
            "/machines",
            "/settings",
            "/index.html",
            "/incidents/index.html",
            "/machines/index.html",
            "/terminal/index.html",
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
            assert_eq!(response.headers()[header::LOCATION], "/login", "{path}");
        }
        for path in [
            "/api/chats",
            "/api/chats/example",
            "/api/sessions",
            "/api/incidents",
            "/api/machines",
            "/ws/test",
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
        for path in ["/login", "/api/health"] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
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
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        for path in ["/index.html", "/incidents/index.html", "/session/index.html"] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "authenticated {path}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn page_shells_come_from_the_runtime_static_dir() {
        let dir = std::env::temp_dir().join(format!("hive-web-shells-{}", uuid::Uuid::new_v4()));
        let pages = [
            ("/", "index.html"),
            ("/sessions", "sessions/index.html"),
            ("/machines", "machines/index.html"),
            ("/incidents", "incidents/index.html"),
            ("/terminal", "terminal/index.html"),
            ("/session", "session/index.html"),
            ("/login", "login/index.html"),
        ];
        for (_, file) in pages {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("<p>runtime {file}</p>")).unwrap();
        }
        let auth = auth::Auth::new("test-password".into());
        let state = AppState {
            auth,
            agent: chat::AgentHandle::disabled(),
            workers: workers::WorkerIngest::from_env(),
            incidents: incidents::IncidentReview::new(IncidentStore::in_memory().unwrap()),
        };
        let app = app_router(state, dir.to_str().unwrap());
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
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        for (path, file) in pages {
            let response = app
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(body, format!("<p>runtime {file}</p>").as_bytes(), "{path}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ---------------------------------------------------------------- pages

/// Serve a Next.js page shell from the static export at request time.
///
/// Shells used to be `include_str!`'d, which pinned them to whatever export
/// existed at compile time while their `_next/` chunks came from
/// `HIVE_WEB_STATIC` at runtime — so pointing the server at a newer export
/// served HTML that referenced chunk hashes the directory no longer had.
fn page_shell(static_dir: &str, page: &str) -> MethodRouter<AppState> {
    get_service(ServeFile::new(std::path::Path::new(static_dir).join(page)))
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

// ------------------------------------------------------------------ api

fn session_worker(
    h: &chat::AgentHandle,
    host: &str,
) -> Result<Option<hive_common::protocol::WorkerInfo>, Response> {
    if host == "local" {
        return Ok(None);
    }
    match h.agent.as_ref().and_then(|a| hive_core::delegation::target(a, host)) {
        // An agent run on the coordinator names its device, not "local".
        // A container on the coordinator still goes through `docker exec`.
        Some(t) if t.local && t.container.is_none() => Ok(None),
        Some(t) => Ok(Some(t)),
        None => Err((StatusCode::NOT_FOUND, "unknown worker").into_response()),
    }
}

async fn session_hosts(State(h): State<chat::AgentHandle>) -> Json<serde_json::Value> {
    let mut hosts = vec![serde_json::json!({"host": "local", "name": h.master_name})];
    if let Some(agent) = h.agent {
        hosts.extend(
            hive_core::delegation::targets(&agent)
                .iter()
                .filter(|t| !t.local || t.container.is_some())
                .map(|t| serde_json::json!({"host": t.name, "name": t.name})),
        );
    }
    Json(serde_json::json!(hosts))
}

async fn list_sessions(State(h): State<chat::AgentHandle>) -> Response {
    let local = sessions::list();
    let remote =
        async {
            match &h.agent {
                Some(agent) => {
                    // The SSH fleet plus registered containers; the coordinator
                    // itself is the local listing above.
                    let devices: Vec<_> = hive_core::delegation::targets(agent)
                        .into_iter()
                        .filter(|t| !t.local || t.container.is_some())
                        .collect();
                    futures::future::join_all(devices.iter().map(|w| async {
                        (w.name.clone(), sessions::list_on(w).await)
                    }))
                    .await
                }
                None => vec![],
            }
        };
    let (local, remote) = tokio::join!(local, remote);
    let mut list = Vec::new();
    let mut errors = Vec::new();
    match local {
        Ok(s) => list.extend(s),
        Err(e) => errors.push(format!("local: {e}")),
    }
    for (host, result) in remote {
        match result {
            Ok(s) => list.extend(s),
            Err(e) => errors.push(format!("{host}: {e}")),
        }
    }
    if let Ok(store) = delegation::store(&h) {
        if let Ok(runs) = store.list() {
            for run in runs {
                // Runs on the coordinator live in its local tmux server.
                let host = match h.agent.as_deref().and_then(|a| {
                    hive_core::delegation::target(a, &run.assignment.device)
                }) {
                    Some(t) if t.local && t.container.is_none() => sessions::local_host(),
                    _ => run.assignment.device.clone(),
                };
                if let Some(session) = list
                    .iter_mut()
                    .find(|s| s.name == run.tmux_name && s.host == host)
                {
                    session.run = Some(run);
                } else {
                    list.push(sessions::Session {
                        name: run.tmux_name.clone(),
                        host,
                        windows: 0,
                        created: 0,
                        attached: false,
                        current_command: String::new(),
                        window_name: run.assignment.agent.clone(),
                        run: Some(run),
                    });
                }
            }
        }
    }
    let mut response = Json(list).into_response();
    if !errors.is_empty() {
        // Keep the array API compatible; the dashboard displays partial failures.
        let value = serde_json::to_string(&errors).unwrap();
        if let Ok(header) = value.parse() {
            response
                .headers_mut()
                .insert("x-hive-session-errors", header);
        }
    }
    response
}

#[derive(Deserialize)]
struct SessionHost {
    #[serde(default = "sessions::local_host")]
    host: String,
}

#[derive(Deserialize)]
struct CreateRequest {
    #[serde(default = "sessions::local_host")]
    host: String,
    name: String,
    #[serde(default = "default_kind")]
    kind: sessions::Kind,
    #[serde(default)]
    working_dir: Option<String>,
}

fn default_kind() -> sessions::Kind {
    sessions::Kind::Shell
}

async fn create_session(
    State(h): State<chat::AgentHandle>,
    Json(req): Json<CreateRequest>,
) -> Response {
    if !sessions::valid_name(&req.name) {
        return (
            StatusCode::BAD_REQUEST,
            "name must be 1–64 chars of [A-Za-z0-9_-]",
        )
            .into_response();
    }
    let worker = match session_worker(&h, &req.host) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let result = match worker {
        Some(w) => sessions::create_on(&w, &req.name, req.kind, req.working_dir.as_deref()).await,
        None => sessions::create(&req.name, req.kind, req.working_dir.as_deref()).await,
    };
    match result {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"name": req.name, "host": req.host})),
        )
            .into_response(),
        Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

async fn kill_session(
    State(h): State<chat::AgentHandle>,
    Path(name): Path<String>,
    Query(target): Query<SessionHost>,
) -> Response {
    if !sessions::valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    let worker = match session_worker(&h, &target.host) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let result = match worker {
        Some(w) => sessions::kill_on(&w, &name).await,
        None => sessions::kill(&name).await,
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct TermSize {
    #[serde(default = "sessions::local_host")]
    host: String,
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
    State(h): State<chat::AgentHandle>,
    Path(name): Path<String>,
    Query(size): Query<TermSize>,
    ws: WebSocketUpgrade,
) -> Response {
    if !sessions::valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    let worker = match session_worker(&h, &size.host) {
        Ok(w) => w,
        Err(r) => return r,
    };
    match &worker {
        Some(w) => {
            if let Err(e) = sessions::exists_on(w, &name).await {
                return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
            }
        }
        None => {
            if !sessions::exists(&name).await {
                return (StatusCode::NOT_FOUND, "no such session").into_response();
            }
        }
    }
    let cols = size.cols.clamp(20, 500);
    let rows = size.rows.clamp(5, 200);
    ws.on_upgrade(move |socket| terminal::bridge(socket, name, worker, cols, rows))
}
