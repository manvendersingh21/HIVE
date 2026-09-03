//! Ingestion of worker status callbacks.
//!
//! `hive-worker` pushes a `TaskStatus` here on every state transition, so the
//! master learns that a task finished or failed without waiting for its next
//! poll. Polling `GET /status/{id}` on the worker remains the backstop; this is
//! the fast path.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use hive_common::TaskStatus;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Latest reported status per task id.
///
/// Last-write-wins and memory-only: this is a cache of what workers have said,
/// not a system of record. The worker's own registry is authoritative, and a
/// master restart simply re-learns on the next callback or poll.
pub type WorkerStatuses = Arc<RwLock<HashMap<String, TaskStatus>>>;

#[derive(Clone)]
pub struct WorkerIngest {
    pub statuses: WorkerStatuses,
    /// Shared secret workers must present. `None` accepts any caller that can
    /// reach the port, which on a tailnet-bound listener means any device on
    /// the tailnet.
    token: Option<Arc<String>>,
}

impl WorkerIngest {
    /// Build from `HIVE_WORKER_TOKEN`.
    pub fn from_env() -> Self {
        let token = std::env::var("HIVE_WORKER_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .map(Arc::new);
        if token.is_none() {
            warn!(
                "HIVE_WORKER_TOKEN unset — worker status callbacks are unauthenticated \
                 (the tailnet is then the only boundary)"
            );
        }
        Self {
            statuses: Arc::new(RwLock::new(HashMap::new())),
            token,
        }
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.token else {
            return true;
        };
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|got| constant_time_eq(got.as_bytes(), expected.as_bytes()))
            .unwrap_or(false)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `POST /api/worker/status` — a worker reporting one task's state.
pub async fn ingest(
    State(ingest): State<WorkerIngest>,
    headers: HeaderMap,
    Json(status): Json<TaskStatus>,
) -> Response {
    if !ingest.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "bad or missing worker token").into_response();
    }
    info!(
        task = %status.task_id,
        state = %status.state,
        worker = status.worker_name.as_deref().unwrap_or("?"),
        exit = ?status.exit_code,
        "worker status callback"
    );
    ingest
        .statuses
        .write()
        .await
        .insert(status.task_id.clone(), status);
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /api/worker/tasks` — everything workers have reported, newest first.
pub async fn list(State(ingest): State<WorkerIngest>) -> Json<Vec<TaskStatus>> {
    let mut all: Vec<_> = ingest.statuses.read().await.values().cloned().collect();
    all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Json(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn without_a_configured_token_any_caller_is_accepted() {
        let ingest = WorkerIngest {
            statuses: Default::default(),
            token: None,
        };
        assert!(ingest.authorized(&HeaderMap::new()));
    }

    #[test]
    fn with_a_token_only_the_matching_bearer_is_accepted() {
        let ingest = WorkerIngest {
            statuses: Default::default(),
            token: Some(Arc::new("s3cret".into())),
        };
        assert!(ingest.authorized(&headers_with("Bearer s3cret")));
        assert!(!ingest.authorized(&headers_with("Bearer wrong")));
        assert!(!ingest.authorized(&headers_with("s3cret")), "scheme is required");
        assert!(!ingest.authorized(&HeaderMap::new()));
    }
}
