use super::{coordination::{self, AgreementRecord, CheckMeasurement, CompletionAssessment, CompletionVerdict}, relay, Assignment, DelegationPlan};
use hacp::v2::ContractState;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    pub conversation_id: String,
    pub assignment: Assignment,
    pub tmux_name: String,
    pub state: String,
    pub metadata: Value,
    pub cursor: i64,
    pub runner_path: Option<String>,
    pub review: Value,
    pub contracts: Vec<AgreementRecord>,
    pub completion: Option<CompletionAssessment>,
    pub identity: relay::PublicIdentity,
    pub relay: Value,
}

/// A message was refused because its ID already names a different envelope, or
/// its route is not allowed. The refusal is already recorded as a single
/// incident, so a sync loop may continue past it.
#[derive(Debug)]
pub struct MessageRejected(pub String);
impl std::fmt::Display for MessageRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for MessageRejected {}

#[derive(Clone)]
pub struct RunStore(Arc<Mutex<Connection>>, relay::Budget);

fn completion_for(db: &Connection, run: &str) -> anyhow::Result<Option<CompletionAssessment>> {
    let saved: Option<String> = db.query_row(
        "SELECT assessment FROM delegated_completions WHERE run_id=?", [run], |row| row.get(0)
    ).optional()?;
    saved.map(|s| serde_json::from_str(&s).map_err(Into::into)).transpose()
}

// Message IDs are delivery identities. An exact replay is harmless, but silently
// accepting a changed envelope could acknowledge work that was never delivered.
fn insert_message(
    db: &Connection,
    id: &str,
    source: &str,
    destination: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !id.is_empty() && payload["id"].as_str() == Some(id),
        "Message ID must match its payload"
    );
    let inserted = db.execute(
        "INSERT OR IGNORE INTO delegated_messages(id,source,destination,payload) VALUES (?,?,?,?)",
        params![id, source, destination, payload.to_string()],
    )?;
    let existing: (String, String, String) = db.query_row(
        "SELECT source,destination,payload FROM delegated_messages WHERE id=?",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    anyhow::ensure!(
        existing.0 == source
            && existing.1 == destination
            && serde_json::from_str::<Value>(&existing.2)? == *payload,
        "Message ID already belongs to a different envelope"
    );
    if inserted == 1 {
        relay::stage(db, id, source, destination, payload, chrono::Utc::now().timestamp())?;
    }
    Ok(())
}

// A forbidden route is refused before storage, so there is no stored message to
// attribute it to; the caller's routing is recorded instead. Recorded at most once
// per (message, reason) so a journal replaying the same event on every sync
// cannot grow the append-only chain.
fn reject_route(
    db: &Connection,
    id: &str,
    source: &str,
    destination: &str,
    task_id: &str,
    reason: &str,
    now: i64,
) -> anyhow::Result<()> {
    let seen: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM delegated_relay_incidents WHERE message_id=? AND reason=?)",
        params![id, reason],
        |r| r.get(0),
    )?;
    if seen {
        return Ok(());
    }
    let at = chrono::DateTime::from_timestamp(now, 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid relay time"))?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    db.execute("INSERT INTO delegated_relay_incidents(source,destination,message_id,kind,reason,created_at) VALUES(?,?,?,'route',?,?)", params![source, destination, id, reason, at])?;
    let envelope = relay::Envelope {
        id: id.into(),
        task_id: task_id.into(),
        source: source.into(),
        destination: destination.into(),
        kind: String::new(),
        text_digest: String::new(),
        seq: 0,
        staged_at: String::new(),
    };
    relay::audit(db, "reject", &envelope, json!({"kind":"route","reason":reason}), now)
}

fn decode_run_row(
    db: &Connection,
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
    ),
) -> anyhow::Result<Run> {
    let (id, task_id, conversation_id, a, tmux_name, state, m, cursor, runner_path, review) = row;
    let identity = relay::identity(db, &id)?;
    let relay = relay::status(db, &id)?;
    let assignment: Assignment = serde_json::from_str(&a)?;
    let metadata: Value = serde_json::from_str(&m)?;
    let review_val = match review.map(|s| serde_json::from_str::<Value>(&s)).transpose() {
        Ok(rev) => rev.unwrap_or(Value::Null),
        Err(e) => return Err(e.into()),
    };
    let completion = completion_for(db, &id)?;
    Ok(Run {
        completion,
        identity,
        relay,
        id,
        task_id,
        conversation_id,
        assignment,
        tmux_name,
        state,
        metadata,
        cursor,
        runner_path,
        review: review_val,
        contracts: vec![],
    })
}

