//! A SQLite-backed [`MessageBus`] — the coordinator's mechanical half (`spec/HACP.md`
//! §13.1).
//!
//! The bus is content-blind. Everything here is about *shape*, *identity*, and *order*:
//! does the envelope have the fields §5 requires, is the sender allowed to claim that
//! `from`, has this `message_id` been seen, and what number does it get in the run's total
//! order. Nothing in this file looks inside a `body`, and nothing here decides whether a
//! message is a good idea — that is the arbiter's job, and mixing the two is how a
//! transport starts quietly editing a protocol.
//!
//! The one rule that is easy to get backwards: an **unregistered kind is not an error**
//! (§5). A kind the registry does not know is persisted and delivered untouched, because
//! that is the protocol's only forward-compatibility mechanism. Rejecting it would make
//! every future minor version a breaking change.
//!
//! Persistence follows the pattern already established by [`crate::memory::graph`]: one
//! `rusqlite::Connection` behind an `Arc<Mutex<_>>`, caller-chosen ids, statements small
//! enough to be read at a glance.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hacp::envelope::urn;
use hacp::state::{RunLimits, RunState};
use hacp::{Envelope, EnvelopeError, MessageKind};
use rusqlite::{params, Connection, OptionalExtension};

use super::{Delivery, Ingested, MessageBus, Result, Sequenced};

/// The durable message log for every run on this coordinator.
///
/// Cloning shares the same connection, so a clone handed to another task sees the same
/// sequence numbers rather than a second, independently-numbered log.
#[derive(Clone)]
pub struct SqliteBus {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteBus {
    /// Open (creating if needed) a bus at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// An ephemeral in-memory bus. Tests use it so they need no filesystem; it is not
    /// suitable for a real run, because §13.1 requires the log to be durable.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // `from` and `to` are SQL keywords, hence the `_urn` suffixes; the envelope field
        // names are unchanged on the wire.
        //
        // `protocol` is stored even though it is not part of the routing triple: §5
        // accepts any higher minor version, and `history` is an audit record. Restamping
        // a replayed message with *this* build's version would quietly rewrite what the
        // sender actually said.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS runs (
                 run_id      TEXT PRIMARY KEY,
                 goal        TEXT NOT NULL,
                 state       TEXT NOT NULL,
                 limits_json TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS messages (
                 run_id      TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                 seq         INTEGER NOT NULL,
                 message_id  TEXT NOT NULL,
                 protocol    TEXT NOT NULL,
                 from_urn    TEXT NOT NULL,
                 to_urn      TEXT NOT NULL,
                 kind        TEXT NOT NULL,
                 in_reply_to TEXT,
                 timestamp   TEXT NOT NULL,
                 body_json   TEXT NOT NULL,
                 PRIMARY KEY (run_id, seq)
             );
             CREATE UNIQUE INDEX IF NOT EXISTS messages_dedupe
                 ON messages(run_id, message_id);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl MessageBus for SqliteBus {
    async fn create_run(&self, run_id: &str, goal: &str, limits: RunLimits) -> Result<()> {
        let limits_json = serde_json::to_string(&limits)?;
        let conn = self.conn.lock().unwrap();
        // `OR IGNORE`, not `OR REPLACE`: re-registering a run that is already `working`
        // must not rewind it to `formation` or overwrite the goal it is working on.
        conn.execute(
            "INSERT OR IGNORE INTO runs (run_id, goal, state, limits_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id,
                goal,
                state_to_text(RunState::Formation)?,
                limits_json
            ],
        )?;
        Ok(())
    }

