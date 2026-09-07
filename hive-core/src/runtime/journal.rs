//! Durable HACP/2 runtime inputs and effect receipts, not a second protocol engine.
//!
//! SQLite commits precede transport exposure and process launch. Replay runs the
//! original inputs back through HACP and refuses divergence. Shell launch cannot be
//! atomic with a SQL transaction: an unfinished intent must be reconciled against
//! the host, never treated as permission to repeat its effects.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use hacp::v2::Envelope;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::collab::SessionSpec;

#[derive(Clone)]
pub struct Journal(Arc<Mutex<Connection>>, Arc<Mutex<Option<usize>>>,
    #[cfg(test)] Arc<std::sync::atomic::AtomicBool>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationRecord {
    pub spec: SessionSpec,
    pub started_unix: i64,
    pub result: Option<Value>,
}

impl Journal {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = crate::private_db::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS runtime_meta (
                 key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS runtime_outbox (
                 side TEXT NOT NULL, ordinal INTEGER NOT NULL,
                 session_id TEXT NOT NULL, message_id TEXT NOT NULL, envelope TEXT NOT NULL,
                 PRIMARY KEY(side, ordinal), UNIQUE(session_id, message_id));
             CREATE TABLE IF NOT EXISTS runtime_inbox (
                 recipient TEXT NOT NULL, session_id TEXT NOT NULL,
                 message_id TEXT NOT NULL, envelope TEXT NOT NULL,
                 PRIMARY KEY(recipient, session_id, message_id));
             CREATE TABLE IF NOT EXISTS runtime_invocations (
                 name TEXT PRIMARY KEY, spec TEXT NOT NULL,
                 started_unix INTEGER NOT NULL, result TEXT);
             CREATE TABLE IF NOT EXISTS runtime_checkpoints (
                 ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
                 label TEXT NOT NULL, state TEXT NOT NULL);",
        )?;
        Ok(Self(Arc::new(Mutex::new(conn)), Arc::new(Mutex::new(None)),
            #[cfg(test)] Arc::new(std::sync::atomic::AtomicBool::new(false))))
    }

    /// Fault injection after a durable boundary, before the caller can observe it.
    /// Disabled in production; shared clones see the same one-shot interruption.
    fn committed(&self) -> anyhow::Result<()> {
        let mut remaining = self.1.lock().map_err(|_| anyhow::anyhow!("fault counter poisoned"))?;
        if let Some(n) = remaining.as_mut() {
            *n -= 1;
            if *n == 0 {
                *remaining = None;
                #[cfg(all(test, unix))]
                if self.2.load(std::sync::atomic::Ordering::Relaxed) {
                    // Only an explicitly configured test child may take this path.
                    // SIGKILL bypasses Rust drops and SQLite connection cleanup.
                    unsafe { libc::kill(libc::getpid(), libc::SIGKILL); }
                    unreachable!("SIGKILL returned without terminating the test child");
                }
                anyhow::bail!("injected interruption after durable commit");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn interrupt_after(&self, commits: usize) {
        assert!(commits > 0);
        *self.1.lock().unwrap() = Some(commits);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn crash_after(&self, commits: usize) {
        self.interrupt_after(commits);
        self.2.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn conn(&self) -> anyhow::Result<MutexGuard<'_, Connection>> {
        self.0.lock().map_err(|_| anyhow::anyhow!("runtime journal mutex poisoned"))
    }

    pub fn get(&self, key: &str) -> anyhow::Result<Option<Value>> {
        let raw: Option<String> = self.conn()?.query_row(
            "SELECT value FROM runtime_meta WHERE key=?1", [key], |r| r.get(0),
        ).optional()?;
        raw.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }

    /// Insert once, returning the committed value if an earlier process got here first.
    pub fn initialize(&self, key: &str, value: &Value) -> anyhow::Result<Value> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO runtime_meta VALUES (?1, ?2)",
            params![key, serde_json::to_string(value)?])?;
        let raw: String = tx.query_row("SELECT value FROM runtime_meta WHERE key=?1", [key], |r| r.get(0))?;
        tx.commit()?;
        self.committed()?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Private mutable projection (for example the latest CLI conversation handle).
    /// Invocation input bindings remain immutable, so replay never substitutes the
    /// latest handle into an earlier call.
    pub fn remember(&self, key: &str, value: &Value) -> anyhow::Result<()> {
        self.conn()?.execute("INSERT INTO runtime_meta VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, serde_json::to_string(value)?])?;
        self.committed()?;
        Ok(())
    }

    /// Save a projection for inspection. The immutable messages remain replay truth.
    pub fn checkpoint(&self, label: &str, state: &Value) -> anyhow::Result<()> {
        self.conn()?.execute("INSERT INTO runtime_checkpoints(label,state) VALUES (?1,?2)",
            params![label, serde_json::to_string(state)?])?;
        self.committed()?;
        Ok(())
    }

    /// Commit before writing any transport file. On replay retain the exact original
    /// envelope, including ID and timestamp, and compare all application inputs.
    pub fn emit(&self, side: &str, ordinal: u32, proposed: Envelope) -> anyhow::Result<Envelope> {
        proposed.validate()?;
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let raw: Option<String> = tx.query_row(
            "SELECT envelope FROM runtime_outbox WHERE side=?1 AND ordinal=?2",
            params![side, ordinal], |r| r.get(0),
        ).optional()?;
        let envelope = if let Some(raw) = raw {
            let saved: Envelope = serde_json::from_str(&raw)?;
            let mut replay = proposed;
            replay.message_id.clone_from(&saved.message_id);
            replay.timestamp.clone_from(&saved.timestamp);
            anyhow::ensure!(saved == replay, "protocol replay diverged at {side}:{ordinal}");
            saved
        } else {
            tx.execute("INSERT INTO runtime_outbox VALUES (?1,?2,?3,?4,?5)", params![
                side, ordinal, proposed.session_id, proposed.message_id, serde_json::to_string(&proposed)?
            ])?;
            proposed
        };
        tx.commit()?;
        self.committed()?;
        Ok(envelope)
    }

    pub fn outbound(&self, side: &str, ordinal: u32) -> anyhow::Result<Envelope> {
        let raw: String = self.conn()?.query_row(
            "SELECT envelope FROM runtime_outbox WHERE side=?1 AND ordinal=?2",
            params![side, ordinal], |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Called only after routing/session authorization. Receipt is the durable ACK;
    /// retransmission after ACK loss yields the same receipt, not another event.
    pub fn receive(&self, recipient: &str, envelope: &Envelope) -> anyhow::Result<bool> {
        envelope.validate()?;
        anyhow::ensure!(envelope.to == recipient, "receipt recipient differs from envelope");
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let raw: Option<String> = tx.query_row(
            "SELECT envelope FROM runtime_inbox WHERE recipient=?1 AND session_id=?2 AND message_id=?3",
            params![recipient, envelope.session_id, envelope.message_id], |r| r.get(0),
        ).optional()?;
        if let Some(raw) = raw {
            let saved: Envelope = serde_json::from_str(&raw)?;
            anyhow::ensure!(&saved == envelope, "message ID reused with different content");
            return Ok(false);
        }
        tx.execute("INSERT INTO runtime_inbox VALUES (?1,?2,?3,?4)", params![
            recipient, envelope.session_id, envelope.message_id, serde_json::to_string(envelope)?
        ])?;
        tx.commit()?;
        self.committed()?;
        Ok(true)
    }

    pub fn pending(&self) -> anyhow::Result<Vec<Envelope>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT o.envelope FROM runtime_outbox o WHERE NOT EXISTS
             (SELECT 1 FROM runtime_inbox i WHERE i.session_id=o.session_id
              AND i.message_id=o.message_id) ORDER BY o.rowid")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|raw| Ok(serde_json::from_str(&raw?)?)).collect()
    }

    /// Return `true` only to the caller that committed the first launch intent.
    pub fn begin_invocation(&self, spec: &SessionSpec) -> anyhow::Result<(bool, InvocationRecord)> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let inserted = tx.execute("INSERT OR IGNORE INTO runtime_invocations VALUES (?1,?2,?3,NULL)",
            params![spec.name, serde_json::to_string(spec)?, chrono::Utc::now().timestamp()])? == 1;
        let (raw, started_unix, result): (String, i64, Option<String>) = tx.query_row(
            "SELECT spec,started_unix,result FROM runtime_invocations WHERE name=?1", [&spec.name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let saved: SessionSpec = serde_json::from_str(&raw)?;
        anyhow::ensure!(&saved == spec, "invocation replay diverged for {}", spec.name);
        let record = InvocationRecord { spec: saved, started_unix,
            result: result.map(|raw| serde_json::from_str(&raw)).transpose()? };
        tx.commit()?;
        self.committed()?;
        Ok((inserted, record))
    }

    pub fn finish_invocation(&self, name: &str, result: &Value) -> anyhow::Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE runtime_invocations SET result=?2 WHERE name=?1 AND result IS NULL",
            params![name, serde_json::to_string(result)?])?;
        anyhow::ensure!(changed == 1, "invocation {name} has no unfinished launch intent");
        self.committed()?;
        Ok(())
    }

    pub fn invocation(&self, name: &str) -> anyhow::Result<InvocationRecord> {
        let (spec, started_unix, result): (String, i64, Option<String>) = self.conn()?.query_row(
            "SELECT spec,started_unix,result FROM runtime_invocations WHERE name=?1", [name],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(InvocationRecord { spec:serde_json::from_str(&spec)?, started_unix,
            result:result.map(|r| serde_json::from_str(&r)).transpose()? })
    }

    pub fn invocation_if_exists(&self, name: &str) -> anyhow::Result<Option<InvocationRecord>> {
        match self.invocation(name) {
            Ok(record) => Ok(Some(record)),
            Err(error) if matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Record human/operator authority before sending SIGCONT. The original paused
    /// receipt is retained in the append-only audit trail, and the same invocation
    /// gets a renewed timeout budget, never a new execution identity.
    pub fn authorize_continue(&self, name: &str, reason: &str, approved_bytes: u64) -> anyhow::Result<()> {
        anyhow::ensure!(!reason.trim().is_empty(), "continuation requires an audit reason");
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let previous:String = tx.query_row("SELECT result FROM runtime_invocations WHERE name=?1", [name], |r| r.get(0))?;
        let result:Value = serde_json::from_str(&previous)?;
        anyhow::ensure!(result["outcome"] == "TimedOut" || result["outcome"].get("Paused").is_some(),
            "only a recorded paused or timed-out invocation can be continued");
        let now = chrono::Utc::now().timestamp();
        tx.execute("INSERT INTO runtime_checkpoints(label,state) VALUES ('operator-continue',?1)",
            [serde_json::to_string(&json!({"session":name,"reason":reason,"approved_bytes":approved_bytes,"requested_unix":now,"previous_result":result}))?])?;
        for (key, value) in [(format!("approved-log:{name}"), json!(approved_bytes)), (format!("continue-pending:{name}"), json!(true))] {
            tx.execute("INSERT INTO runtime_meta VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, serde_json::to_string(&value)?])?;
        }
        tx.execute("UPDATE runtime_invocations SET result=NULL,started_unix=?2 WHERE name=?1", params![name,now])?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }

    pub fn inspect(&self) -> anyhow::Result<Value> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT name,started_unix,result FROM runtime_invocations ORDER BY rowid")?;
        let invocations: Vec<Value> = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))?
            .map(|row| { let (name, started, result) = row?; Ok(json!({"name":name,"started_unix":started,
                "result":result.map(|r| serde_json::from_str::<Value>(&r)).transpose()?})) })
            .collect::<anyhow::Result<_>>()?;
        let latest: Option<(String, String)> = conn.query_row(
            "SELECT label,state FROM runtime_checkpoints ORDER BY ordinal DESC LIMIT 1", [],
            |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let outbox: u64 = conn.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r.get(0))?;
        let inbox: u64 = conn.query_row("SELECT count(*) FROM runtime_inbox", [], |r| r.get(0))?;
        Ok(json!({"outbox":outbox,"receipts":inbox,"invocations":invocations,
            "checkpoint":latest.map(|(label,raw)| -> anyhow::Result<Value> { Ok(json!({"label":label,"state":serde_json::from_str::<Value>(&raw)?})) }).transpose()?}))
    }
}

/// OS-backed exclusive writer ownership: automatically released on process death.
/// A stale PID file is never used as evidence that another coordinator is alive.
pub struct RunLock(std::fs::File);

impl RunLock {
    #[cfg(unix)]
    pub fn acquire(root: &Path) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true)
            .truncate(false).mode(0o600).open(root.join("runtime.lock"))?;
        // SAFETY: valid owned fd; flock does not read or write caller memory.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        anyhow::ensure!(result == 0, "another coordinator owns this run: {}", std::io::Error::last_os_error());
        Ok(Self(file))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Reading the member documents that its lifetime, not its pathname, is the lock.
        let _ = &self.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "urn:hacp:agent:a";
    const B: &str = "urn:hacp:agent:b";

    #[test]
    fn restart_retains_outbox_and_receipt_then_rejects_divergent_replay() {
        let dir = crate::runtime::Scratch::new("journal");
        let path = dir.join("runtime.db");
        let env = Envelope::new("s-1", A, B, "heartbeat", json!({}));
        let j = Journal::open(&path).unwrap();
        j.emit("a", 1, env.clone()).unwrap();
        drop(j); // interrupted before transport exposure or receiver ACK
        let j = Journal::open(&path).unwrap();
        assert_eq!(j.pending().unwrap(), vec![env.clone()]);
        assert!(j.receive(B, &env).unwrap());
        drop(j);
        let j = Journal::open(&path).unwrap();
        assert!(!j.receive(B, &env).unwrap());
        assert!(j.pending().unwrap().is_empty());
        let same = Envelope::new("s-1", A, B, "heartbeat", json!({}));
        assert_eq!(j.emit("a", 1, same).unwrap(), env);
        let mut bad = env.clone();
        bad.body = json!({"changed":true});
        assert!(j.receive(B, &bad).is_err());
        assert!(j.emit("a", 1, bad).is_err());
        assert_eq!(j.outbound("a", 1).unwrap(), env);
    }

    #[test]
    fn writer_lock_is_exclusive_and_released_without_removing_the_file() {
        let dir = crate::runtime::Scratch::new("run-lock");
        let first = RunLock::acquire(&dir.dir).unwrap();
        assert!(RunLock::acquire(&dir.dir).is_err());
        drop(first);
        assert!(RunLock::acquire(&dir.dir).is_ok());
    }

    #[test]
    fn launch_intent_survives_interruption_without_permitting_a_second_launch() {
        let dir = crate::runtime::Scratch::new("invocation-journal");
        let path = dir.join("runtime.db");
        let spec = SessionSpec { name:"hive-test".into(), program:"sh".into(), args:vec![],
            prompt:"task".into(), cwd:dir.dir.clone(), log:dir.join("log"), timeout_secs:10 };
        let j = Journal::open(&path).unwrap();
        assert!(j.begin_invocation(&spec).unwrap().0);
        drop(j);
        let j = Journal::open(&path).unwrap();
        let (new, record) = j.begin_invocation(&spec).unwrap();
        assert!(!new);
        assert!(record.result.is_none());
        j.finish_invocation(&spec.name, &json!({"exit":0})).unwrap();
        assert_eq!(j.begin_invocation(&spec).unwrap().1.result, Some(json!({"exit":0})));
        let mut changed = spec.clone();
        changed.prompt = "different task".into();
        assert!(j.begin_invocation(&changed).is_err());
        assert!(j.finish_invocation(&spec.name, &json!({"exit":1})).is_err());
    }

    #[test]
    fn operator_continuation_retains_the_pause_receipt_and_never_permits_relaunch() {
        let scratch = crate::runtime::Scratch::new("continue-journal");
        let j = Journal::open(&scratch.join("runtime.db")).unwrap();
        let spec = SessionSpec { name:"hive-continued".into(), program:"sh".into(), args:vec![], prompt:String::new(),
            cwd:scratch.dir.clone(), log:scratch.join("log"), timeout_secs:30 };
        j.begin_invocation(&spec).unwrap();
        let paused = json!({"outcome":{"Paused":{"reason":"review"}},"call":{"role":"worker"}});
        j.finish_invocation(&spec.name, &paused).unwrap();
        assert!(j.authorize_continue(&spec.name, "", 123).is_err());
        j.authorize_continue(&spec.name, "reviewed this scratch-only operation", 123).unwrap();
        let (new, record) = j.begin_invocation(&spec).unwrap();
        assert!(!new);
        assert!(record.result.is_none());
        assert_eq!(j.get("approved-log:hive-continued").unwrap(), Some(json!(123)));
        assert_eq!(j.inspect().unwrap()["checkpoint"]["state"]["previous_result"], paused);
        assert!(j.authorize_continue(&spec.name, "duplicate approval", 123).is_err());
    }
}
