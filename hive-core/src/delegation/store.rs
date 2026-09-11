use super::{Assignment, DelegationPlan};
use rusqlite::{params, Connection};
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
}

#[derive(Clone)]
pub struct RunStore(Arc<Mutex<Connection>>);

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
    db.execute(
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
    Ok(())
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
          CREATE TABLE IF NOT EXISTS delegated_messages (id TEXT PRIMARY KEY, source TEXT NOT NULL, destination TEXT NOT NULL, payload TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);")?;
        conn.lock().unwrap().execute_batch("CREATE TABLE IF NOT EXISTS delegated_reviews(task_id TEXT PRIMARY KEY,cursor TEXT,status TEXT NOT NULL DEFAULT 'reviewing',summary TEXT NOT NULL DEFAULT '',lock_until INTEGER NOT NULL DEFAULT 0);")?;
        Ok(Self(conn))
    }
    pub fn create(
        &self,
        task: &str,
        conversation: &str,
        plan: &DelegationPlan,
    ) -> anyhow::Result<Vec<Run>> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
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
        }
        tx.commit()?;
        drop(db);
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.task_id == task)
            .collect())
    }
    pub fn claim_review(&self, task: &str, cursor: &str) -> anyhow::Result<bool> {
        let db = self.0.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        Ok(db.execute("INSERT INTO delegated_reviews(task_id,cursor,lock_until) VALUES (?,?,?) ON CONFLICT(task_id) DO UPDATE SET cursor=excluded.cursor,status='reviewing',lock_until=excluded.lock_until WHERE delegated_reviews.lock_until<? AND (delegated_reviews.cursor!=excluded.cursor OR delegated_reviews.status='reviewing')",params![task,cursor,now+240,now])?==1)
    }
    pub fn finish_review(
        &self,
        task: &str,
        cursor: &str,
        status: &str,
        summary: &str,
        messages: &[(String, Value)],
    ) -> anyhow::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        anyhow::ensure!(tx.execute("UPDATE delegated_reviews SET status=?,summary=?,lock_until=0 WHERE task_id=? AND cursor=?",params![status,summary,task,cursor])?==1,"Review was superseded");
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
        let tx = db.transaction()?;
        anyhow::ensure!(tx.execute("UPDATE delegated_runs SET state='superseded',metadata=json_set(metadata,'$.replacement_id',?) WHERE id=? AND state!='superseded'",params![replacement,id])?==1,"Assignment already replaced");
        tx.execute("INSERT INTO delegated_runs(id,task_id,conversation_id,assignment,tmux_name,state) VALUES (?,?,?,?,?,'queued')",params![replacement,old.task_id,old.conversation_id,serde_json::to_string(assignment)?,format!("hive-agent-{replacement}")])?;
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
        rows.map(|row| {
            let (id, task_id, conversation_id, a, tmux_name, state, m, cursor, runner_path, review) = row?;
            Ok(Run {
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
            })
        })
        .collect()
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
        let tx = db.transaction()?;
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
        if source != "user" {
            anyhow::ensure!(
                self.get(source)?.task_id == to.task_id,
                "Peer message crosses task boundary"
            );
        }
        insert_message(&self.0.lock().unwrap(), id, source, destination, payload)
    }
    pub fn pending_messages(&self, run: &str) -> anyhow::Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut stmt=db.prepare("SELECT payload FROM delegated_messages WHERE destination=? AND delivered=0 ORDER BY rowid")?;
        let rows = stmt.query_map([run], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn message_delivered(&self, id: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("UPDATE delegated_messages SET delivered=1 WHERE id=?", [id])?;
        Ok(())
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
        s.message_delivered("message").unwrap();
        let reopened = RunStore::new(g.shared_conn()).unwrap();
        reopened.message("message", a, b, &payload).unwrap();
        assert!(reopened.pending_messages(b).unwrap().is_empty());
    }

    #[test]
    fn reviews_recover_leases_and_commit_messages_atomically() {
        let path = std::env::temp_dir().join(format!("hive-review-{}.db", uuid::Uuid::new_v4()));
        let g = crate::memory::graph::KnowledgeGraph::open(&path).unwrap();
        let s = RunStore::new(g.shared_conn()).unwrap();
        let run = s.create("task", "chat", &plan()).unwrap().remove(0);
        let other = s.create("other", "chat", &plan()).unwrap().remove(0);
        assert!(s.claim_review("task", "cursor-1").unwrap());
        assert!(!s.claim_review("task", "cursor-1").unwrap());
        assert!(!s.claim_review("task", "cursor-2").unwrap());
        // Simulate coordinator recovery after the existing review lease expires.
        g.shared_conn()
            .lock()
            .unwrap()
            .execute(
                "UPDATE delegated_reviews SET lock_until=0 WHERE task_id='task'",
                [],
            )
            .unwrap();
        assert!(s.claim_review("task", "cursor-2").unwrap());
        assert!(s
            .finish_review("task", "cursor-1", "complete", "old result", &[])
            .is_err());
        let message = json!({"id":"review-message","text":"verify hashes"});
        s.finish_review(
            "task",
            "cursor-2",
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
        assert!(!s.claim_review("task", "cursor-2").unwrap());
        assert!(s.claim_review("task", "cursor-3").unwrap());
        let valid = (run.id.clone(), json!({"id":"valid","text":"one"}));
        let changed = (
            run.id.clone(),
            json!({"id":"review-message","text":"changed"}),
        );
        assert!(s
            .finish_review(
                "task",
                "cursor-3",
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
                "continue",
                "wrong task",
                &[valid, (other.id, json!({"id":"cross-task","text":"bad"}))]
            )
            .is_err());
        assert_eq!(s.pending_messages(&run.id).unwrap(), vec![message]);
        s.finish_review("task", "cursor-3", "complete", "verified", &[])
            .unwrap();
        drop(s);
        drop(g);
        std::fs::remove_file(path).unwrap();
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