impl RunStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> anyhow::Result<Self> {
        let _ = conn.lock().unwrap().execute_batch("CREATE TABLE IF NOT EXISTS delegated_runs (
          id TEXT PRIMARY KEY, task_id TEXT NOT NULL, conversation_id TEXT NOT NULL,
          assignment TEXT NOT NULL, tmux_name TEXT NOT NULL, state TEXT NOT NULL,
          metadata TEXT NOT NULL DEFAULT '{}', cursor INTEGER NOT NULL DEFAULT 0, runner_path TEXT,
          UNIQUE(task_id, assignment));
          CREATE TABLE IF NOT EXISTS delegated_events (run_id TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(run_id,id));
          CREATE TABLE IF NOT EXISTS delegated_decisions (run_id TEXT NOT NULL, id TEXT NOT NULL, fingerprint TEXT NOT NULL, decision TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(run_id,id));
          CREATE TABLE IF NOT EXISTS delegated_messages (id TEXT PRIMARY KEY, source TEXT NOT NULL, destination TEXT NOT NULL, payload TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS delegated_completions (run_id TEXT PRIMARY KEY, assessment TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS delegated_contracts (task_id TEXT NOT NULL, party_a TEXT NOT NULL, party_b TEXT NOT NULL, contract TEXT NOT NULL, PRIMARY KEY(task_id,party_a,party_b));");
        {
            let db = conn.lock().unwrap();
            let _ = db.execute_batch("CREATE TABLE IF NOT EXISTS delegated_reviews(task_id TEXT PRIMARY KEY,cursor TEXT,status TEXT NOT NULL DEFAULT 'reviewing',summary TEXT NOT NULL DEFAULT '',lock_until INTEGER NOT NULL DEFAULT 0,continue_reviews INTEGER NOT NULL DEFAULT 0,claim_token TEXT NOT NULL DEFAULT '');");
            // A database created before review rounds were bounded has no counter yet.
            if db.prepare("SELECT continue_reviews FROM delegated_reviews").is_err() {
                let _ = db.execute_batch("ALTER TABLE delegated_reviews ADD COLUMN continue_reviews INTEGER NOT NULL DEFAULT 0;");
            }
            if db.prepare("SELECT claim_token FROM delegated_reviews").is_err() {
                let _ = db.execute_batch("ALTER TABLE delegated_reviews ADD COLUMN claim_token TEXT NOT NULL DEFAULT '';");
            }
        }
        {
            let mut db = conn.lock().unwrap();
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate);
            if let Ok(tx) = tx {
                let _ = relay::schema(&tx);
                // Existing runs gain identities; unsigned historical messages are
                // never retroactively signed and will fail closed before delivery.
                let ids = {
                    let mut ids = Vec::new();
                    if let Ok(mut stmt) = tx.prepare("SELECT id FROM delegated_runs WHERE id NOT IN (SELECT run_id FROM delegated_relay_keys)") {
                        if let Ok(mut rows) = stmt.query([]) {
                            while let Ok(Some(r)) = rows.next() {
                                if let Ok(id) = r.get::<_, String>(0) {
                                    ids.push(id);
                                }
                            }
                        }
                    }
                    ids
                };
                for id in ids { let _ = relay::create_identity(&tx, &id); }
                let _ = tx.commit();
            }
        }
        Ok(Self(conn, relay::Budget::from_env()?))
    }
    pub fn create(
        &self,
        task: &str,
        conversation: &str,
        plan: &DelegationPlan,
    ) -> anyhow::Result<Vec<Run>> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM delegated_runs WHERE task_id=?",
            [task],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            count == 0,
            "Task already has assignments; reconcile existing runs"
        );
        let mut inserted_ids = Vec::with_capacity(plan.assignments.len());
        for assignment in &plan.assignments {
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO delegated_runs(id,task_id,conversation_id,assignment,tmux_name,state) VALUES (?,?,?,?,?,?)", params![id,task,conversation,serde_json::to_string(assignment)?,format!("hive-agent-{id}"),"queued"])?;
            relay::create_identity(&tx, &id)?;
            inserted_ids.push(id);
        }
        tx.commit()?;
        drop(db);

        anyhow::ensure!(
            plan.assignments.is_empty() || !inserted_ids.is_empty(),
            "Plan containing assignments yielded zero runs"
        );

        let mut runs = Vec::with_capacity(inserted_ids.len());
        for id in &inserted_ids {
            runs.push(self.get(id)?);
        }
        anyhow::ensure!(
            runs.len() == inserted_ids.len(),
            "Plan containing assignments yielded zero runs"
        );
        Ok(runs)
    }
    pub fn claim_review(&self, task: &str, cursor: &str) -> anyhow::Result<Option<String>> {
        let db = self.0.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        let token = uuid::Uuid::new_v4().to_string();
        let changed = db.execute(
            "INSERT INTO delegated_reviews(task_id,cursor,lock_until,claim_token) VALUES (?1,?2,?3,?4)
             ON CONFLICT(task_id) DO UPDATE SET cursor=excluded.cursor,status='reviewing',lock_until=excluded.lock_until,claim_token=excluded.claim_token
             WHERE delegated_reviews.lock_until<?5 AND (delegated_reviews.cursor!=excluded.cursor OR delegated_reviews.status='reviewing')",
            params![task, cursor, now + 240, token, now],
        )?;
        Ok(if changed == 1 { Some(token) } else { None })
    }
    /// Consecutive `continue` rounds already spent on a task. Reset whenever a
    /// review settles the task, so the bound only ever counts real rounds.
    pub fn continue_reviews(&self, task: &str) -> anyhow::Result<i64> {
        let db = self.0.lock().unwrap();
        Ok(db.query_row(
            "SELECT COALESCE((SELECT continue_reviews FROM delegated_reviews WHERE task_id=?),0)",
            [task],
            |row| row.get(0),
        )?)
    }
    pub fn finish_review(
        &self,
        task: &str,
        cursor: &str,
        claim_token: &str,
        status: &str,
        summary: &str,
        messages: &[(String, Value)],
    ) -> anyhow::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        anyhow::ensure!(
            tx.execute(
                "UPDATE delegated_reviews SET status=?,summary=?,lock_until=0,claim_token='',continue_reviews=CASE WHEN ?='continue' THEN continue_reviews+1 ELSE 0 END WHERE task_id=? AND cursor=? AND claim_token=?",
                params![status, summary, status, task, cursor, claim_token]
            )? == 1,
            "Review was superseded"
        );
        for (destination, payload) in messages {
            let same_task: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM delegated_runs WHERE id=? AND task_id=? AND state!='superseded')",
                params![destination, task],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                same_task,
                "Review message destination is not active in this task"
            );
            let id = payload["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Message ID required"))?;
            insert_message(&tx, id, "coordinator", destination, payload)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn retry_setup(&self, id: &str) -> anyhow::Result<()> {
        let changed = self.0.lock().unwrap().execute(
            "UPDATE delegated_runs SET state='queued' WHERE id=? AND runner_path IS NULL AND state IN ('needs-setup','disconnected')",
            [id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "Only a run that never launched can retry setup"
        );
        Ok(())
    }

    pub fn retry_launching(&self, id: &str, timeout_secs: i64) -> anyhow::Result<()> {
        let run = self.get(id)?;
        anyhow::ensure!(
            run.state == "launching",
            "Only a launching run can retry setup as launching"
        );
        let claimed_at = run
            .metadata
            .get("claimed_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let now = chrono::Utc::now().timestamp();
        anyhow::ensure!(
            now.saturating_sub(claimed_at) >= timeout_secs,
            "Launch is still within bounded timeout window; retry refused"
        );
        let changed = self.0.lock().unwrap().execute(
            "UPDATE delegated_runs SET state='queued',runner_path=NULL WHERE id=? AND state='launching'",
            [id],
        )?;
        anyhow::ensure!(changed == 1, "Run is no longer launching");
        Ok(())
    }
    pub fn replace(&self, id: &str, assignment: &Assignment) -> anyhow::Result<Run> {
        let old = self.get(id)?;
        if let Some(replacement) = old.metadata["replacement_id"].as_str() {
            let existing = self.get(replacement)?;
            anyhow::ensure!(
                existing.assignment == *assignment,
                "Assignment already moved elsewhere"
            );
            return Ok(existing);
        }
        let replacement = uuid::Uuid::new_v4().to_string();
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        anyhow::ensure!(tx.execute("UPDATE delegated_runs SET state='superseded',metadata=json_set(metadata,'$.replacement_id',?) WHERE id=? AND state!='superseded'",params![replacement,id])?==1,"Assignment already replaced");
        tx.execute("INSERT INTO delegated_runs(id,task_id,conversation_id,assignment,tmux_name,state) VALUES (?,?,?,?,?,'queued')",params![replacement,old.task_id,old.conversation_id,serde_json::to_string(assignment)?,format!("hive-agent-{replacement}")])?;
        relay::create_identity(&tx, &replacement)?;
        tx.commit()?;
        drop(db);
        self.get(&replacement)
    }
    pub fn list(&self) -> anyhow::Result<(Vec<Run>, usize)> {
        let db = self.0.lock().unwrap();
        let mut runs: Vec<Run> = Vec::new();
        let mut error_count: usize = 0;

        let mut last_rowid: i64 = 0;
        let max_rowid: i64 = db
            .query_row(
                "SELECT COALESCE(MAX(rowid), 0) FROM delegated_runs",
                [],
                |r| r.get(0),
            )
            .unwrap_or(i64::MAX);

        while last_rowid < max_rowid {
            let mut stmt = match db.prepare(
                "SELECT rowid, id, task_id, conversation_id, assignment, tmux_name, state, metadata, cursor, runner_path, \
                 (SELECT json_object('status',status,'summary',summary) FROM delegated_reviews WHERE delegated_reviews.task_id=delegated_runs.task_id) \
                 FROM delegated_runs WHERE rowid > ? ORDER BY rowid LIMIT 100",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error=%e, "skipping unreadable delegated run row");
                    error_count += 1;
                    break;
                }
            };

            let mut rows = match stmt.query([last_rowid]) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error=%e, "skipping unreadable delegated run row");
                    error_count += 1;
                    last_rowid += 1;
                    continue;
                }
            };

            let mut batch_read_any = false;
            let mut hit_corrupt_in_batch = false;

            loop {
                let row_result = rows.next();
                match row_result {
                    Ok(Some(r)) => {
                        let rowid: i64 = match r.get(0) {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::warn!(error=%e, "skipping unreadable delegated run row");
                                error_count += 1;
                                last_rowid += 1;
                                hit_corrupt_in_batch = true;
                                break;
                            }
                        };
                        last_rowid = rowid;
                        batch_read_any = true;

                        let tuple = (
                            r.get::<_, String>(1),
                            r.get::<_, String>(2),
                            r.get::<_, String>(3),
                            r.get::<_, String>(4),
                            r.get::<_, String>(5),
                            r.get::<_, String>(6),
                            r.get::<_, String>(7),
                            r.get::<_, i64>(8),
                            r.get::<_, Option<String>>(9),
                            r.get::<_, Option<String>>(10),
                        );
                        let tuple = match tuple {
                            (
                                Ok(t1),
                                Ok(t2),
                                Ok(t3),
                                Ok(t4),
                                Ok(t5),
                                Ok(t6),
                                Ok(t7),
                                Ok(t8),
                                Ok(t9),
                                Ok(t10),
                            ) => (t1, t2, t3, t4, t5, t6, t7, t8, t9, t10),
                            _ => {
                                tracing::warn!("skipping unreadable delegated run row");
                                error_count += 1;
                                continue;
                            }
                        };

                        match decode_run_row(&db, tuple) {
                            Ok(run) => runs.push(run),
                            Err(e) => {
                                tracing::warn!(error=%e, "skipping unreadable delegated run row");
                                error_count += 1;
                            }
                        }
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error=%e, "skipping unreadable delegated run row");
                        error_count += 1;
                        hit_corrupt_in_batch = true;
                        break;
                    }
                }
            }

            drop(rows);
            drop(stmt);

            if hit_corrupt_in_batch {
                let mut probed = false;
                let mut probe_id = last_rowid + 1;
                let probe_limit = if max_rowid < i64::MAX {
                    max_rowid
                } else {
                    last_rowid + 1000
                };
                while probe_id <= probe_limit {
                    let probe_res = db.query_row(
                        "SELECT rowid, id, task_id, conversation_id, assignment, tmux_name, state, metadata, cursor, runner_path, \
                         (SELECT json_object('status',status,'summary',summary) FROM delegated_reviews WHERE delegated_reviews.task_id=delegated_runs.task_id) \
                         FROM delegated_runs WHERE rowid = ?",
                        [probe_id],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                                r.get::<_, String>(5)?,
                                r.get::<_, String>(6)?,
                                r.get::<_, String>(7)?,
                                r.get::<_, i64>(8)?,
                                r.get::<_, Option<String>>(9)?,
                                r.get::<_, Option<String>>(10)?,
                            ))
                        },
                    );
                    match probe_res {
                        Ok((
                            rowid,
                            id,
                            task_id,
                            conversation_id,
                            assignment,
                            tmux_name,
                            state,
                            metadata,
                            cursor,
                            runner_path,
                            review,
                        )) => {
                            last_rowid = rowid;
                            let tuple = (
                                id,
                                task_id,
                                conversation_id,
                                assignment,
                                tmux_name,
                                state,
                                metadata,
                                cursor,
                                runner_path,
                                review,
                            );
                            match decode_run_row(&db, tuple) {
                                Ok(run) => runs.push(run),
                                Err(e) => {
                                    tracing::warn!(error=%e, "skipping unreadable delegated run row");
                                    error_count += 1;
                                }
                            }
                            probed = true;
                            break;
                        }
                        Err(rusqlite::Error::QueryReturnedNoRows) => {
                            probe_id += 1;
                        }
                        Err(e) => {
                            tracing::warn!(rowid=probe_id, error=%e, "skipping unreadable delegated run row");
                            error_count += 1;
                            probe_id += 1;
                        }
                    }
                }
                if !probed {
                    last_rowid = probe_limit;
                }
            } else if !batch_read_any {
                break;
            }
        }

        drop(db);
        for run in &mut runs {
            match self.contracts_for(&run.id) {
                Ok(contracts) => run.contracts = contracts,
                Err(error) => {
                    tracing::warn!(run_id=%run.id, error=%error, "failed to load contracts for run");
                }
            }
        }
        Ok((runs, error_count))
    }

    pub fn quick_check(path: &std::path::Path) -> bool {
        let Ok(conn) = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return false;
        };
        let res: Result<String, _> = conn.query_row("PRAGMA quick_check", [], |r| r.get(0));
        matches!(res, Ok(s) if s == "ok")
    }

    pub fn contracts_for(&self, run: &str) -> anyhow::Result<Vec<AgreementRecord>> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare("SELECT contract FROM delegated_contracts WHERE party_a=? OR party_b=? ORDER BY rowid")?;
        let rows = stmt.query_map([run, run], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Commit the coordinator's measurements once per settled native turn.
    /// The verdict and any rework message share one transaction, so a restart
    /// neither loses feedback nor consumes the same rework round twice.
    pub fn assess_completion(&self, id: &str, turn_seq: i64, measurements: &[CheckMeasurement]) -> anyhow::Result<bool> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, metadata, assignment): (String, String, String) = tx.query_row(
            "SELECT state,metadata,assignment FROM delegated_runs WHERE id=?", [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let metadata: Value = serde_json::from_str(&metadata)?;
        if state != "verifying" || metadata["acceptance_turn"].as_i64() != Some(turn_seq) {
            return Ok(false);
        }
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM delegated_messages WHERE destination=? AND delivered=0 AND id NOT IN (SELECT message_id FROM delegated_relay_envelopes WHERE rejected=1))",
            [id], |r| r.get(0))?;
        if pending {
            return Ok(false);
        }
        let previous = completion_for(&tx, id)?;
        if previous.as_ref().is_some_and(|a| a.turn_seq >= turn_seq || a.record.verdict == CompletionVerdict::NoAgreement) {
            return Ok(false);
        }
        let assignment: Assignment = serde_json::from_str(&assignment)?;
        let mut record = previous.map(|a| a.record).unwrap_or_default();
        let corroboration = coordination::corroborate_checks(&assignment.acceptance_checks, measurements);
        record.assess_with_limits(serde_json::to_string(&json!({"turn_seq":turn_seq,"measurements":measurements,"corroboration":corroboration}))?,
            &corroboration, &coordination::limits(assignment.max_rework));
        let state = match record.verdict {
            CompletionVerdict::Accept => "completed",
            CompletionVerdict::Rework => "reviewing",
            CompletionVerdict::NoAgreement => "no_agreement",
        };
        if let Some(followup) = &record.followup {
            let message_id = format!("acceptance-{id}-{turn_seq}");
            let payload = json!({"id":message_id,"source":"coordinator","text":format!(
                "{followup}\nRework round {}/{}. Repair the failed checks in this same workspace and conversation. The coordinator will measure them again after your next final turn.", record.rework_rounds, assignment.max_rework)});
            insert_message(&tx, &message_id, "coordinator", id, &payload)?;
        }
        let assessment = CompletionAssessment { turn_seq, record, measurements: measurements.to_vec() };
        tx.execute("INSERT INTO delegated_completions(run_id,assessment) VALUES (?,?) ON CONFLICT(run_id) DO UPDATE SET assessment=excluded.assessment",
            params![id, serde_json::to_string(&assessment)?])?;
        tx.execute("UPDATE delegated_runs SET state=? WHERE id=?", params![state,id])?;
        tx.commit()?;
        Ok(true)
    }

    /// Apply a peer agreement to one durable bilateral HACP v2 contract.
    pub fn record_agreement(&self, source: &str, destination: &str, text: &str) -> anyhow::Result<String> {
        let from = self.get(source)?;
        let to = self.get(destination)?;
        if to.state == "no_agreement" {
            return Err(MessageRejected("Run reached terminal no_agreement; create a new assignment to continue".into()).into());
        }
        anyhow::ensure!(from.task_id == to.task_id, "Peer agreement crosses task boundary");
        let (party_a, party_b) = if source < destination { (source, destination) } else { (destination, source) };
        let db = self.0.lock().unwrap();
        let saved: Option<String> = db.query_row(
            "SELECT contract FROM delegated_contracts WHERE task_id=? AND party_a=? AND party_b=?",
            params![from.task_id, party_a, party_b], |row| row.get(0)).optional()?;
        let mut record = match saved.as_ref() {
            None => AgreementRecord::propose(&from.task_id, source, destination, text)?,
            Some(saved) => serde_json::from_str(saved)?,
        };
        let result = if saved.is_none() { Ok(record.proposed_digest.clone()) }
            else if record.contract.state == ContractState::Executing { record.propose_amendment(source, text) }
            else if record.contract.state == ContractState::Amending { record.agree_amendment(source, text) }
            else { record.agree(source, text) };
        db.execute("INSERT INTO delegated_contracts(task_id,party_a,party_b,contract) VALUES (?,?,?,?) ON CONFLICT(task_id,party_a,party_b) DO UPDATE SET contract=excluded.contract",
            params![from.task_id, party_a, party_b, serde_json::to_string(&record)?])?;
        result
    }
    pub fn get(&self, id: &str) -> anyhow::Result<Run> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,task_id,conversation_id,assignment,tmux_name,state,metadata,cursor,runner_path,(SELECT json_object('status',status,'summary',summary) FROM delegated_reviews WHERE delegated_reviews.task_id=delegated_runs.task_id) FROM delegated_runs WHERE id=?")?;
        let tuple = stmt.query_row([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut run = decode_run_row(&db, tuple)?;
        drop(stmt);
        drop(db);
        run.contracts = self.contracts_for(&run.id).unwrap_or_default();
        Ok(run)
    }
    pub fn state(&self, id: &str, state: &str, reason: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE delegated_runs SET state=?,metadata=json_set(metadata,'$.reason',?) WHERE id=? AND state NOT IN ('superseded','no_agreement')",
            params![state, reason, id],
        )?;
        Ok(())
    }
    pub fn claim(&self, id: &str, runner: &str) -> anyhow::Result<bool> {
        let now = chrono::Utc::now().timestamp();
        Ok(self.0.lock().unwrap().execute(
            "UPDATE delegated_runs SET state='launching',runner_path=?,metadata=json_set(metadata,'$.claimed_at',?) WHERE id=? AND state='queued'",
            params![runner, now, id],
        )? == 1)
    }
    pub fn sync(&self, id: &str, snapshot: &Value) -> anyhow::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cursor = 0;
        for event in snapshot["events"].as_array().into_iter().flatten() {
            let seq = event["seq"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("Invalid event sequence"))?;
            cursor = cursor.max(seq);
            tx.execute(
                "INSERT OR IGNORE INTO delegated_events VALUES (?,?,?,?,?)",
                params![
                    id,
                    event["id"].as_str(),
                    seq,
                    event["kind"].as_str(),
                    event["payload"].to_string()
                ],
            )?;
        }
        let mut metadata = snapshot["metadata"].clone();
        metadata["approvals"] = snapshot["approvals"].clone();
        // Coordinator-observed contact time, never trusted from a worker.
        metadata["last_seen"] = json!(chrono::Utc::now().to_rfc3339());
        let reported_state = metadata["state"]
            .as_str()
            .unwrap_or("disconnected");
        let completion = completion_for(&tx, id)?;
        let state = if completion.as_ref().is_some_and(|a| a.record.verdict == CompletionVerdict::NoAgreement) {
            "no_agreement"
        } else if reported_state == "completed" {
            match completion.as_ref().filter(|a| Some(a.turn_seq) == metadata["acceptance_turn"].as_i64()) {
                Some(a) if a.record.verdict == CompletionVerdict::Accept => "completed",
                Some(_) => "reviewing",
                None => "verifying",
            }
        } else {
            reported_state
        };
        tx.execute(
            "UPDATE delegated_runs SET metadata=?,state=?,cursor=max(cursor,?) WHERE id=? AND state!='superseded'",
            params![metadata.to_string(), state, cursor, id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn events(&self, id: &str, after: i64) -> anyhow::Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,seq,kind,payload FROM delegated_events WHERE run_id=? AND seq>? ORDER BY seq LIMIT 300")?;
        let rows = stmt.query_map(params![id, after], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|r| {let (id,seq,kind,payload)=r?; Ok(json!({"id":id,"seq":seq,"kind":kind,"payload":serde_json::from_str::<Value>(&payload)?}))}).collect()
    }
    /// Most recent evidence in chronological order, independent of pagination.
    pub fn recent_events(&self, id: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,seq,kind,payload FROM (SELECT id,seq,kind,payload FROM delegated_events WHERE run_id=? ORDER BY seq DESC LIMIT ?) ORDER BY seq")?;
        let rows = stmt.query_map(params![id, limit.min(1000) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|r| {
            let (id, seq, kind, payload) = r?;
            Ok(json!({"id":id,"seq":seq,"kind":kind,"payload":serde_json::from_str::<Value>(&payload)?}))
        }).collect()
    }
    pub fn decide(
        &self,
        run: &str,
        id: &str,
        fingerprint: &str,
        decision: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            ["continue", "stop"].contains(&decision),
            "Decision must be continue or stop"
        );
        let r = self.get(run)?;
        anyhow::ensure!(
            r.metadata["approvals"].as_array().is_some_and(|a| a
                .iter()
                .any(|p| p["id"] == id && p["fingerprint"] == fingerprint && p["consumed"] == 0)),
            "Exact pending action not found"
        );
        let db = self.0.lock().unwrap();
        db.execute("INSERT OR IGNORE INTO delegated_decisions(run_id,id,fingerprint,decision) VALUES (?,?,?,?)", params![run,id,fingerprint,decision])?;
        let existing: (String, String) = db.query_row(
            "SELECT fingerprint,decision FROM delegated_decisions WHERE run_id=? AND id=?",
            params![run, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        anyhow::ensure!(
            existing == (fingerprint.to_string(), decision.to_string()),
            "Action already has a different decision"
        );
        Ok(())
    }
    pub fn pending_decisions(&self, run: &str) -> anyhow::Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut stmt=db.prepare("SELECT id,fingerprint,decision FROM delegated_decisions WHERE run_id=? AND delivered=0")?;
        let rows=stmt.query_map([run],|r|Ok(json!({"id":r.get::<_,String>(0)?,"fingerprint":r.get::<_,String>(1)?,"decision":r.get::<_,String>(2)?})))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    pub fn decision_delivered(&self, run: &str, id: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE delegated_decisions SET delivered=1 WHERE run_id=? AND id=?",
            params![run, id],
        )?;
        Ok(())
    }
    pub fn message(
        &self,
        id: &str,
        source: &str,
        destination: &str,
        payload: &Value,
    ) -> anyhow::Result<()> {
        let to = self.get(destination)?;
        if to.state == "no_agreement" {
            return Err(MessageRejected("Run reached terminal no_agreement; create a new assignment to continue".into()).into());
        }
        if source != "user" && self.get(source)?.task_id != to.task_id {
            let reason = "Peer message crosses task boundary";
            let mut db = self.0.lock().unwrap();
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            reject_route(&tx, id, source, destination, &to.task_id, reason, chrono::Utc::now().timestamp())?;
            tx.commit()?;
            return Err(MessageRejected(reason.into()).into());
        }
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = insert_message(&tx, id, source, destination, payload);
        match result {
            Ok(()) => { tx.commit()?; Ok(()) }
            Err(error) => {
                tx.rollback()?;
                // Preserve evidence even when rejecting a changed delivered ID.
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM delegated_messages WHERE id=?)", [id], |r| r.get(0))?;
                if exists { relay::incident(&tx, id, "Message ID reused with a different envelope", chrono::Utc::now().timestamp())?; }
                tx.commit()?;
                if exists {
                    return Err(MessageRejected(error.to_string()).into());
                }
                Err(error)
            }
        }
    }
    pub fn has_pending_messages(&self, run: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM delegated_messages WHERE destination=? AND delivered=0 AND id NOT IN (SELECT message_id FROM delegated_relay_envelopes WHERE rejected=1))", [run], |row| row.get(0))?)
    }
    pub fn pending_messages(&self, run: &str) -> anyhow::Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut stmt=db.prepare("SELECT payload FROM delegated_messages WHERE destination=? AND delivered=0 AND id NOT IN (SELECT message_id FROM delegated_relay_envelopes WHERE rejected=1) ORDER BY rowid")?;
        let rows = stmt.query_map([run], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn next_delivery(&self, run: &str) -> anyhow::Result<Option<relay::Delivery>> {
        self.next_delivery_at(run, chrono::Utc::now().timestamp())
    }
    fn next_delivery_at(&self, run: &str, now: i64) -> anyhow::Result<Option<relay::Delivery>> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let delivery = relay::next(&tx, run, self.1, now)?;
        tx.commit()?;
        Ok(delivery)
    }
    pub fn message_delivered(&self, delivery: &relay::Delivery) -> anyhow::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        relay::delivered(&tx, delivery, chrono::Utc::now().timestamp())?;
        tx.commit()?;
        Ok(())
    }
    pub fn delivery_failed(&self, delivery: &relay::Delivery) -> anyhow::Result<()> {
        relay::release(&self.0.lock().unwrap(), delivery)
    }
    /// Incremental audit: hashes only rows appended since the last verified
    /// checkpoint and serves the cached validity otherwise.
    pub fn audit(&self, run: &str) -> anyhow::Result<Value> {
        self.audit_with(run, false)
    }
    /// Full audit from genesis; detects removed or modified old rows.
    pub fn audit_full(&self, run: &str) -> anyhow::Result<Value> {
        self.audit_with(run, true)
    }
    fn audit_with(&self, run: &str, full: bool) -> anyhow::Result<Value> {
        self.get(run)?;
        self.audit_report(run, full)
    }
    /// Re-verifies the whole chain and refreshes the checkpoint (startup).
    pub fn verify_audit_chain(&self) -> anyhow::Result<bool> {
        Ok(self.audit_report("", true)?["chain_valid"] == true)
    }
    fn audit_report(&self, run: &str, full: bool) -> anyhow::Result<Value> {
        let mut db = self.0.lock().unwrap();
        // Read the chain and its head from one SQLite snapshot even when a
        // second coordinator connection is appending audit records; IMMEDIATE
        // because the verified checkpoint is written in the same transaction.
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let report = relay::audit_report(&tx, run, full)?;
        tx.commit()?;
        Ok(report)
    }
}