    async fn ingest(&self, sender_role: &str, envelope: Envelope) -> Result<Ingested> {
        // Shape before identity: a malformed envelope has no trustworthy `from` to bind a
        // role against.
        if let Err(err) = envelope.validate() {
            let code = match err {
                // §5 names this one specifically, because the sender is owed a
                // `supported_versions` list rather than a generic complaint.
                EnvelopeError::ProtocolMismatch { .. } => "protocol-version",
                _ => "malformed-envelope",
            };
            return Ok(Ingested::Rejected {
                code: code.to_string(),
                detail: err.to_string(),
            });
        }

        if !role_binds(sender_role, &envelope.from) {
            return Ok(Ingested::Rejected {
                code: "role-mismatch".to_string(),
                detail: format!(
                    "credentials for role {sender_role:?} may not send as {:?}",
                    envelope.from
                ),
            });
        }

        let conn = self.conn.lock().unwrap();

        // Without this the foreign key would fail with a SQLite error, which tells the
        // sender nothing it can act on.
        let known_run: Option<String> = conn
            .query_row(
                "SELECT run_id FROM runs WHERE run_id = ?1",
                params![envelope.run_id],
                |r| r.get(0),
            )
            .optional()?;
        if known_run.is_none() {
            return Ok(Ingested::Rejected {
                code: "unknown-run".to_string(),
                detail: format!("no run {:?} on this coordinator", envelope.run_id),
            });
        }

        // At-least-once edges make redelivery ordinary traffic (§5). The *original* seq
        // goes back, not a fresh one: the sender's cursor must land where the message
        // actually sits in the total order.
        let existing: Option<i64> = conn
            .query_row(
                "SELECT seq FROM messages WHERE run_id = ?1 AND message_id = ?2",
                params![envelope.run_id, envelope.message_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            return Ok(Ingested::Duplicate { seq: seq as u64 });
        }

        // Holding the connection lock across the read-then-insert is what makes seq
        // assignment atomic; two concurrent ingests cannot both read the same maximum.
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE run_id = ?1",
            params![envelope.run_id],
            |r| r.get(0),
        )?;

        // No inspection of `kind` here, deliberately: an unregistered kind is persisted
        // and delivered like any other (§5).
        conn.execute(
            "INSERT INTO messages
                 (run_id, seq, message_id, protocol, from_urn, to_urn, kind,
                  in_reply_to, timestamp, body_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                envelope.run_id,
                next,
                envelope.message_id,
                envelope.protocol,
                envelope.from,
                envelope.to,
                envelope.kind.as_str(),
                envelope.in_reply_to,
                envelope.timestamp.to_rfc3339(),
                envelope.body.to_string(),
            ],
        )?;
        Ok(Ingested::Accepted { seq: next as u64 })
    }

