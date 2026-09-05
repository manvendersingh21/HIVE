//! Human review of safety incidents.
//!
//! The watchdog suspends a session (SIGSTOP, never SIGKILL) when a Tier-1 rule
//! or a Tier-2 LLM review trips, and `hive_core::watchdog::incidents` writes
//! that fact down durably. This module is the other half: the endpoints an
//! operator's browser uses to see what is frozen and waiting, and to answer it.
//!
//! # These routes can abort a running session
//!
//! Every route here is registered *inside* the `auth::require_auth` layer in
//! `main`, and none of them matches that middleware's open-path list (`/login`,
//! `/api/health`, `/api/worker/status`, `/assets/`, `*.css`, `*.js`). Keep it
//! that way: `/api/incidents/{id}/decide` reaches a suspended process, so an
//! unauthenticated caller reaching it would be strictly worse than an
//! unauthenticated terminal — it could silently wave through the very command
//! the watchdog stopped.
//!
//! # Deciding writes first and acts second
//!
//! Acting on a decision — SIGCONT, a note typed into the pane, a corrected
//! command, or killing the session — belongs to `hive_core::watchdog::review`,
//! reached from exactly one call in [`decide`] rather than smeared across the
//! HTTP layer. `ReviewDesk::decide` records before it acts, because
//! `IncidentStore::resolve` is the compare-and-swap that rejects the second
//! operator: doing tmux work first would let both of them touch the process.
//! A decision written but not applied is recoverable; one applied but not
//! written is not.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hive_common::config::{DatabaseConfig, HiveConfig, WorkersConfig};
use hive_common::protocol::{HumanDecision, Incident, WorkerInfo};
use hive_core::watchdog::incidents::{IncidentStore, ResolveError};
use hive_core::watchdog::review::{Applied, DecisionError, ReviewDesk, SessionControl};
use hive_core::workers::ssh::SshWorker;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The review page. Served from `main` so the binary carries it.
pub const PAGE: &str = include_str!("../static/incidents.html");

/// How much history `?all=1` returns. A review page is for triage, not for
/// forensics — the full log is in the database for anyone who wants it.
const HISTORY_LIMIT: usize = 200;

/// The incident log, as router state.
#[derive(Clone)]
pub struct IncidentReview {
    store: IncidentStore,
    /// Where each worker's sessions live. Needed only to answer an incident:
    /// the SIGCONT has to be sent to the host that holds the stopped process.
    workers: Arc<Vec<WorkerInfo>>,
}

impl IncidentReview {
    pub fn new(store: IncidentStore) -> Self {
        Self {
            store,
            workers: Arc::new(Vec::new()),
        }
    }

    pub fn with_workers(mut self, workers: Vec<WorkerInfo>) -> Self {
        self.workers = Arc::new(workers);
        self
    }

    /// Open an SSH control channel to the worker holding a suspended session.
    ///
    /// Deliberately called *before* the decision is recorded. `resolve` spends
    /// the incident's single answer, so an operator who clicks Resume while the
    /// worker is unreachable must get an error with the incident still open,
    /// not a durable "resumed" against a process that never moved.
    async fn control_for(&self, worker: &str) -> Result<SshWorker, ApiError> {
        let info = self
            .workers
            .iter()
            .find(|w| w.name == worker)
            .ok_or_else(|| {
                ApiError::Upstream(format!(
                    "worker '{worker}' is not in workers.toml, so its suspended \
                     session cannot be reached — the incident stays open"
                ))
            })?;

        SshWorker::connect(&info.ssh_target()).await.map_err(|e| {
            ApiError::Upstream(format!(
                "could not reach worker '{worker}': {e} — the incident stays open"
            ))
        })
    }

