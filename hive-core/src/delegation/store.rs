use super::{coordination::AgreementRecord, relay, Assignment, DelegationPlan};
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

impl RunStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> anyhow::Result<Self> {
        conn.lock().unwrap().execute_batch("CREATE TABLE IF NOT EXISTS delegated_runs (
          id TEXT PRIMARY KEY, task_id TEXT NOT NULL, conversation_id TEXT NOT NULL,
          assignment TEXT NOT NULL, tmux_name TEXT NOT NULL, state TEXT NOT NULL,
          metadata TEXT NOT NULL DEFAULT '{}', cursor INTEGER NOT NULL DEFAULT 0, runner_path TEXT,
          UNIQUE(task_id, assignment));
          CREATE TABLE IF NOT EXISTS delegated_events (run_id TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(run_id,id));
          CREATE TABLE IF NOT EXISTS delegated_decisions (run_id TEXT NOT NULL, id TEXT NOT NULL, fingerprint TEXT NOT NULL, decision TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(run_id,id));
          CREATE TABLE IF NOT EXISTS delegated_messages (id TEXT PRIMARY KEY, source TEXT NOT NULL, destination TEXT NOT NULL, payload TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS delegated_contracts (task_id TEXT NOT NULL, party_a TEXT NOT NULL, party_b TEXT NOT NULL, contract TEXT NOT NULL, PRIMARY KEY(task_id,party_a,party_b));")?;
        {
            let db = conn.lock().unwrap();
            db.execute_batch("CREATE TABLE IF NOT EXISTS delegated_reviews(task_id TEXT PRIMARY KEY,cursor TEXT,status TEXT NOT NULL DEFAULT 'reviewing',summary TEXT NOT NULL DEFAULT '',lock_until INTEGER NOT NULL DEFAULT 0,continue_reviews INTEGER NOT NULL DEFAULT 0,claim_token TEXT NOT NULL DEFAULT '');")?;
            // A database created before review rounds were bounded has no counter yet.
            if db.prepare("SELECT continue_reviews FROM delegated_reviews").is_err() {
                db.execute_batch("ALTER TABLE delegated_reviews ADD COLUMN continue_reviews INTEGER NOT NULL DEFAULT 0;")?;
            }
            if db.prepare("SELECT claim_token FROM delegated_reviews").is_err() {
                db.execute_batch("ALTER TABLE delegated_reviews ADD COLUMN claim_token TEXT NOT NULL DEFAULT '';")?;
            }
        }
        {
            let mut db = conn.lock().unwrap();
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            relay::schema(&tx)?;
            // Existing runs gain identities; unsigned historical messages are
            // never retroactively signed and will fail closed before delivery.
            let ids = {
                let mut stmt = tx.prepare("SELECT id FROM delegated_runs WHERE id NOT IN (SELECT run_id FROM delegated_relay_keys)")?;
                let rows = stmt.query_map([], |r| r.get::<_,String>(0))?;
                rows.collect::<Result<Vec<_>,_>>()?
            };
            for id in ids { relay::create_identity(&tx, &id)?; }
            tx.commit()?;
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
        for assignment in &plan.assignments {
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO delegated_runs(id,task_id,conversation_id,assignment,tmux_name,state) VALUES (?,?,?,?,?,?)", params![id,task,conversation,serde_json::to_string(assignment)?,format!("hive-agent-{id}"),"queued"])?;
            relay::create_identity(&tx, &id)?;
        }
        tx.commit()?;
        drop(db);
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.task_id == task)
            .collect())
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
        let changed=self.0.lock().unwrap().execute("UPDATE delegated_runs SET state='queued' WHERE id=? AND runner_path IS NULL AND state IN ('needs-setup','disconnected')",[id])?;
        anyhow::ensure!(
            changed == 1,
            "Only a run that never launched can retry setup"
        );
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
    pub fn list(&self) -> anyhow::Result<Vec<Run>> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,task_id,conversation_id,assignment,tmux_name,state,metadata,cursor,runner_path,(SELECT json_object('status',status,'summary',summary) FROM delegated_reviews WHERE delegated_reviews.task_id=delegated_runs.task_id) FROM delegated_runs ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| {
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
        let mut runs: Vec<Run> = rows.map(|row| {
            let (id, task_id, conversation_id, a, tmux_name, state, m, cursor, runner_path, review) = row?;
            Ok(Run {
                identity: relay::identity(&db, &id)?,
                relay: relay::status(&db, &id)?,
                id,
                task_id,
                conversation_id,
                assignment: serde_json::from_str(&a)?,
                tmux_name,
                state,
                metadata: serde_json::from_str(&m)?,
                cursor,
                runner_path,
                review: review
                    .map(|s| serde_json::from_str(&s))
                    .transpose()?
                    .unwrap_or(Value::Null),
                contracts: vec![],
            })
        })
        .collect::<anyhow::Result<_>>()?;
        drop(stmt);
        drop(db);
        for run in &mut runs { run.contracts = self.contracts_for(&run.id)?; }
        Ok(runs)
    }

    pub fn contracts_for(&self, run: &str) -> anyhow::Result<Vec<AgreementRecord>> {
        let db = self.0.lock().unwrap();
        let mut stmt = db.prepare("SELECT contract FROM delegated_contracts WHERE party_a=? OR party_b=? ORDER BY rowid")?;
        let rows = stmt.query_map([run, run], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Apply a peer agreement to one durable bilateral HACP v2 contract.
    pub fn record_agreement(&self, source: &str, destination: &str, text: &str) -> anyhow::Result<String> {
        let from = self.get(source)?;
        let to = self.get(destination)?;
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
        self.list()?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("Run not found"))
    }
    pub fn state(&self, id: &str, state: &str, reason: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE delegated_runs SET state=?,metadata=json_set(metadata,'$.reason',?) WHERE id=? AND state!='superseded'",
            params![state, reason, id],
        )?;
        Ok(())
    }
    pub fn claim(&self, id: &str, runner: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().unwrap().execute("UPDATE delegated_runs SET state='launching',runner_path=? WHERE id=? AND state='queued'",params![runner,id])? == 1)
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
        let state = metadata["state"]
            .as_str()
            .unwrap_or("disconnected")
            .to_string();
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

#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> DelegationPlan {
        serde_json::from_value(json!({"summary":"work","assignments":[{"key":"a","device":"air","agent":"claude","model":null,"workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap()
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
        std::fs::remove_file(path).unwrap();
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
        for run in s.list().unwrap() {
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
        std::fs::remove_file(path).unwrap();
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
        let _ = std::fs::remove_file(path);
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
        assert_eq!(s.list().unwrap().len(), 2);
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
        std::fs::remove_file(path).unwrap();
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
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod relay_tests;
