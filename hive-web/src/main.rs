//! Hive Web — browser-based terminal server.
//!
//! Serves a dashboard of live tmux sessions and bridges each one to xterm.js
//! over a WebSocket, so any session is attachable from a phone or laptop.
//!
//! Deployment shape: bind loopback, and let Tailscale Serve terminate TLS and
//! publish it on the tailnet. Binding 0.0.0.0 on a box with a public IP would
//! put a root-capable shell on the internet behind one password.

mod auth;
mod sessions;
mod terminal;

use std::net::SocketAddr;

use axum::{
    extract::{ws::WebSocketUpgrade, Path, Query},
    http::StatusCode,
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tower_http::services::ServeDir;
use tracing::{info, warn};

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
    // `Auth` is the router state: it is the only thing any handler needs.
    let auth_state = auth::Auth::new(password);

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/terminal/{name}", get(terminal_page))
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{name}", axum::routing::delete(kill_session))
        .route("/ws/{name}", get(ws_handler))
        .layer(middleware::from_fn_with_state(
            auth_state.clone(),
            auth::require_auth,
        ))
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(auth_state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, static_dir = %static_dir, "Hive Web listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------- pages

async fn dashboard() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
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
        Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({"name": req.name})))
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