    /// Open the configured incident log, falling back to an ephemeral one.
    ///
    /// A corrupt or unwritable `~/.hive/hive.db` costs the operator their
    /// incident *history*; refusing to boot would cost them every terminal on
    /// the host, which is the reason they can reach this box at all. So the
    /// failure is loud in the log and invisible to the listener.
    pub fn from_env() -> Self {
        let root = std::env::var("HIVE_CONFIG_ROOT").unwrap_or_else(|_| ".".to_string());
        let database = match HiveConfig::from_project_root(std::path::Path::new(&root)) {
            Ok(c) => c.database,
            // Workers ship no config at all; the documented default is where
            // the watchdog writes on a machine that has one.
            Err(_) => DatabaseConfig {
                path: "~/.hive/hive.db".to_string(),
            },
        };
        let path = database.resolved_path();
        // An empty roster is survivable: the page still lists and reads
        // incidents, and only *answering* one needs to reach its worker.
        let workers = WorkersConfig::from_project_root(std::path::Path::new(&root))
            .map(|c| c.workers)
            .unwrap_or_default();

        match IncidentStore::open(&path) {
            Ok(store) => Self::new(store).with_workers(workers),
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "could not open the incident log — reviews this session will not persist"
                );
                Self::new(
                    IncidentStore::in_memory()
                        .expect("an in-memory SQLite database cannot fail to open"),
                )
            }
        }
    }
}

/// An incident plus the answer it was given, if any.
///
/// `decision` is a separate column rather than part of `review_state` because
/// "resumed" does not say whether a note or a corrected command went back with
/// it, and that is what an operator reading the history wants to know.
#[derive(Debug, Serialize)]
pub struct IncidentView {
    #[serde(flatten)]
    pub incident: Incident,
    pub decision: Option<HumanDecision>,
}

/// An answered incident, plus what the session actually did about it — the
/// page distinguishes "resumed" from "the session had already finished".
#[derive(Debug, Serialize)]
pub struct DecidedView {
    #[serde(flatten)]
    pub view: IncidentView,
    pub applied: Applied,
}

/// `?all=1` for history, `?recent=N` for the last N in any state. Neither
/// present means pending only — the default a review page wants.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub recent: Option<usize>,
}

impl ListQuery {
    fn wants_history(&self) -> bool {
        self.recent.is_some()
            || matches!(
                self.all.as_deref(),
                Some("1") | Some("true") | Some("yes") | Some("")
            )
    }
}

/// Everything an HTTP caller can be told went wrong, and the status it earns.
#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    /// Two operators with the page open is the expected case, not an edge
    /// case. The loser gets 409 and the state that beat them, so the UI can
    /// say *what* happened instead of "something went wrong".
    Conflict(String),
    Internal(String),
    /// The incident log did its job but the worker did not. Distinct from
    /// `Internal` because the operator's next move is different: reach the
    /// session by hand, or fix connectivity and try again — and the message
    /// says which of those two applies.
    Upstream(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ApiError::NotFound(m)
            | ApiError::Conflict(m)
            | ApiError::Internal(m)
            | ApiError::Upstream(m) => m,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // JSON, not a bare string: the page shows `error` inline next to the
        // incident that lost the race.
        let body = Json(serde_json::json!({ "error": self.message() }));
        (self.status(), body).into_response()
    }
}

impl From<ResolveError> for ApiError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::NotFound(inner) => ApiError::NotFound(inner.to_string()),
            already @ ResolveError::AlreadyResolved { .. } => {
                ApiError::Conflict(already.to_string())
            }
            ResolveError::Other(inner) => ApiError::Internal(inner.to_string()),
        }
    }
}

impl From<DecisionError> for ApiError {
    fn from(e: DecisionError) -> Self {
        match e {
            DecisionError::Resolve(inner) => inner.into(),
            // The decision is durable and final; only the session failed to
            // move. 502 rather than 500 because nothing here will be fixed by
            // retrying the request — the message tells the operator to attach.
            recorded @ DecisionError::RecordedButNotApplied { .. } => {
                ApiError::Upstream(recorded.to_string())
            }
        }
    }
}

// ------------------------------------------------------------------ logic
//
// The handlers below are thin wrappers over these three functions. Splitting
// them this way is what makes the 409 path testable without standing up a
// listener: the tests drive these against an `IncidentStore::in_memory()`.

fn list_incidents(store: &IncidentStore, query: &ListQuery) -> Result<Vec<IncidentView>, ApiError> {
    let incidents = if query.wants_history() {
        store.recent(query.recent.unwrap_or(HISTORY_LIMIT))
    } else {
        store.pending()
    }
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    incidents.into_iter().map(|i| view(store, i)).collect()
}

