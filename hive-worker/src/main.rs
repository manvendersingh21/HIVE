//! Hive Worker — lightweight daemon that runs on each worker machine.
//!
//! Receives task assignments from the master agent, executes commands
//! in tmux sessions, and reports status back.

use std::net::SocketAddr;

use axum::{
    extract::Path,
    routing::{get, post},
    Json, Router,
};
use hive_common::{TaskAssignment, TaskState, TaskStatus};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hive_worker=info".into()),
        )
        .init();

    let bind_addr: SocketAddr = std::env::var("HIVE_WORKER_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9091".to_string())
        .parse()?;

    info!("Starting Hive Worker daemon on {}", bind_addr);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/task", post(receive_task))
        .route("/status/{task_id}", get(task_status));

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!("Hive Worker listening on {}", bind_addr);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Health check endpoint.
async fn health_check() -> &'static str {
    "ok"
}

/// Receive a task assignment from the master.
async fn receive_task(Json(task): Json<TaskAssignment>) -> Json<TaskStatus> {
    info!("Received task '{}': {}", task.task_id, task.description);

    // TODO: Create tmux session, execute commands, track status
    let status = TaskStatus::running(&task.task_id, &task.tmux_session_name);
    Json(status)
}

/// Get the status of a running task.
async fn task_status(Path(task_id): Path<String>) -> Json<TaskStatus> {
    info!("Status request for task '{}'", task_id);

    // TODO: Look up actual task status
    let status = TaskStatus::new(&task_id, TaskState::Running);
    Json(status)
}