    async fn poll(&self, run_id: &str, who: &str, since: u64) -> Result<Delivery> {
        let conn = self.conn.lock().unwrap();
        let state = read_state(&conn, run_id)?;

        let mut stmt = conn.prepare(
            "SELECT seq, message_id, protocol, from_urn, to_urn, kind, in_reply_to,
                    timestamp, body_json
             FROM messages
             WHERE run_id = ?1 AND seq > ?2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![run_id, since as i64], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row_to_envelope(run_id, row)?))
        })?;

        let mut cursor = since;
        let mut messages = Vec::new();
        for row in rows {
            let (seq, envelope) = row?;
            // The cursor advances past messages this caller is not shown, or a bystander
            // would re-read the same withheld peer traffic on every poll and never make
            // progress.
            cursor = seq;
            // Peer privacy (§6) is the envelope's own rule. Calling it here rather than
            // filtering in SQL keeps one definition of who may see what.
            if envelope.deliverable_to(who) {
                // The row's own seq travels with the envelope. The cursor above cannot
                // stand in for it: the numbers this caller sees have gaps wherever a
                // withheld message sits, and §13.2 names each INBOX file by the real
                // sequence number.
                messages.push(Sequenced { seq, envelope });
            }
        }

        Ok(Delivery {
            state,
            seq: cursor,
            messages,
        })
    }

    async fn history(&self, run_id: &str) -> Result<Vec<Envelope>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, message_id, protocol, from_urn, to_urn, kind, in_reply_to,
                    timestamp, body_json
             FROM messages
             WHERE run_id = ?1
             ORDER BY seq",
        )?;
        // Unfiltered on purpose: this is the audit trail, which includes the peer traffic
        // no worker was shown.
        let rows = stmt.query_map(params![run_id], |row| row_to_envelope(run_id, row))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    async fn state(&self, run_id: &str) -> Result<RunState> {
        let conn = self.conn.lock().unwrap();
        read_state(&conn, run_id)
    }

    async fn set_state(&self, run_id: &str, next: RunState) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let current = read_state(&conn, run_id)?;
        // No self-transition shortcut: `Drafted -> Drafted` is legal and `Working ->
        // Working` is not, and only the table knows which is which.
        if !current.can_transition_to(next) {
            anyhow::bail!("run {run_id}: illegal transition {current} -> {next}");
        }
        conn.execute(
            "UPDATE runs SET state = ?2 WHERE run_id = ?1",
            params![run_id, state_to_text(next)?],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Whether credentials issued for `sender_role` may claim `from` (§13.3).
///
/// A worker's role id is embedded in its URN, so the check is exact. The coordinator and
/// the arbiter have no role id — their URN *is* their identity — so they present it whole.
/// The `urn` module keeps its prefixes private and re-spelling them here would let the two
/// drift, which is why this compares against the full URN rather than parsing one.
fn role_binds(sender_role: &str, from: &str) -> bool {
    if sender_role == from {
        return true;
    }
    matches!(urn::parse_agent(from), Some((role, _)) if role == sender_role)
}

/// The run's state, or an error naming the run — an unknown run is a caller mistake, not
/// an empty result.
fn read_state(conn: &Connection, run_id: &str) -> Result<RunState> {
    let text: Option<String> = conn
        .query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional()?;
    match text {
        Some(text) => state_from_text(&text),
        None => anyhow::bail!("no run {run_id:?} on this bus"),
    }
}

/// Serde's snake_case spelling, not [`RunState::label`]: the label is prose ("timed out")
/// and would not round-trip.
fn state_to_text(state: RunState) -> Result<String> {
    match serde_json::to_value(state)? {
        serde_json::Value::String(s) => Ok(s),
        other => anyhow::bail!("RunState did not serialize as a string: {other}"),
    }
}

fn state_from_text(text: &str) -> Result<RunState> {
    Ok(serde_json::from_value(serde_json::Value::String(
        text.to_string(),
    ))?)
}

/// Rebuild an envelope from a row of `messages`.
///
/// `run_id` is passed in rather than selected: every caller already filters on it, so
/// carrying it through the row would be a column read per message for a value that cannot
/// differ.
fn row_to_envelope(run_id: &str, row: &rusqlite::Row) -> rusqlite::Result<Envelope> {
    let timestamp: String = row.get(7)?;
    let body: String = row.get(8)?;
    Ok(Envelope {
        protocol: row.get(2)?,
        message_id: row.get(1)?,
        run_id: run_id.to_string(),
        from: row.get(3)?,
        to: row.get(4)?,
        kind: MessageKind::new(row.get::<_, String>(5)?),
        in_reply_to: row.get(6)?,
        timestamp: parse_timestamp(&timestamp, 7)?,
        body: serde_json::from_str(&body).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?,
    })
}

fn parse_timestamp(text: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RUN: &str = "run-8a41";

    fn agent(role: &str) -> String {
        urn::agent(role, "8a41")
    }

    async fn bus() -> SqliteBus {
        let bus = SqliteBus::in_memory().expect("in-memory bus opens");
        bus.create_run(RUN, "ship the thing", RunLimits::default())
            .await
            .unwrap();
        bus
    }

    fn envelope(from: &str, to: &str, kind: &str) -> Envelope {
        Envelope::new(RUN, from, to, kind, json!({"text": "hi"}))
    }

    #[tokio::test]
    async fn create_run_does_not_rewind_a_run_in_flight() {
        let bus = bus().await;
        bus.set_state(RUN, RunState::Planning).await.unwrap();
        bus.create_run(RUN, "a different goal", RunLimits::default())
            .await
            .unwrap();

        assert_eq!(bus.state(RUN).await.unwrap(), RunState::Planning);
        assert_eq!(bus.history(RUN).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn assigns_a_monotonic_per_run_seq() {
        let bus = bus().await;
        let a = agent("api");
        for expected in 1..=3 {
            let got = bus
                .ingest("api", envelope(&a, urn::ALL, "work.started"))
                .await
                .unwrap();
            assert_eq!(got, Ingested::Accepted { seq: expected });
        }
    }

    #[tokio::test]
    async fn duplicate_returns_the_original_seq() {
        let bus = bus().await;
        let a = agent("api");
        bus.ingest("api", envelope(&a, urn::ALL, "work.started"))
            .await
            .unwrap();

        let once = envelope(&a, urn::ALL, "artifact.published");
        let seq = match bus.ingest("api", once.clone()).await.unwrap() {
            Ingested::Accepted { seq } => seq,
            other => panic!("expected acceptance, got {other:?}"),
        };
        assert_eq!(seq, 2);

        // Two more redeliveries, with an unrelated message in between, so a wrong
        // implementation returning "the latest seq" would be caught.
        assert_eq!(
            bus.ingest("api", once.clone()).await.unwrap(),
            Ingested::Duplicate { seq: 2 }
        );
        bus.ingest("api", envelope(&a, urn::ALL, "heartbeat"))
            .await
            .unwrap();
        assert_eq!(
            bus.ingest("api", once).await.unwrap(),
            Ingested::Duplicate { seq: 2 }
        );
        assert_eq!(bus.history(RUN).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn an_unregistered_kind_is_persisted_and_delivered() {
        let bus = bus().await;
        let a = agent("api");
        let kind = "contract.telepathy.requested";
        assert!(!MessageKind::new(kind).is_registered());

        assert!(matches!(
            bus.ingest("api", envelope(&a, urn::ALL, kind)).await.unwrap(),
            Ingested::Accepted { .. }
        ));

        let delivered = bus.poll(RUN, &agent("store"), 0).await.unwrap();
        assert_eq!(delivered.messages.len(), 1);
        assert_eq!(delivered.messages[0].envelope.kind.as_str(), kind);
        // The body must survive untouched — a bus that "normalizes" an unknown body is
        // no longer content-blind.
        assert_eq!(delivered.messages[0].envelope.body, json!({"text": "hi"}));
    }

    #[tokio::test]
    async fn rejects_a_sender_speaking_as_someone_else() {
        let bus = bus().await;
        let rejection = bus
            .ingest("api", envelope(&agent("store"), urn::ALL, "work.started"))
            .await
            .unwrap();
        match rejection {
            Ingested::Rejected { code, .. } => assert_eq!(code, "role-mismatch"),
            other => panic!("expected rejection, got {other:?}"),
        }
        assert_eq!(bus.history(RUN).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn the_coordinator_binds_against_its_own_urn() {
        let bus = bus().await;
        let coord = urn::coordinator("hive");
        assert!(matches!(
            bus.ingest(&coord, envelope(&coord, urn::ALL, "run.started"))
                .await
                .unwrap(),
            Ingested::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn a_differing_major_version_is_a_protocol_version_rejection() {
        let bus = bus().await;
        let a = agent("api");
        let mut env = envelope(&a, urn::ALL, "work.started");
        env.protocol = "HACP/2.0".to_string();
        match bus.ingest("api", env).await.unwrap() {
            Ingested::Rejected { code, .. } => assert_eq!(code, "protocol-version"),
            other => panic!("expected rejection, got {other:?}"),
        }

        // A higher *minor* is additive and must be accepted (§5).
        let mut future = envelope(&a, urn::ALL, "work.started");
        future.protocol = "HACP/1.9".to_string();
        assert!(matches!(
            bus.ingest("api", future).await.unwrap(),
            Ingested::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn peer_traffic_is_hidden_from_bystanders() {
        let bus = bus().await;
        let (api, store, ui) = (agent("api"), agent("store"), agent("ui"));
        bus.ingest("api", envelope(&api, &store, "peer.question"))
            .await
            .unwrap();
        bus.ingest("api", envelope(&api, urn::ALL, "work.started"))
            .await
            .unwrap();

        let seen = |d: Delivery| -> Vec<String> {
            d.messages
                .iter()
                .map(|m| m.envelope.kind.0.clone())
                .collect()
        };
        assert_eq!(
            seen(bus.poll(RUN, &store, 0).await.unwrap()),
            ["peer.question", "work.started"]
        );
        assert_eq!(
            seen(bus.poll(RUN, &api, 0).await.unwrap()),
            ["peer.question", "work.started"]
        );
        assert_eq!(seen(bus.poll(RUN, &ui, 0).await.unwrap()), ["work.started"]);
        assert_eq!(
            seen(bus.poll(RUN, &urn::arbiter("hive"), 0).await.unwrap()),
            ["peer.question", "work.started"]
        );
        // Withheld from a bystander, but still in the audit trail.
        assert_eq!(bus.history(RUN).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_cursor_advances_past_withheld_messages() {
        let bus = bus().await;
        let (api, store, ui) = (agent("api"), agent("store"), agent("ui"));
        bus.ingest("api", envelope(&api, &store, "peer.question"))
            .await
            .unwrap();

        let first = bus.poll(RUN, &ui, 0).await.unwrap();
        assert!(first.messages.is_empty());
        // Were the cursor to stay at 0, the bystander would rescan the peer message
        // forever instead of making progress.
        assert_eq!(first.seq, 1);

        let second = bus.poll(RUN, &ui, first.seq).await.unwrap();
        assert!(second.messages.is_empty());
        assert_eq!(second.seq, 1);
    }

    #[tokio::test]
    async fn a_bystander_receives_the_buss_own_non_contiguous_seqs() {
        let bus = bus().await;
        let (api, store, ui) = (agent("api"), agent("store"), agent("ui"));

        // Interleave withheld peer traffic with broadcasts so the bystander's numbers
        // must have holes in them.
        bus.ingest("api", envelope(&api, &store, "peer.question"))
            .await
            .unwrap();
        bus.ingest("api", envelope(&api, urn::ALL, "work.started"))
            .await
            .unwrap();
        bus.ingest(
            "store",
            envelope(&store, &api, "peer.answer").with_in_reply_to("m-cause"),
        )
        .await
        .unwrap();
        bus.ingest("api", envelope(&api, urn::ALL, "artifact.published"))
            .await
            .unwrap();
        bus.ingest("api", envelope(&api, &store, "peer.proposal"))
            .await
            .unwrap();
        bus.ingest("api", envelope(&api, urn::ALL, "heartbeat"))
            .await
            .unwrap();

        let delivered = bus.poll(RUN, &ui, 0).await.unwrap();
        let seqs: Vec<u64> = delivered.messages.iter().map(|m| m.seq).collect();
        // Holes where the peer traffic sits. This is load-bearing for the file edge:
        // §13.2 names each INBOX file by the message's sequence number, so an adapter
        // that renumbered these 1,2,3 would rename every message it wrote.
        assert_eq!(seqs, [2, 4, 6]);

        // And they are the bus's own numbers, not a per-caller ordinal: the position in
        // the full audit log must agree, message for message.
        let history = bus.history(RUN).await.unwrap();
        for message in &delivered.messages {
            let position = history
                .iter()
                .position(|h| h.message_id == message.envelope.message_id)
                .expect("a delivered message is in the history");
            assert_eq!(message.seq, position as u64 + 1);
        }

        // A delivered seq is a usable cursor even though the sequence has gaps.
        let rest = bus.poll(RUN, &ui, seqs[0]).await.unwrap();
        assert_eq!(
            rest.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
            [4, 6]
        );

        // An endpoint of the peer traffic sees the same numbers, with nothing missing.
        let endpoint = bus.poll(RUN, &store, 0).await.unwrap();
        assert_eq!(
            endpoint.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
    }

    #[tokio::test]
    async fn poll_reports_the_run_state_and_respects_the_cursor() {
        let bus = bus().await;
        let api = agent("api");
        bus.ingest("api", envelope(&api, urn::ALL, "work.started"))
            .await
            .unwrap();
        bus.set_state(RUN, RunState::Planning).await.unwrap();

        let d = bus.poll(RUN, &api, 1).await.unwrap();
        assert!(d.messages.is_empty());
        assert_eq!(d.state, RunState::Planning);
        assert_eq!(d.seq, 1);
    }

    #[tokio::test]
    async fn an_illegal_transition_is_refused_where_it_happens() {
        let bus = bus().await;
        // formation -> completed skips every check that makes "completed" mean anything.
        let err = bus.set_state(RUN, RunState::Completed).await.unwrap_err();
        assert!(err.to_string().contains("illegal transition"), "{err}");
        assert_eq!(bus.state(RUN).await.unwrap(), RunState::Formation);

        for next in [
            RunState::Planning,
            RunState::Drafted,
            RunState::Frozen,
            RunState::Working,
            RunState::Reporting,
            RunState::Verifying,
            RunState::Integrating,
            RunState::Completed,
        ] {
            bus.set_state(RUN, next).await.unwrap();
        }
        assert_eq!(bus.state(RUN).await.unwrap(), RunState::Completed);

        // Terminal means terminal, including for the "always reachable" failure states.
        assert!(bus.set_state(RUN, RunState::Failed).await.is_err());
    }

    #[tokio::test]
    async fn a_message_for_an_unknown_run_is_rejected_not_a_sql_error() {
        let bus = bus().await;
        let mut env = envelope(&agent("api"), urn::ALL, "work.started");
        env.run_id = "run-nope".to_string();
        match bus.ingest("api", env).await.unwrap() {
            Ingested::Rejected { code, .. } => assert_eq!(code, "unknown-run"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn envelopes_round_trip_through_storage() {
        let bus = bus().await;
        let api = agent("api");
        let sent = envelope(&api, urn::ALL, "answer").with_in_reply_to("m-cause");
        bus.ingest("api", sent.clone()).await.unwrap();

        let back = bus.history(RUN).await.unwrap().pop().unwrap();
        assert_eq!(back.message_id, sent.message_id);
        assert_eq!(back.protocol, sent.protocol);
        assert_eq!(back.from, sent.from);
        assert_eq!(back.to, sent.to);
        assert_eq!(back.kind, sent.kind);
        assert_eq!(back.in_reply_to.as_deref(), Some("m-cause"));
        assert_eq!(back.body, sent.body);
        assert_eq!(back.timestamp, sent.timestamp);
    }

    #[tokio::test]
    async fn a_shape_violation_is_not_reported_as_a_version_problem() {
        let bus = bus().await;
        // `answer` requires in_reply_to (§5), and this one has none.
        let env = envelope(&agent("api"), urn::ALL, "answer");
        match bus.ingest("api", env).await.unwrap() {
            Ingested::Rejected { code, detail } => {
                assert_eq!(code, "malformed-envelope");
                assert!(detail.contains("in_reply_to"), "{detail}");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runs_are_numbered_independently() {
        let bus = bus().await;
        bus.create_run("run-other", "another goal", RunLimits::default())
            .await
            .unwrap();
        let api = agent("api");
        bus.ingest("api", envelope(&api, urn::ALL, "work.started"))
            .await
            .unwrap();

        let mut env = envelope(&api, urn::ALL, "work.started");
        env.run_id = "run-other".to_string();
        assert_eq!(
            bus.ingest("api", env).await.unwrap(),
            Ingested::Accepted { seq: 1 }
        );
    }
}