fn one_incident(store: &IncidentStore, id: &str) -> Result<IncidentView, ApiError> {
    let incident = store
        .get(id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no incident '{id}'")))?;
    view(store, incident)
}

/// Record the answer and carry it out, in that order.
///
/// Generic over [`SessionControl`] so the whole path — including the 409 a
/// second operator gets — is testable without an SSH host or a live tmux.
async fn apply_decision<C: SessionControl>(
    store: &IncidentStore,
    control: C,
    id: &str,
    decision: &HumanDecision,
) -> Result<DecidedView, ApiError> {
    let (incident, applied) = ReviewDesk::new(store.clone(), control)
        .decide(id, decision)
        .await?;
    Ok(DecidedView {
        view: view(store, incident)?,
        applied,
    })
}

fn view(store: &IncidentStore, incident: Incident) -> Result<IncidentView, ApiError> {
    let decision = store
        .decision(&incident.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(IncidentView { incident, decision })
}

// --------------------------------------------------------------- handlers

/// `GET /api/incidents` — pending by default, `?all=1` or `?recent=N` for
/// everything.
pub async fn list(
    State(review): State<IncidentReview>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<IncidentView>>, ApiError> {
    list_incidents(&review.store, &query).map(Json)
}

/// `GET /api/incidents/{id}` — one incident and the answer it was given.
pub async fn get_one(
    State(review): State<IncidentReview>,
    Path(id): Path<String>,
) -> Result<Json<IncidentView>, ApiError> {
    one_incident(&review.store, &id).map(Json)
}

/// `POST /api/incidents/{id}/decide` — the operator's answer.
///
/// 200 with the updated incident, 404 for an unknown id, 409 if someone
/// already decided it.
pub async fn decide(
    State(review): State<IncidentReview>,
    Path(id): Path<String>,
    Json(decision): Json<HumanDecision>,
) -> Result<Json<DecidedView>, ApiError> {
    // Read before writing: the incident names the worker holding the stopped
    // process, and an unknown id should 404 having changed nothing. Connecting
    // first also keeps an unreachable worker from spending the incident's one
    // answer — see `control_for`.
    let pending = one_incident(&review.store, &id)?;
    let control = review.control_for(&pending.incident.worker).await?;

    let decided = apply_decision(&review.store, control, &id, &decision).await?;

    info!(
        incident = %id,
        session = %decided.view.incident.tmux_session,
        state = ?decided.view.incident.review_state,
        applied = ?decided.applied,
        "incident decided"
    );

    Ok(Json(decided))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::protocol::IncidentReviewState;

    /// A session that accepts every control operation. The ordering rules
    /// themselves are pinned in `hive_core::watchdog::review`; what matters
    /// here is the HTTP mapping around them.
    struct FakeControl {
        live: bool,
    }

    #[async_trait::async_trait]
    impl SessionControl for FakeControl {
        async fn resume(&self, _s: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn abort(&self, _s: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_line(&self, _s: &str, _l: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn interrupt(&self, _s: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn is_live(&self, _s: &str) -> bool {
            self.live
        }
    }

    /// The handler path, minus axum's extractors and SSH.
    async fn decide_on(
        store: &IncidentStore,
        id: &str,
        decision: &HumanDecision,
    ) -> Result<DecidedView, ApiError> {
        apply_decision(store, FakeControl { live: true }, id, decision).await
    }
    use hive_common::{SafetyAnalysis, SafetyCategory, Severity};
    use hive_core::watchdog::incidents::new_incident;

    fn review() -> IncidentReview {
        IncidentReview::new(IncidentStore::in_memory().unwrap())
    }

    fn analysis() -> SafetyAnalysis {
        SafetyAnalysis {
            is_safe: false,
            severity: Severity::Critical,
            category: Some(SafetyCategory::DestructiveCommand),
            reason: "Tier-1 rule 'destructive' matched: rm -rf /".to_string(),
            suggested_action: "Pause and review before resuming.".to_string(),
        }
    }

    fn raise(r: &IncidentReview, session: &str, flagged: &str) -> String {
        let incident = new_incident("task-1", "worker-1", session, analysis(), flagged);
        r.store.record(&incident).unwrap();
        incident.id
    }

    fn all() -> ListQuery {
        ListQuery {
            all: Some("1".into()),
            ..ListQuery::default()
        }
    }

    #[tokio::test]
    async fn listing_defaults_to_pending_and_all_includes_history() {
        let r = review();
        let a = raise(&r, "hive-w1", "rm -rf /");
        let b = raise(&r, "hive-w2", "cat ~/.aws/credentials");

        assert_eq!(
            list_incidents(&r.store, &ListQuery::default()).unwrap().len(),
            2
        );

        decide_on(&r.store, &a, &HumanDecision::Resume).await.unwrap();

        let pending = list_incidents(&r.store, &ListQuery::default()).unwrap();
        assert_eq!(pending.len(), 1, "a decided incident leaves the queue");
        assert_eq!(pending[0].incident.id, b);

        let history = list_incidents(&r.store, &all()).unwrap();
        assert_eq!(history.len(), 2, "?all=1 keeps the decided one");
        assert!(history.iter().any(|v| v.incident.id == a));
    }

    #[test]
    fn recent_bounds_the_history() {
        let r = review();
        for _ in 0..5 {
            raise(&r, "hive-w1", "rm -rf /");
        }
        let q = ListQuery {
            all: None,
            recent: Some(2),
        };
        assert!(q.wants_history());
        assert_eq!(list_incidents(&r.store, &q).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn deciding_records_the_answer_and_clears_the_queue() {
        let r = review();
        let id = raise(&r, "hive-w1", "rm -rf /");

        let out = decide_on(
            &r.store,
            &id,
            &HumanDecision::ResumeWithNote("stay in /tmp".into()),
        )
        .await
        .unwrap();
        assert_eq!(out.applied, Applied::ResumedWithNote);

        assert_eq!(out.view.incident.review_state, IncidentReviewState::Resumed);
        assert!(out.view.incident.resolved_at.is_some());
        assert_eq!(
            out.view.decision,
            Some(HumanDecision::ResumeWithNote("stay in /tmp".into())),
            "the note must survive for whoever hands it back to the session"
        );
        assert!(list_incidents(&r.store, &ListQuery::default())
            .unwrap()
            .is_empty());

        // And it is still readable by id, decision attached.
        let fetched = one_incident(&r.store, &id).unwrap();
        assert_eq!(fetched.incident.review_state, IncidentReviewState::Resumed);
        assert!(fetched.decision.is_some());
    }

    #[tokio::test]
    async fn an_abort_is_recorded_as_aborted() {
        let r = review();
        let id = raise(&r, "hive-w1", "rm -rf /");
        let out = decide_on(&r.store, &id, &HumanDecision::Abort).await.unwrap();
        assert_eq!(out.view.incident.review_state, IncidentReviewState::Aborted);
        assert_eq!(out.applied, Applied::Aborted);
    }

    #[tokio::test]
    async fn deciding_an_already_decided_incident_is_409_not_500() {
        // Two operators with the review page open is the expected case. The
        // loser must be told the incident was already answered — a 500 reads
        // as "retry", and a silent 200 would let resume and abort both claim
        // to have won on the same suspended process.
        let r = review();
        let id = raise(&r, "hive-w1", "rm -rf /");
        decide_on(&r.store, &id, &HumanDecision::Resume).await.unwrap();

        let err = decide_on(&r.store, &id, &HumanDecision::Abort)
            .await
            .expect_err("the second decision must be refused");
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(err.into_response().status(), StatusCode::CONFLICT);

        // The losing decision left no trace.
        assert_eq!(
            r.store.decision(&id).unwrap(),
            Some(HumanDecision::Resume),
            "the first answer stands"
        );
        assert_eq!(
            r.store.get(&id).unwrap().unwrap().review_state,
            IncidentReviewState::Resumed
        );
    }

    #[tokio::test]
    async fn an_unknown_id_is_404_on_both_read_and_decide() {
        let r = review();
        let read = one_incident(&r.store, "no-such-id").expect_err("unknown id");
        assert_eq!(read.status(), StatusCode::NOT_FOUND);

        let decided = decide_on(&r.store, "no-such-id", &HumanDecision::Resume)
            .await
            .expect_err("unknown id");
        assert_eq!(decided.status(), StatusCode::NOT_FOUND);
        assert_eq!(decided.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_session_that_already_finished_is_reported_not_hidden() {
        // An agent finishing while a human deliberates is the common case. The
        // decision is still recorded, but the page must not claim a process was
        // resumed when there was nothing left to resume.
        let r = review();
        let id = raise(&r, "hive-w1", "rm -rf /");

        let out = apply_decision(&r.store, FakeControl { live: false }, &id, &HumanDecision::Resume)
            .await
            .unwrap();

        assert_eq!(out.applied, Applied::SessionAlreadyGone);
        assert_eq!(out.view.incident.review_state, IncidentReviewState::Resumed);
    }

    #[tokio::test]
    async fn a_worker_that_cannot_be_reached_is_502_and_leaves_the_incident_open() {
        // `resolve` spends the incident's single answer. If an unreachable
        // worker consumed it, the operator would hold a durable "resumed"
        // against a process still frozen, and no way to ask again.
        let r = review();
        let id = raise(&r, "hive-w1", "rm -rf /");

        // `SshWorker` is not `Debug`, so match rather than `expect_err`.
        let err = match r.control_for("cis-a6000").await {
            Err(e) => e,
            Ok(_) => panic!("no workers are configured in this test"),
        };

        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.message().contains("stays open"), "got: {}", err.message());
        assert_eq!(
            r.store.get(&id).unwrap().unwrap().review_state,
            IncidentReviewState::PendingReview,
            "a failed connection must not consume the answer"
        );
    }

    #[test]
    fn a_recorded_but_unapplied_decision_is_502_not_500() {
        // 500 reads as "retry", and retrying can only ever hit the 409 — the
        // decision is already durable. The operator needs to be told to attach.
        let err: ApiError = DecisionError::RecordedButNotApplied {
            id: "abc".into(),
            session: "hive-w1".into(),
            intent: "resumed",
            source: anyhow::anyhow!("ssh: connection closed"),
        }
        .into();

        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.message().contains("hive-w1"));
        assert!(err.message().contains("cannot be re-submitted"));
    }

    #[test]
    fn flagged_output_reaches_the_client_verbatim() {
        // The server deliberately does not sanitize: a reviewer who is shown a
        // scrubbed command cannot judge it, and that judgment is the whole
        // point of the record. Escaping is therefore the *renderer's* duty,
        // which is what the next test pins.
        let r = review();
        let hostile = "<img src=x onerror=alert(1)>; rm -rf /";
        let id = raise(&r, "hive-w1", hostile);

        let out = one_incident(&r.store, &id).unwrap();
        assert_eq!(out.incident.flagged_output, hostile);

        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("onerror=alert(1)"),
            "the payload must carry what was flagged, escaped only by JSON"
        );
    }

    #[test]
    fn the_review_page_never_uses_innerhtml() {
        // `flagged_output` is untrusted process output that reached the
        // watchdog *because* it looked dangerous, and `reason` embeds the
        // matched line verbatim. Rendering either with innerHTML would turn
        // "an agent printed a string" into stored XSS in the operator's
        // browser, reachable by any command a supervised session runs. The
        // page builds nodes and assigns textContent instead; this test is
        // here so that stays true.
        assert!(
            !PAGE.contains("innerHTML"),
            "incidents.html must not use innerHTML — see this test's comment"
        );
        assert!(
            !PAGE.contains("insertAdjacentHTML") && !PAGE.contains("outerHTML"),
            "nor the other HTML-parsing sinks"
        );
    }

    #[test]
    fn history_flags_are_read_the_way_the_page_sends_them() {
        assert!(!ListQuery::default().wants_history());
        for flag in ["1", "true", "yes", ""] {
            let q = ListQuery {
                all: Some(flag.to_string()),
                recent: None,
            };
            assert!(q.wants_history(), "?all={flag} should include history");
        }
        let off = ListQuery {
            all: Some("0".into()),
            recent: None,
        };
        assert!(!off.wants_history());
    }
}
