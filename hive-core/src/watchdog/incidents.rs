//! Durable incident records — the watchdog's memory.
//!
//! Until now a Tier-1 or Tier-2 hit produced a `tracing::warn!` and nothing
//! else. The session was suspended correctly, but the fact that it happened
//! lived only in a log line: nothing could list what was awaiting review,
//! nothing could tell whether a human had already answered, and a restart of
//! the supervising process forgot every incident it had ever raised.
//! [`Incident`] and [`IncidentReviewState`] have been declared in
//! `hive-common` since Phase 1 with no producer. This is the producer.
//!
//! The substrate is the one [`crate::memory::graph`] already proved: SQLite
//! behind `Arc<Mutex<Connection>>`, caller-chosen ids, idempotent upserts.
//! Incidents get their own table rather than becoming graph entities because
//! they are queried by state and recency ("what is pending?"), not by
//! traversal.
//!
//! # A record of an incident can itself be sensitive
//!
//! `SafetyCategory::CredentialExposure` fires precisely *because* a
//! credential appeared in a session's output — and `flagged_output` is that
//! output. Persisting it writes the secret to disk. Redacting it wholesale
//! would be worse than useless: a reviewer who cannot see what was flagged
//! cannot judge it, which is the entire point of the record. So the bytes are
//! kept and the file is locked down instead — [`IncidentStore::open`] forces
//! mode `0600` on the database. Anyone who can read that file could already
//! read the operator's `~/.hive` credentials; nothing new is exposed to a
//! reader who was not already inside. Do not relax those permissions, and do
//! not copy an incident database off the host.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use hive_common::protocol::{HumanDecision, Incident, IncidentReviewState, SafetyAnalysis};
use hive_common::{HiveError, Severity};
use rusqlite::{params, Connection, OptionalExtension, Row};

/// Mint a fresh pending incident from a safety analysis.
///
/// The id is a uuid rather than something derived from the session, because a
/// single session can raise more than one incident over its life and each is
/// reviewed on its own.
pub fn new_incident(
    task_id: &str,
    worker: &str,
    tmux_session: &str,
    analysis: SafetyAnalysis,
    flagged_output: &str,
) -> Incident {
    Incident {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        worker: worker.to_string(),
        tmux_session: tmux_session.to_string(),
        analysis,
        flagged_output: flagged_output.to_string(),
        review_state: IncidentReviewState::PendingReview,
        created_at: Utc::now(),
        resolved_at: None,
    }
}

/// SQLite-backed incident log. Cloning shares the same connection.
#[derive(Clone)]
pub struct IncidentStore {
    conn: Arc<Mutex<Connection>>,
}

