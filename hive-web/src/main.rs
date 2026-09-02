//! Hive Web — browser-based terminal server.
//!
//! Provides a web dashboard to view active tmux sessions across all workers
//! and a WebSocket-based terminal (xterm.js) to interact with them from
//! a phone or laptop browser.

use std::net::SocketAddr;

use axum::{routing::get, Router};
use tower_http::services::ServeDir;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hive_web=info".into()),
        )
        .init();

    let bind_addr: SocketAddr = std::env::var("HIVE_WEB_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;

    info!("Starting Hive Web terminal server on {}", bind_addr);

    let app = Router::new()
        .route("/api/health", get(health_check))
        .route("/api/sessions", get(list_sessions))
        // TODO: WebSocket endpoint for terminal streaming
        // .route("/ws/:session_id", get(ws_handler))
        .fallback_service(ServeDir::new("static"));

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!("Hive Web listening on {}", bind_addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_check() -> &'static str {
    "ok"
}

async fn list_sessions() -> &'static str {
    "[]" // TODO: Query all workers for active sessions
}