pub fn quick_check(path: &std::path::Path) -> bool {
    RunStore::quick_check(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> DelegationPlan {
        serde_json::from_value(json!({"summary":"work","assignments":[{"key":"a","device":"air","agent":"claude","model":null,"workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap()
    }

    fn checked_run(store: &RunStore) -> (Run, CheckMeasurement) {
        let check = coordination::AcceptanceCheck::FileExists { path: "result.txt".into() };
        let mut plan = plan();
        plan.assignments[0].acceptance_checks = vec![check.clone()];
        let run = store.create("task", "chat", &plan).unwrap().remove(0);
        (run, CheckMeasurement { check, passed: true, detail: "file exists".into() })
    }

    fn completed_snapshot(turn: i64) -> Value {
        json!({"metadata":{"state":"completed","acceptance_turn":turn,"native_conversation_id":"same-conversation"},"events":[],"approvals":[]})
    }

    #[test]
    fn completion_requires_measurements_and_each_new_turn_is_checked() {
        let graph = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let (run, measurement) = checked_run(&store);
        store.sync(&run.id, &completed_snapshot(1)).unwrap();
        assert_eq!(store.get(&run.id).unwrap().state, "verifying");
        assert!(!store.assess_completion(&run.id, 0, &[measurement.clone()]).unwrap());
        assert!(store.assess_completion(&run.id, 1, &[measurement.clone()]).unwrap());
        assert_eq!(store.get(&run.id).unwrap().state, "completed");
        store.sync(&run.id, &completed_snapshot(1)).unwrap();
        assert_eq!(store.get(&run.id).unwrap().state, "completed");
        assert!(!store.assess_completion(&run.id, 1, &[]).unwrap());
        store.sync(&run.id, &completed_snapshot(2)).unwrap();
        assert_eq!(store.get(&run.id).unwrap().state, "verifying");
        store.assess_completion(&run.id, 2, &[measurement]).unwrap();
        let accepted = store.get(&run.id).unwrap().completion.unwrap();
        assert_eq!(accepted.record.verdict, CompletionVerdict::Accept);
        assert_eq!(accepted.record.evidence.len(), 2);
        assert!(store.pending_messages(&run.id).unwrap().is_empty());
    }

    #[test]
    fn failed_measurements_are_durable_bounded_and_never_replayed() {
        let graph = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let (run, mut measurement) = checked_run(&store);
        measurement.passed = false;
        measurement.detail = "result.txt missing".into();
        for turn in 1..=3 {
            store.sync(&run.id, &completed_snapshot(turn)).unwrap();
            assert!(store.assess_completion(&run.id, turn, &[measurement.clone()]).unwrap());
            // Re-opening the store and replaying the same snapshot cannot spend
            // another round or duplicate its durable feedback message.
            let reopened = RunStore::new(graph.shared_conn()).unwrap();
            reopened.sync(&run.id, &completed_snapshot(turn)).unwrap();
            assert!(!reopened.assess_completion(&run.id, turn, &[measurement.clone()]).unwrap());
            assert_eq!(reopened.pending_messages(&run.id).unwrap().len(), usize::from(turn < 3));
            if let Some(delivery) = reopened.next_delivery(&run.id).unwrap() {
                reopened.message_delivered(&delivery).unwrap();
            }
            assert_eq!(reopened.get(&run.id).unwrap().metadata["native_conversation_id"], "same-conversation");
        }
        let result = store.get(&run.id).unwrap();
        assert_eq!(result.state, "no_agreement");
        let result = result.completion.unwrap();
        assert_eq!(result.record.rework_rounds, 2);
        assert_eq!(result.record.evidence.len(), 3);
        assert!(result.record.evidence.iter().all(|s| s.contains("result.txt missing")));
        let messages: i64 = graph.shared_conn().lock().unwrap().query_row(
            "SELECT count(*) FROM delegated_messages WHERE destination=?", [&run.id], |r| r.get(0)).unwrap();
        assert_eq!(messages, 2);
        assert!(store.message("late", "user", &run.id, &json!({"id":"late","text":"revive"})).is_err());
        store.state(&run.id, "disconnected", "late transport error").unwrap();
        assert_eq!(store.get(&run.id).unwrap().state, "no_agreement");
        for state in ["completed", "working", "paused-quota"] {
            let mut snapshot = completed_snapshot(4);
            snapshot["metadata"]["state"] = json!(state);
            store.sync(&run.id, &snapshot).unwrap();
            assert_eq!(store.get(&run.id).unwrap().state, "no_agreement");
            assert!(!store.assess_completion(&run.id, 4, &[]).unwrap());
        }
    }

    #[test]
    fn quota_and_peer_waits_preserve_acceptance_budget() {
        let graph = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let (run, _) = checked_run(&store);
        for state in ["paused-quota", "waiting-for-peer", "awaiting-approval", "working"] {
            let mut snapshot = completed_snapshot(1);
            snapshot["metadata"]["state"] = json!(state);
            store.sync(&run.id, &snapshot).unwrap();
            assert!(!store.assess_completion(&run.id, 1, &[]).unwrap());
            assert_eq!(store.get(&run.id).unwrap().state, state);
            assert!(store.get(&run.id).unwrap().completion.is_none());
        }
        store.sync(&run.id, &completed_snapshot(1)).unwrap();
        store.assess_completion(&run.id, 1, &[]).unwrap();
        assert_eq!(store.get(&run.id).unwrap().completion.unwrap().record.rework_rounds, 1);
    }

    #[test]
    fn legacy_assignments_without_checks_cannot_accept_claims() {
        let graph = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let mut plan = plan();
        plan.assignments[0].max_rework = 0;
        let run = store.create("task", "chat", &plan).unwrap().remove(0);
        store.sync(&run.id, &completed_snapshot(0)).unwrap();
        store.assess_completion(&run.id, 0, &[]).unwrap();
        assert_eq!(store.get(&run.id).unwrap().state, "no_agreement");
        assert!(store.pending_messages(&run.id).unwrap().is_empty());
    }

    #[test]
    fn pending_followups_prevent_accepting_an_older_turn() {
        let graph = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let (run, measurement) = checked_run(&store);
        store.sync(&run.id, &completed_snapshot(1)).unwrap();
        store.message("new-request", "user", &run.id, &json!({"id":"new-request","text":"more work"})).unwrap();
        assert!(!store.assess_completion(&run.id, 1, &[measurement]).unwrap());
        assert!(store.get(&run.id).unwrap().completion.is_none());
    }

    #[test]
    fn both_participants_serialize_the_same_frozen_digest_during_amendment() {
        let graph = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let mut plan = plan();
        let mut peer = plan.assignments[0].clone();
        peer.key = "b".into();
        plan.assignments.push(peer);
        let runs = store.create("task", "chat", &plan).unwrap();
        let terms = store.record_agreement(&runs[0].id, &runs[1].id, "API v1").unwrap();
        let frozen = store.record_agreement(&runs[1].id, &runs[0].id, &terms).unwrap();
        assert_ne!(terms, frozen, "proposal and frozen revision have different canonical identities");
        let pending = store.record_agreement(&runs[0].id, &runs[1].id, "API v2").unwrap();
        for run in &runs {
            let wire = serde_json::to_value(store.get(&run.id).unwrap()).unwrap();
            let contract = &wire["contracts"][0];
            assert_eq!(contract["contract"]["revisions"][0]["digest"], frozen);
            assert_eq!(contract["contract"]["revisions"][0]["content"]["agreement"], "API v1");
            assert_eq!(contract["proposed_digest"], pending);
            assert_eq!(contract["contract"]["state"], "amending");
            assert_eq!(contract["contract"]["limits"]["max_rework"], 2);
        }
    }
    fn cleanup_test_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn durable_identity_claim_and_event_replay() {
        let path = std::env::temp_dir().join(format!("hive-runs-{}.db", uuid::Uuid::new_v4()));
        let graph = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(graph.shared_conn()).unwrap();
        let run = s.create("task", "chat", &plan()).unwrap().remove(0);
        assert!(s.claim(&run.id, "/runner").unwrap());
        assert!(!s.claim(&run.id, "/runner").unwrap());
        let snapshot = json!({"metadata":{"state":"working","native_conversation_id":"native"},"events":[{"id":"event","seq":1,"kind":"output","payload":{"text":"hello"}}],"approvals":[]});
        s.sync(&run.id, &snapshot).unwrap();
        s.sync(&run.id, &snapshot).unwrap();
        assert_eq!(s.events(&run.id, 0).unwrap().len(), 1);
        drop(s);
        drop(graph);
        let graph = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(graph.shared_conn()).unwrap();
        let restored = s.get(&run.id).unwrap();
        assert_eq!(restored.metadata["native_conversation_id"], "native");
        assert!(!s.claim(&run.id, "/runner").unwrap());
        assert!(s.create("task", "chat", &plan()).is_err());
        drop(s);
        drop(graph);
        cleanup_test_db(&path);
    }
    #[test]
    fn a_runner_stall_reason_is_recorded_on_the_working_run_and_cleared_with_it() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let r = s.create("task", "chat", &plan()).unwrap().remove(0);
        let stall = json!({"tool":"bash","command":"cargo build","silent_since":1_790_000_000,"silent_minutes":10,"reason":"Stalled: cargo build silent for 10 min"});
        let events = json!([{"id":"stalled","seq":1,"kind":"stalled","payload":stall}]);
        s.sync(&r.id, &json!({"metadata":{"state":"working","stall":stall},"events":events,"approvals":[]})).unwrap();
        let run = s.get(&r.id).unwrap();
        assert_eq!(run.state, "working");
        assert_eq!(run.metadata["stall"]["reason"], "Stalled: cargo build silent for 10 min");
        assert_eq!(s.events(&r.id, 0).unwrap().len(), 1);
        s.sync(&r.id, &json!({"metadata":{"state":"working","stall":null},"events":[],"approvals":[]})).unwrap();
        assert!(s.get(&r.id).unwrap().metadata["stall"].is_null());
    }
    #[test]
    fn decisions_are_exact_durable_and_nonreplaceable() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let r = s.create("task", "chat", &plan()).unwrap().remove(0);
        s.sync(&r.id,&json!({"metadata":{"state":"awaiting-approval"},"events":[],"approvals":[{"id":"approval","fingerprint":"digest","consumed":0}]})).unwrap();
        assert!(s.decide(&r.id, "approval", "changed", "continue").is_err());
        s.decide(&r.id, "approval", "digest", "continue").unwrap();
        s.decide(&r.id, "approval", "digest", "continue").unwrap();
        assert!(s.decide(&r.id, "approval", "digest", "stop").is_err());
        assert_eq!(s.pending_decisions(&r.id).unwrap().len(), 1);
        s.decision_delivered(&r.id, "approval").unwrap();
        assert!(s.pending_decisions(&r.id).unwrap().is_empty());
    }
    #[test]
    fn peers_are_task_scoped() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let a = s.create("a", "chat", &plan()).unwrap().remove(0);
        let b = s.create("b", "chat", &plan()).unwrap().remove(0);
        assert!(s
            .message("m", &a.id, &b.id, &json!({"id":"m","text":"bad"}))
            .is_err());
    }

    #[test]
    fn repeated_cross_task_peer_sync_records_one_incident_and_one_reject() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let a = s.create("a", "chat", &plan()).unwrap().remove(0);
        let b = s.create("b", "chat", &plan()).unwrap().remove(0);
        let payload = json!({"id":"cross","source":a.id,"text":"bad"});
        for _ in 0..3 {
            let error = s.message("cross", &a.id, &b.id, &payload).unwrap_err();
            assert!(error.is::<MessageRejected>());
        }
        let count = |sql: &str| -> i64 { s.0.lock().unwrap().query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM delegated_messages WHERE id='cross'"), 0);
        assert_eq!(count("SELECT count(*) FROM delegated_relay_incidents WHERE message_id='cross'"), 1);
        assert_eq!(count("SELECT count(*) FROM delegated_relay_audit WHERE json_extract(record,'$.event')='reject' AND json_extract(record,'$.message_id')='cross'"), 1);
        assert_eq!(s.audit_full(&a.id).unwrap()["chain_valid"], true);
        assert!(s.pending_messages(&b.id).unwrap().is_empty());
    }

    #[test]
    fn message_replay_rejects_changed_envelopes_and_preserves_acknowledgment() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let mut p = plan();
        let mut second = p.assignments[0].clone();
        second.key = "b".into();
        p.assignments.push(second);
        let runs = s.create("task", "chat", &p).unwrap();
        let (a, b) = (&runs[0].id, &runs[1].id);
        let payload = json!({"id":"message","text":"protocol v1"});
        s.message("message", a, b, &payload).unwrap();
        s.message("message", a, b, &payload).unwrap();
        assert_eq!(s.pending_messages(b).unwrap(), vec![payload.clone()]);
        assert!(s.message("message", "user", b, &payload).is_err());
        assert!(s.message("message", a, a, &payload).is_err());
        assert!(s
            .message(
                "message",
                a,
                b,
                &json!({"id":"message","text":"protocol v2"})
            )
            .is_err());
        assert!(s.message("different-id", a, b, &payload).is_err());
        let delivery = s.next_delivery(b).unwrap().unwrap();
        s.message_delivered(&delivery).unwrap();
        let reopened = RunStore::new(g.shared_conn()).unwrap();
        reopened.message("message", a, b, &payload).unwrap();
        assert!(reopened.pending_messages(b).unwrap().is_empty());
    }

    #[test]
    fn bilateral_agreements_are_frozen_stored_and_visible_to_both_runs() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let mut p = plan();
        let mut second = p.assignments[0].clone();
        second.key = "b".into();
        p.assignments.push(second);
        let runs = s.create("task", "chat", &p).unwrap();
        let digest = s.record_agreement(&runs[0].id, &runs[1].id, "API v1").unwrap();
        s.record_agreement(&runs[1].id, &runs[0].id, &digest).unwrap();
        for run in s.list().unwrap().0 {
            assert_eq!(run.contracts.len(), 1);
            assert_eq!(run.contracts[0].contract.state, ContractState::Executing);
        }
        let amendment = s.record_agreement(&runs[0].id, &runs[1].id, "API v2").unwrap();
        assert!(s.record_agreement(&runs[1].id, &runs[0].id, "one-sided-v3").is_err());
        assert_eq!(s.contracts_for(&runs[0].id).unwrap()[0].rejected_changes.len(), 1);
        s.record_agreement(&runs[1].id, &runs[0].id, &amendment).unwrap();
        assert_eq!(s.contracts_for(&runs[1].id).unwrap()[0].contract.revisions.len(), 2);
    }

    #[test]
    fn reviews_recover_leases_and_commit_messages_atomically() {
        let path = std::env::temp_dir().join(format!("hive-review-{}.db", uuid::Uuid::new_v4()));
        let g = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let run = s.create("task", "chat", &plan()).unwrap().remove(0);
        let other = s.create("other", "chat", &plan()).unwrap().remove(0);
        let token1 = s.claim_review("task", "cursor-1").unwrap().unwrap();
        assert!(s.claim_review("task", "cursor-1").unwrap().is_none());
        assert!(s.claim_review("task", "cursor-2").unwrap().is_none());
        // Simulate coordinator recovery after the existing review lease expires.
        g.shared_conn()
            .lock()
            .unwrap()
            .execute(
                "UPDATE delegated_reviews SET lock_until=0 WHERE task_id='task'",
                [],
            )
            .unwrap();
        let token2 = s.claim_review("task", "cursor-2").unwrap().unwrap();
        assert!(s
            .finish_review("task", "cursor-1", &token1, "complete", "old result", &[])
            .is_err());
        let message = json!({"id":"review-message","text":"verify hashes"});
        s.finish_review(
            "task",
            "cursor-2",
            &token2,
            "continue",
            "verification required",
            &[(run.id.clone(), message.clone())],
        )
        .unwrap();
        drop(s);
        drop(g);
        let g = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        assert_eq!(
            s.get(&run.id).unwrap().review["summary"],
            "verification required"
        );
        assert_eq!(s.pending_messages(&run.id).unwrap(), vec![message.clone()]);
        assert!(s.claim_review("task", "cursor-2").unwrap().is_none());
        let token3 = s.claim_review("task", "cursor-3").unwrap().unwrap();
        let valid = (run.id.clone(), json!({"id":"valid","text":"one"}));
        let changed = (
            run.id.clone(),
            json!({"id":"review-message","text":"changed"}),
        );
        assert!(s
            .finish_review(
                "task",
                "cursor-3",
                &token3,
                "continue",
                "must roll back",
                &[valid.clone(), changed]
            )
            .is_err());
        assert_eq!(s.pending_messages(&run.id).unwrap(), vec![message.clone()]);
        assert_eq!(s.get(&run.id).unwrap().review["status"], "reviewing");
        assert!(s
            .finish_review(
                "task",
                "cursor-3",
                &token3,
                "continue",
                "wrong task",
                &[valid, (other.id, json!({"id":"cross-task","text":"bad"}))]
            )
            .is_err());
        assert_eq!(s.pending_messages(&run.id).unwrap(), vec![message]);
        s.finish_review("task", "cursor-3", &token3, "complete", "verified", &[])
            .unwrap();
        drop(s);
        drop(g);
        cleanup_test_db(&path);
    }

    #[test]
    fn two_claims_of_same_cursor_with_first_expiring_increments_continue_reviews_once() {
        let path = std::env::temp_dir().join(format!("hive-review-claim-{}.db", uuid::Uuid::new_v4()));
        let g = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let _run = s.create("task", "chat", &plan()).unwrap().remove(0);

        // First claim of cursor-1
        let token1 = s.claim_review("task", "cursor-1").unwrap().expect("first claim must succeed");
        assert_eq!(s.continue_reviews("task").unwrap(), 0);

        // Simulate lease expiration of the first claim
        g.shared_conn()
            .lock()
            .unwrap()
            .execute("UPDATE delegated_reviews SET lock_until=0 WHERE task_id='task'", [])
            .unwrap();

        // Second claim of the same cursor-1 succeeds with a new token
        let token2 = s.claim_review("task", "cursor-1").unwrap().expect("second claim must succeed");
        assert_ne!(token1, token2);

        // Both finish:
        // First claim finishes, but its token was superseded so it must be rejected
        let res1 = s.finish_review("task", "cursor-1", &token1, "continue", "expired review result", &[]);
        assert!(res1.is_err(), "superseded claim token must be rejected");

        // Second claim finishes as the current token holder
        let res2 = s.finish_review("task", "cursor-1", &token2, "continue", "active review result", &[]);
        assert!(res2.is_ok(), "current claim token holder must succeed");

        // continue_reviews increased by exactly 1
        assert_eq!(s.continue_reviews("task").unwrap(), 1);

        drop(s);
        drop(g);
        cleanup_test_db(&path);
    }

    #[test]
    fn replacement_survives_restart_and_old_snapshots_cannot_restore_it() {
        let path =
            std::env::temp_dir().join(format!("hive-replacement-{}.db", uuid::Uuid::new_v4()));
        let g = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let old = s.create("task", "chat", &plan()).unwrap().remove(0);
        assert!(s.claim(&old.id, "/runner").unwrap());
        let mut assignment = old.assignment.clone();
        assignment.device = "cis-a6000".into();
        let replacement = s.replace(&old.id, &assignment).unwrap();
        drop(s);
        drop(g);
        let g = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        assert_eq!(s.replace(&old.id, &assignment).unwrap().id, replacement.id);
        assert_eq!(s.list().unwrap().0.len(), 2);
        let restored = s.get(&replacement.id).unwrap();
        assert_eq!(restored.task_id, old.task_id);
        assert_eq!(restored.conversation_id, old.conversation_id);
        assert_ne!(restored.tmux_name, old.tmux_name);
        s.sync(
            &old.id,
            &json!({"metadata":{"state":"working"},"events":[],"approvals":[]}),
        )
        .unwrap();
        s.state(&old.id, "disconnected", "old probe").unwrap();
        assert_eq!(s.get(&old.id).unwrap().state, "superseded");
        assert_eq!(
            s.get(&old.id).unwrap().metadata["replacement_id"],
            replacement.id
        );
        assert!(!s.claim(&old.id, "/runner").unwrap());
        assert!(s.claim(&replacement.id, "/runner").unwrap());
        assert!(!s.claim(&replacement.id, "/runner").unwrap());
        assignment.device = "elsewhere".into();
        assert!(s.replace(&old.id, &assignment).is_err());
        drop(s);
        drop(g);
        cleanup_test_db(&path);
    }

    #[test]
    fn recent_events_retain_latest_evidence_beyond_first_page() {
        let g = crate::memory::graph::KnowledgeGraph::in_memory().unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let run = s.create("task", "chat", &plan()).unwrap().remove(0);
        let events: Vec<Value> = (1..=1200).map(|seq| json!({"id":format!("event-{seq}"),"seq":seq,"kind":"native","payload":{"seq":seq}})).collect();
        s.sync(
            &run.id,
            &json!({"metadata":{"state":"working"},"events":events,"approvals":[]}),
        )
        .unwrap();
        let first = s.events(&run.id, 0).unwrap();
        assert_eq!(first.len(), 300);
        assert_eq!(first.last().unwrap()["seq"], 300);
        let recent = s.recent_events(&run.id, 20).unwrap();
        assert_eq!(recent.len(), 20);
        assert_eq!(recent[0]["seq"], 1181);
        assert_eq!(recent.last().unwrap()["seq"], 1200);
        assert_eq!(s.recent_events(&run.id, usize::MAX).unwrap().len(), 1000);
        assert!(s.recent_events(&run.id, 0).unwrap().is_empty());
    }

    #[test]
    fn create_returns_exactly_the_inserted_ids() {
        let path = std::env::temp_dir().join(format!("hive-create-ids-{}.db", uuid::Uuid::new_v4()));
        let graph = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let store = RunStore::new(graph.shared_conn()).unwrap();
        let mut p = plan();
        let mut b = p.assignments[0].clone();
        b.key = "b".into();
        b.workspace = "~/hive-workspaces/test-b".into();
        p.assignments.push(b);
        let runs = store.create("task-test-ids", "chat", &p).unwrap();
        assert_eq!(runs.len(), 2);
        let id0 = &runs[0].id;
        let id1 = &runs[1].id;
        assert_eq!(store.get(id0).unwrap().id, *id0);
        assert_eq!(store.get(id1).unwrap().id, *id1);
        drop(store);
        drop(graph);
        cleanup_test_db(&path);
    }

    #[test]
    fn corrupt_page_fixture_db_makes_list_return_readable_rows_plus_error_count() {
        use std::io::{Seek, SeekFrom, Write};
        let path = std::env::temp_dir().join(format!("hive-corrupt-page-{}.db", uuid::Uuid::new_v4()));
        {
            let graph = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
            let store = RunStore::new(graph.shared_conn()).unwrap();
            for i in 0..50 {
                let mut p = plan();
                p.assignments[0].objective = format!("objective {i} {}", "x".repeat(600));
                store.create(&format!("task-{i}"), "chat", &p).unwrap();
            }
            let leaf_page: i64 = {
                let conn = graph.shared_conn();
                let p: i64 = conn.lock().unwrap().query_row(
                    "SELECT pageno FROM dbstat WHERE name = 'delegated_runs' AND pagetype = 'leaf' LIMIT 1",
                    [],
                    |r| r.get(0),
                ).unwrap_or(20);
                let _ = conn.lock().unwrap().execute_batch("PRAGMA journal_mode = DELETE;");
                p
            };
            drop(store);
            drop(graph);

            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let offset = ((leaf_page - 1) * 4096) as u64;
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&vec![0xff; 4096]).unwrap();
            file.flush().unwrap();
            drop(file);
        }

        assert!(!RunStore::quick_check(&path));

        let conn = std::sync::Arc::new(std::sync::Mutex::new(rusqlite::Connection::open(&path).unwrap()));
        let store = RunStore::new(conn).unwrap();
        let (runs, errors) = store.list().expect("list must not fail completely on corrupt pages");
        assert!(errors > 0, "errors must be greater than 0");
        assert!(!runs.is_empty(), "must return readable rows");
        drop(store);
        cleanup_test_db(&path);
    }

    #[test]
    fn quick_check_reports_false_for_corrupt_fixture_db() {
        use std::io::{Seek, SeekFrom, Write};
        let path = std::env::temp_dir().join(format!("hive-qc-fixture-{}.db", uuid::Uuid::new_v4()));
        {
            let graph = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
            let store = RunStore::new(graph.shared_conn()).unwrap();
            store.create("task-qc", "chat", &plan()).unwrap();
            let _ = graph.shared_conn().lock().unwrap().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        assert!(RunStore::quick_check(&path));

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let len = file.metadata().unwrap().len();
        if len > 8192 {
            file.seek(SeekFrom::Start(4096)).unwrap();
            file.write_all(&vec![0xff; 4096]).unwrap();
            file.flush().unwrap();
        } else {
            file.seek(SeekFrom::End(-100)).unwrap();
            file.write_all(&vec![0xff; 100]).unwrap();
            file.flush().unwrap();
        }
        drop(file);

        assert!(!RunStore::quick_check(&path));
        cleanup_test_db(&path);
    }
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod relay_tests;