impl IncidentStore {
    /// Open (creating if needed) the incident log at `path`.
    ///
    /// Parent directories are created — the configured default lives under
    /// `~/.hive/`, which will not exist on a fresh machine. The file is
    /// chmod'd to `0600`; see the module docs for why that matters here more
    /// than it does for the knowledge graph.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = Self::from_connection(Connection::open(path)?)?;
        restrict_permissions(path)?;
        Ok(store)
    }

    /// An ephemeral in-memory log, for tests and as the fallback when the
    /// on-disk database cannot be opened. A broken db file should cost the
    /// operator their incident *history*, not their supervision: a watchdog
    /// that refuses to start because it cannot log is strictly worse than one
    /// that runs and warns.
    pub fn in_memory() -> anyhow::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS incidents (
                 id             TEXT PRIMARY KEY,
                 task_id        TEXT NOT NULL,
                 worker         TEXT NOT NULL,
                 tmux_session   TEXT NOT NULL,
                 severity       TEXT NOT NULL,
                 analysis       TEXT NOT NULL,
                 flagged_output TEXT NOT NULL,
                 review_state   TEXT NOT NULL,
                 created_at     TEXT NOT NULL,
                 resolved_at    TEXT,
                 decision       TEXT
             );
             CREATE INDEX IF NOT EXISTS incidents_pending
                 ON incidents(review_state, created_at DESC);
             CREATE INDEX IF NOT EXISTS incidents_session ON incidents(tmux_session);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert or replace an incident, keyed on its id.
    ///
    /// Idempotent, so a supervisor that crashes between raising an incident
    /// and confirming the write can replay it without duplicating the row —
    /// the same reason [`crate::memory::graph`] upserts on a caller-chosen id.
    /// A replay must not resurrect a decision, so `resolved_at` and `decision`
    /// are left alone on conflict: only a [`Self::resolve`] call writes them.
    pub fn record(&self, incident: &Incident) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO incidents
                 (id, task_id, worker, tmux_session, severity, analysis,
                  flagged_output, review_state, created_at, resolved_at, decision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)
             ON CONFLICT(id) DO UPDATE SET
                 task_id        = excluded.task_id,
                 worker         = excluded.worker,
                 tmux_session   = excluded.tmux_session,
                 severity       = excluded.severity,
                 analysis       = excluded.analysis,
                 flagged_output = excluded.flagged_output",
            params![
                incident.id,
                incident.task_id,
                incident.worker,
                incident.tmux_session,
                severity_str(incident.analysis.severity),
                serde_json::to_string(&incident.analysis)?,
                incident.flagged_output,
                state_str(&incident.review_state),
                incident.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<Incident>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(SELECT_COLUMNS, params![id], row_to_incident)
            .optional()?;
        Ok(row.transpose()?)
    }

    /// Every incident still awaiting a human, newest first.
    pub fn pending(&self) -> anyhow::Result<Vec<Incident>> {
        self.query(
            "WHERE review_state = 'pending_review' ORDER BY created_at DESC",
            params![],
        )
    }

    /// The `limit` most recent incidents in any state.
    pub fn recent(&self, limit: usize) -> anyhow::Result<Vec<Incident>> {
        self.query("ORDER BY created_at DESC LIMIT ?1", params![limit as i64])
    }

    /// Every incident raised against one tmux session, newest first.
    pub fn for_session(&self, tmux_session: &str) -> anyhow::Result<Vec<Incident>> {
        self.query(
            "WHERE tmux_session = ?1 ORDER BY created_at DESC",
            params![tmux_session],
        )
    }

    /// The decision recorded against an incident, if it has been reviewed.
    pub fn decision(&self, id: &str) -> anyhow::Result<Option<HumanDecision>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT decision FROM incidents WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        match raw.flatten() {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Record a human's answer and move the incident out of review.
    ///
    /// Returns the updated incident, so a caller can act on the decision
    /// against the session named in the row it just won.
    ///
    /// # Deciding twice is refused, not merged
    ///
    /// The `review_state = 'pending_review'` guard in the UPDATE makes this a
    /// compare-and-swap: exactly one caller can resolve a given incident.
    /// Two operators clicking *resume* in the web UI, or a retried request,
    /// would otherwise both proceed to act on the session — and "resume" and
    /// "abort" racing each other on the same suspended process is precisely
    /// the state a human was asked to prevent. The loser gets
    /// [`ResolveError::AlreadyResolved`] naming the decision that won.
    pub fn resolve(&self, id: &str, decision: &HumanDecision) -> Result<Incident, ResolveError> {
        let state = match decision {
            HumanDecision::Abort => IncidentReviewState::Aborted,
            HumanDecision::Resume
            | HumanDecision::ResumeWithNote(_)
            | HumanDecision::ModifyAndResume(_) => IncidentReviewState::Resumed,
        };
        let decision_json = serde_json::to_string(decision).map_err(ResolveError::other)?;

        {
            let conn = self.conn.lock().unwrap();
            let changed = conn
                .execute(
                    "UPDATE incidents
                        SET review_state = ?2, decision = ?3, resolved_at = ?4
                      WHERE id = ?1 AND review_state = 'pending_review'",
                    params![
                        id,
                        state_str(&state),
                        decision_json,
                        Utc::now().to_rfc3339()
                    ],
                )
                .map_err(ResolveError::other)?;

            if changed == 0 {
                // Distinguish "no such incident" from "someone got there first":
                // the caller shows the operator a very different message for each.
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT review_state FROM incidents WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(ResolveError::other)?;
                return Err(match existing {
                    Some(state) => ResolveError::AlreadyResolved {
                        id: id.to_string(),
                        state,
                    },
                    None => ResolveError::NotFound(HiveError::IncidentNotFound(id.to_string())),
                });
            }
        }

        self.get(id)
            .map_err(ResolveError::Other)?
            .ok_or_else(|| ResolveError::NotFound(HiveError::IncidentNotFound(id.to_string())))
    }

    /// How many incidents are waiting on a human. Cheap enough to poll for a
    /// status line or a badge.
    pub fn pending_count(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM incidents WHERE review_state = 'pending_review'",
            params![],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    fn query(&self, tail: &str, args: &[&dyn rusqlite::ToSql]) -> anyhow::Result<Vec<Incident>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{SELECT_ALL} {tail}"))?;
        let rows = stmt.query_map(args, row_to_incident)?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// Why a [`IncidentStore::resolve`] could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    NotFound(HiveError),
    #[error("incident '{id}' was already reviewed ({state}) — refusing to decide it twice")]
    AlreadyResolved { id: String, state: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ResolveError {
    fn other(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Other(anyhow::Error::new(e))
    }
}

const SELECT_ALL: &str = "SELECT id, task_id, worker, tmux_session, analysis, flagged_output, \
                          review_state, created_at, resolved_at FROM incidents";
const SELECT_COLUMNS: &str = "SELECT id, task_id, worker, tmux_session, analysis, flagged_output, \
                              review_state, created_at, resolved_at FROM incidents WHERE id = ?1";

/// The stored severity column duplicates `analysis.severity`. It exists so the
/// review UI can filter and order by severity without deserializing every
/// analysis blob; `analysis` stays the source of truth.
fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
    }
}

fn state_str(s: &IncidentReviewState) -> &'static str {
    match s {
        IncidentReviewState::PendingReview => "pending_review",
        IncidentReviewState::Resumed => "resumed",
        IncidentReviewState::Aborted => "aborted",
    }
}

fn parse_state(s: &str) -> anyhow::Result<IncidentReviewState> {
    match s {
        "pending_review" => Ok(IncidentReviewState::PendingReview),
        "resumed" => Ok(IncidentReviewState::Resumed),
        "aborted" => Ok(IncidentReviewState::Aborted),
        other => anyhow::bail!("unknown incident review_state '{other}' in the database"),
    }
}

fn parse_time(s: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

/// Row → `Incident`, with the JSON/enum decoding kept inside the returned
/// `Result` so a single corrupt row surfaces as an error instead of a panic
/// inside rusqlite's callback.
fn row_to_incident(row: &Row<'_>) -> rusqlite::Result<anyhow::Result<Incident>> {
    let analysis: String = row.get(4)?;
    let review_state: String = row.get(6)?;
    let created_at: String = row.get(7)?;
    let resolved_at: Option<String> = row.get(8)?;
    let id: String = row.get(0)?;
    let task_id: String = row.get(1)?;
    let worker: String = row.get(2)?;
    let tmux_session: String = row.get(3)?;
    let flagged_output: String = row.get(5)?;

    Ok((|| {
        Ok(Incident {
            id,
            task_id,
            worker,
            tmux_session,
            analysis: serde_json::from_str::<SafetyAnalysis>(&analysis)?,
            flagged_output,
            review_state: parse_state(&review_state)?,
            created_at: parse_time(&created_at)?,
            resolved_at: resolved_at.as_deref().map(parse_time).transpose()?,
        })
    })())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::SafetyCategory;

    fn analysis(severity: Severity) -> SafetyAnalysis {
        SafetyAnalysis {
            is_safe: false,
            severity,
            category: Some(SafetyCategory::DestructiveCommand),
            reason: "Tier-1 rule 'destructive' matched: rm -rf /".to_string(),
            suggested_action: "Pause the session and have a human review before resuming."
                .to_string(),
        }
    }

    fn store() -> IncidentStore {
        IncidentStore::in_memory().unwrap()
    }

    fn raise(store: &IncidentStore, session: &str, severity: Severity) -> Incident {
        let incident = new_incident("task-1", "cis-a6000", session, analysis(severity), "rm -rf /");
        store.record(&incident).unwrap();
        incident
    }

    #[test]
    fn an_incident_survives_the_round_trip() {
        let store = store();
        let raised = raise(&store, "hive-w1", Severity::Critical);

        let read = store.get(&raised.id).unwrap().expect("recorded");
        assert_eq!(read.id, raised.id);
        assert_eq!(read.task_id, "task-1");
        assert_eq!(read.worker, "cis-a6000");
        assert_eq!(read.tmux_session, "hive-w1");
        assert_eq!(read.analysis.severity, Severity::Critical);
        assert_eq!(read.analysis.category, Some(SafetyCategory::DestructiveCommand));
        assert_eq!(read.flagged_output, "rm -rf /");
        assert_eq!(read.review_state, IncidentReviewState::PendingReview);
        assert!(read.resolved_at.is_none());
    }

    #[test]
    fn recording_the_same_incident_twice_does_not_duplicate_it() {
        // A supervisor that crashes between raising and confirming replays the
        // write; the operator must not then see two rows for one event.
        let store = store();
        let incident = new_incident("t", "w", "s", analysis(Severity::High), "out");
        store.record(&incident).unwrap();
        store.record(&incident).unwrap();
        assert_eq!(store.pending().unwrap().len(), 1);
    }

    #[test]
    fn a_replayed_record_cannot_resurrect_a_decided_incident() {
        // The in-flight copy a retry holds still says PendingReview. If the
        // upsert wrote review_state back, an abort could be undone by a stale
        // retry landing after it.
        let store = store();
        let incident = raise(&store, "s", Severity::High);
        store.resolve(&incident.id, &HumanDecision::Abort).unwrap();

        store.record(&incident).unwrap();

        let read = store.get(&incident.id).unwrap().unwrap();
        assert_eq!(read.review_state, IncidentReviewState::Aborted);
        assert!(read.resolved_at.is_some());
        assert_eq!(store.pending_count().unwrap(), 0);
    }

    #[test]
    fn pending_lists_only_what_is_still_waiting() {
        let store = store();
        let a = raise(&store, "s-a", Severity::High);
        let b = raise(&store, "s-b", Severity::Low);
        assert_eq!(store.pending_count().unwrap(), 2);

        store.resolve(&a.id, &HumanDecision::Resume).unwrap();

        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, b.id);
        assert_eq!(store.recent(10).unwrap().len(), 2, "history keeps both");
    }

    #[test]
    fn resolving_records_the_decision_and_the_state_it_implies() {
        let store = store();

        let resumed = raise(&store, "s1", Severity::High);
        let out = store.resolve(&resumed.id, &HumanDecision::Resume).unwrap();
        assert_eq!(out.review_state, IncidentReviewState::Resumed);
        assert!(out.resolved_at.is_some());

        let aborted = raise(&store, "s2", Severity::High);
        let out = store.resolve(&aborted.id, &HumanDecision::Abort).unwrap();
        assert_eq!(out.review_state, IncidentReviewState::Aborted);

        // Both resume-shaped variants leave review, and the note survives for
        // whoever hands it back to the session.
        let noted = raise(&store, "s3", Severity::High);
        let out = store
            .resolve(&noted.id, &HumanDecision::ResumeWithNote("stay in /tmp".into()))
            .unwrap();
        assert_eq!(out.review_state, IncidentReviewState::Resumed);
        assert_eq!(
            store.decision(&noted.id).unwrap(),
            Some(HumanDecision::ResumeWithNote("stay in /tmp".into()))
        );

        let modified = raise(&store, "s4", Severity::High);
        store
            .resolve(&modified.id, &HumanDecision::ModifyAndResume("rm -rf ./build".into()))
            .unwrap();
        assert_eq!(
            store.decision(&modified.id).unwrap(),
            Some(HumanDecision::ModifyAndResume("rm -rf ./build".into()))
        );
    }

    #[test]
    fn an_incident_cannot_be_decided_twice() {
        // Two operators on the review page, or one retried request: resume and
        // abort racing on the same suspended process is exactly what the human
        // was asked to prevent. First writer wins, second is told who won.
        let store = store();
        let incident = raise(&store, "s", Severity::Critical);
        store.resolve(&incident.id, &HumanDecision::Resume).unwrap();

        let second = store.resolve(&incident.id, &HumanDecision::Abort);
        match second {
            Err(ResolveError::AlreadyResolved { id, state }) => {
                assert_eq!(id, incident.id);
                assert_eq!(state, "resumed");
            }
            other => panic!("expected AlreadyResolved, got {other:?}"),
        }

        // And the losing decision left no trace.
        assert_eq!(
            store.decision(&incident.id).unwrap(),
            Some(HumanDecision::Resume)
        );
    }

    #[test]
    fn resolving_an_unknown_incident_is_not_found_not_already_resolved() {
        let store = store();
        match store.resolve("no-such-id", &HumanDecision::Resume) {
            Err(ResolveError::NotFound(HiveError::IncidentNotFound(id))) => {
                assert_eq!(id, "no-such-id")
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn incidents_are_queryable_by_session() {
        let store = store();
        raise(&store, "hive-w1", Severity::High);
        raise(&store, "hive-w1", Severity::Low);
        raise(&store, "hive-w2", Severity::High);

        assert_eq!(store.for_session("hive-w1").unwrap().len(), 2);
        assert_eq!(store.for_session("hive-w2").unwrap().len(), 1);
        assert!(store.for_session("hive-w9").unwrap().is_empty());
    }

    #[test]
    fn the_log_persists_across_reopening() {
        // The whole point: a restart of the supervising process must not forget
        // what is awaiting review.
        let dir = std::env::temp_dir().join(format!("hive-incidents-{}", uuid::Uuid::new_v4()));
        let path = dir.join("hive.db");

        let id = {
            let store = IncidentStore::open(&path).unwrap();
            raise(&store, "s", Severity::Critical).id
        };

        let reopened = IncidentStore::open(&path).unwrap();
        assert_eq!(reopened.pending().unwrap().len(), 1);
        assert_eq!(reopened.get(&id).unwrap().unwrap().id, id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_database_is_not_world_readable() {
        // flagged_output holds whatever tripped the rule — including, for a
        // CredentialExposure hit, the credential itself.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("hive-incidents-{}", uuid::Uuid::new_v4()));
        let path = dir.join("hive.db");
        let _store = IncidentStore::open(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "incident db was mode {mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
