//! Coordinator-owned relay attestation, not end-to-end agent authentication.
//! HACP Secure degraded mode: agents and their journals remain untrusted.
//!
//! The pinned HACP guardian intentionally keeps IdentitySecret/sign/verify and
//! seed import/export private. This DB-custody adapter uses the same Ed25519
//! primitive (dalek, strict verification) and HACP canonical digests; it never
//! creates a file-backed guardian or exposes a general signing endpoint.
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hacp::v2::canon;
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const MODE: &str = "Relay-attested (HACP Secure degraded mode)";
const DOMAIN: &str = "HIVE/HACP-SECURE/relay/v1\n";
const LEASE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub public_key: String,
    pub fingerprint: String,
}
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub per_run: u32,
    pub per_task: u32,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            per_run: 10,
            per_task: 30,
        }
    }
}
impl Budget {
    pub fn from_env() -> anyhow::Result<Self> {
        fn cap(name: &str, default: u32) -> anyhow::Result<u32> {
            let value = match std::env::var(name) {
                Ok(value) => value.parse()?,
                Err(std::env::VarError::NotPresent) => default,
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(value > 0, "{name} must be positive");
            Ok(value)
        }
        Ok(Self {
            per_run: cap("HIVE_RELAY_RUN_PER_MINUTE", 10)?,
            per_task: cap("HIVE_RELAY_TASK_PER_MINUTE", 30)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub id: String,
    pub task_id: String,
    pub source: String,
    pub destination: String,
    pub kind: String,
    pub text_digest: String,
    pub seq: i64,
    pub staged_at: String,
}
impl Envelope {
    fn signing_input(&self) -> anyhow::Result<Vec<u8>> {
        Ok(format!(
            "{DOMAIN}{}",
            canon::canonical_json(&serde_json::to_value(self)?)?
        )
        .into_bytes())
    }
}

/// A lease binds the exact verified payload to its acknowledgment. No key material.
pub struct Delivery {
    pub payload: Value,
    pub id: String,
    pub(crate) token: String,
}

pub(super) fn schema(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS delegated_relay_keys (
        run_id TEXT PRIMARY KEY, seed BLOB NOT NULL, public_key TEXT NOT NULL, fingerprint TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS delegated_relay_pairs (
        source TEXT NOT NULL, destination TEXT NOT NULL, staged_seq INTEGER NOT NULL DEFAULT 0,
        delivered_seq INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(source,destination));
        CREATE TABLE IF NOT EXISTS delegated_relay_envelopes (
        message_id TEXT PRIMARY KEY, envelope TEXT NOT NULL, signature TEXT NOT NULL,
        rejected INTEGER NOT NULL DEFAULT 0, hold_reason TEXT, token TEXT, lease_until INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS delegated_relay_budget (
        message_id TEXT NOT NULL, source TEXT NOT NULL, task_id TEXT NOT NULL, attempted_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS relay_budget_time ON delegated_relay_budget(attempted_at);
        CREATE TABLE IF NOT EXISTS delegated_relay_incidents (
        id INTEGER PRIMARY KEY, source TEXT NOT NULL, destination TEXT NOT NULL,
        message_id TEXT NOT NULL, kind TEXT NOT NULL, reason TEXT NOT NULL, created_at TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS delegated_relay_audit (
        position INTEGER PRIMARY KEY, record TEXT NOT NULL, digest TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS delegated_relay_head (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), position INTEGER NOT NULL, digest TEXT NOT NULL);
        INSERT OR IGNORE INTO delegated_relay_head VALUES(1,0,'');
        CREATE TABLE IF NOT EXISTS delegated_relay_verified (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), position INTEGER NOT NULL,
        digest TEXT NOT NULL, valid INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS relay_audit_source ON delegated_relay_audit(json_extract(record,'$.source'));
        CREATE INDEX IF NOT EXISTS relay_audit_destination ON delegated_relay_audit(json_extract(record,'$.destination'));
        CREATE TRIGGER IF NOT EXISTS relay_audit_no_update BEFORE UPDATE ON delegated_relay_audit
        BEGIN SELECT RAISE(ABORT,'relay audit is append-only'); END;
        CREATE TRIGGER IF NOT EXISTS relay_audit_no_delete BEFORE DELETE ON delegated_relay_audit
        BEGIN SELECT RAISE(ABORT,'relay audit is append-only'); END;")?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex<const N: usize>(s: &str) -> anyhow::Result<[u8; N]> {
    anyhow::ensure!(
        s.len() == N * 2 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid relay encoding"
    );
    let mut out = [0; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

pub(super) fn create_identity(db: &Connection, run: &str) -> anyhow::Result<()> {
    let mut seed = Zeroizing::new([0u8; 32]);
    OsRng
        .try_fill_bytes(&mut *seed)
        .map_err(|_| anyhow::anyhow!("Relay entropy unavailable"))?;
    let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    db.execute(
        "INSERT INTO delegated_relay_keys VALUES(?,?,?,?)",
        params![
            run,
            seed.as_slice(),
            hex(&public),
            hex(&Sha256::digest(public))
        ],
    )?;
    Ok(())
}

pub(super) fn identity(db: &Connection, run: &str) -> anyhow::Result<PublicIdentity> {
    Ok(db.query_row(
        "SELECT public_key,fingerprint FROM delegated_relay_keys WHERE run_id=?",
        [run],
        |r| {
            Ok(PublicIdentity {
                public_key: r.get(0)?,
                fingerprint: r.get(1)?,
            })
        },
    )?)
}

fn sign(db: &Connection, envelope: &Envelope) -> anyhow::Result<String> {
    let seed = Zeroizing::new(db.query_row(
        "SELECT seed FROM delegated_relay_keys WHERE run_id=?",
        [&envelope.source],
        |r| r.get::<_, Vec<u8>>(0),
    )?);
    let bytes: &[u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid relay identity"))?;
    Ok(hex(&SigningKey::from_bytes(bytes)
        .sign(&envelope.signing_input()?)
        .to_bytes()))
}

fn payload_fields<'a>(
    id: &str,
    source: &str,
    payload: &'a Value,
) -> anyhow::Result<(&'a str, &'a str)> {
    let object = payload
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Invalid relay payload"))?;
    anyhow::ensure!(
        object
            .keys()
            .all(|k| ["id", "source", "kind", "text"].contains(&k.as_str())),
        "Unknown relay payload field"
    );
    anyhow::ensure!(
        payload["id"].as_str() == Some(id),
        "Relay payload ID mismatch"
    );
    if let Some(value) = object.get("source") {
        anyhow::ensure!(value.as_str() == Some(source), "Relay source mismatch");
    }
    let kind = match object.get("kind") {
        Some(k) => k
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid relay kind"))?,
        None => "message",
    };
    let text = payload["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid relay text"))?;
    Ok((kind, text))
}

fn timestamp(now: i64) -> anyhow::Result<String> {
    Ok(chrono::DateTime::from_timestamp(now, 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid relay time"))?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string())
}

pub(super) fn audit(
    db: &Connection,
    event: &str,
    envelope: &Envelope,
    detail: Value,
    now: i64,
) -> anyhow::Result<()> {
    let (position, previous): (i64, String) = db.query_row(
        "SELECT position,digest FROM delegated_relay_head WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let record = json!({"position":position+1,"previous_digest":previous,"event":event,"at":timestamp(now)?,"message_id":envelope.id,"source":envelope.source,"destination":envelope.destination,"task_id":envelope.task_id,"seq":envelope.seq,"detail":detail});
    let digest = canon::digest_of(&record)?;
    db.execute(
        "INSERT INTO delegated_relay_audit VALUES(?,?,?)",
        params![position + 1, canon::canonical_json(&record)?, digest],
    )?;
    db.execute(
        "UPDATE delegated_relay_head SET position=?,digest=? WHERE singleton=1",
        params![position + 1, digest],
    )?;
    Ok(())
}

pub(super) fn stage(
    db: &Connection,
    id: &str,
    source: &str,
    destination: &str,
    payload: &Value,
    now: i64,
) -> anyhow::Result<()> {
    let (kind, text) = payload_fields(id, source, payload)?;
    let task: String = db.query_row(
        "SELECT task_id FROM delegated_runs WHERE id=?",
        [destination],
        |r| r.get(0),
    )?;
    // Trusted coordinator/user sources also get DB-only identities. They are not
    // charged to the peer budget, so a flooded agent cannot block human control.
    if source == "user" || source == "coordinator" {
        if identity(db, source).is_err() {
            create_identity(db, source)?;
        }
    } else {
        let source_task: String = db.query_row(
            "SELECT task_id FROM delegated_runs WHERE id=?",
            [source],
            |r| r.get(0),
        )?;
        anyhow::ensure!(source_task == task, "Peer message crosses task boundary");
    }
    db.execute(
        "INSERT OR IGNORE INTO delegated_relay_pairs(source,destination) VALUES(?,?)",
        params![source, destination],
    )?;
    db.execute(
        "UPDATE delegated_relay_pairs SET staged_seq=staged_seq+1 WHERE source=? AND destination=?",
        params![source, destination],
    )?;
    let seq = db.query_row(
        "SELECT staged_seq FROM delegated_relay_pairs WHERE source=? AND destination=?",
        params![source, destination],
        |r| r.get(0),
    )?;
    let envelope = Envelope {
        id: id.into(),
        task_id: task,
        source: source.into(),
        destination: destination.into(),
        kind: kind.into(),
        text_digest: canon::digest_canonical(text),
        seq,
        staged_at: timestamp(now)?,
    };
    let signature = sign(db, &envelope)?;
    db.execute(
        "INSERT INTO delegated_relay_envelopes(message_id,envelope,signature) VALUES(?,?,?)",
        params![
            id,
            canon::canonical_json(&serde_json::to_value(&envelope)?)?,
            signature
        ],
    )?;
    audit(
        db,
        "stage",
        &envelope,
        json!({"envelope":envelope,"signature":signature,"identity":identity(db,source)?}),
        now,
    )
}

// Use routing from delegated_messages, not a potentially corrupt envelope, to
// attribute an incident. This also handles legacy unsigned rows without signing
// history retroactively.
fn routing(db: &Connection, id: &str) -> anyhow::Result<Envelope> {
    Ok(db.query_row("SELECT m.source,m.destination,r.task_id FROM delegated_messages m JOIN delegated_runs r ON r.id=m.destination WHERE m.id=?", [id], |r| {
        Ok(Envelope { id:id.into(),source:r.get(0)?,destination:r.get(1)?,task_id:r.get(2)?,kind:String::new(),text_digest:String::new(),seq:0,staged_at:String::new() })
    })?)
}

/// Records an integrity incident at most once per (message, reason), so an
/// untrusted journal that keeps re-emitting the same conflict cannot grow the
/// append-only chain on every sync retry.
pub(super) fn incident(db: &Connection, id: &str, reason: &str, now: i64) -> anyhow::Result<()> {
    let seen: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM delegated_relay_incidents WHERE message_id=? AND reason=?)",
        params![id, reason],
        |r| r.get(0),
    )?;
    if seen {
        return Ok(());
    }
    let envelope = routing(db, id)?;
    db.execute("INSERT INTO delegated_relay_incidents(source,destination,message_id,kind,reason,created_at) VALUES(?,?,?,'integrity',?,?)", params![envelope.source,envelope.destination,id,reason,timestamp(now)?])?;
    audit(
        db,
        "reject",
        &envelope,
        json!({"kind":"integrity","reason":reason}),
        now,
    )
}

fn verify(db: &Connection, id: &str, payload: &Value) -> anyhow::Result<Envelope> {
    let route = routing(db, id)?;
    let (encoded, signature): (String, String) = db.query_row(
        "SELECT envelope,signature FROM delegated_relay_envelopes WHERE message_id=?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let envelope: Envelope = serde_json::from_str(&encoded)?;
    anyhow::ensure!(
        envelope.id == id
            && envelope.source == route.source
            && envelope.destination == route.destination
            && envelope.task_id == route.task_id
            && envelope.seq > 0,
        "Relay routing mismatch"
    );
    let (kind, text) = payload_fields(id, &route.source, payload)?;
    anyhow::ensure!(
        envelope.kind == kind && envelope.text_digest == canon::digest_canonical(text),
        "Relay payload digest mismatch"
    );
    canon::validate_timestamp(&envelope.staged_at)?;
    let public = identity(db, &envelope.source)?;
    let key = VerifyingKey::from_bytes(&unhex(&public.public_key)?)?;
    anyhow::ensure!(
        hex(&Sha256::digest(key.as_bytes())) == public.fingerprint,
        "Relay fingerprint mismatch"
    );
    key.verify_strict(
        &envelope.signing_input()?,
        &Signature::from_bytes(&unhex(&signature)?),
    )
    .map_err(|_| anyhow::anyhow!("Relay signature mismatch"))?;
    let last: i64 = db.query_row(
        "SELECT delivered_seq FROM delegated_relay_pairs WHERE source=? AND destination=?",
        params![envelope.source, envelope.destination],
        |r| r.get(0),
    )?;
    anyhow::ensure!(envelope.seq > last, "Relay replay or out-of-order sequence");
    Ok(envelope)
}

/// Called in an IMMEDIATE transaction: verification, budget reservation and lease
/// are indivisible across concurrent destination syncs and coordinator processes.
pub(super) fn next(
    db: &Connection,
    run: &str,
    budget: Budget,
    now: i64,
) -> anyhow::Result<Option<Delivery>> {
    let mut stmt = db.prepare("SELECT m.id,m.payload FROM delegated_messages m LEFT JOIN delegated_relay_envelopes e ON e.message_id=m.id WHERE m.destination=? AND m.delivered=0 AND coalesce(e.rejected,0)=0 ORDER BY m.rowid")?;
    let messages = stmt
        .query_map([run], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut blocked_sources = std::collections::HashSet::new();
    for (id, raw) in messages {
        let route = routing(db, &id)?;
        if blocked_sources.contains(&route.source) {
            continue;
        }
        let payload = serde_json::from_str::<Value>(&raw);
        let checked = payload
            .as_ref()
            .map_err(|_| anyhow::anyhow!("Invalid relay JSON"))
            .and_then(|p| verify(db, &id, p));
        let envelope = match checked {
            Ok(envelope) => envelope,
            Err(_) => {
                // Fixed failure text prevents leaking payload or database internals.
                incident(
                    db,
                    &id,
                    "Signature, payload, routing or sequence verification failed",
                    now,
                )?;
                db.execute("INSERT INTO delegated_relay_envelopes(message_id,envelope,signature,rejected) VALUES(?,'{}','',1) ON CONFLICT(message_id) DO UPDATE SET rejected=1,hold_reason=NULL,token=NULL,lease_until=0", [&id])?;
                continue;
            }
        };
        let lease: i64 = db.query_row(
            "SELECT lease_until FROM delegated_relay_envelopes WHERE message_id=?",
            [&id],
            |r| r.get(0),
        )?;
        if lease > now {
            blocked_sources.insert(envelope.source);
            continue;
        }
        let peer = envelope.source != "user" && envelope.source != "coordinator";
        if peer {
            db.execute("DELETE FROM delegated_relay_budget WHERE attempted_at<=?", [now-60])?;
            let (per_run, per_task): (u32,u32) = db.query_row("SELECT count(CASE WHEN source=? THEN 1 END),count(*) FROM delegated_relay_budget WHERE task_id=? AND attempted_at>?", params![envelope.source,envelope.task_id,now-60], |r| Ok((r.get(0)?,r.get(1)?)))?;
            if per_run >= budget.per_run || per_task >= budget.per_task {
                let reason = if per_run >= budget.per_run {
                    "Per-run peer message budget exhausted; held for the rolling 60-second window"
                } else {
                    "Per-task peer message budget exhausted; held for the rolling 60-second window"
                };
                let old: Option<String> = db.query_row(
                    "SELECT hold_reason FROM delegated_relay_envelopes WHERE message_id=?",
                    [&id],
                    |r| r.get(0),
                )?;
                if old.as_deref() != Some(reason) {
                    db.execute(
                        "UPDATE delegated_relay_envelopes SET hold_reason=? WHERE message_id=?",
                        params![reason, id],
                    )?;
                    audit(db, "hold", &envelope, json!({"reason":reason}), now)?;
                }
                blocked_sources.insert(envelope.source);
                continue;
            }
            db.execute(
                "INSERT INTO delegated_relay_budget VALUES(?,?,?,?)",
                params![id, envelope.source, envelope.task_id, now],
            )?;
        }
        let token = uuid::Uuid::new_v4().to_string();
        db.execute("UPDATE delegated_relay_envelopes SET token=?,lease_until=?,hold_reason=NULL WHERE message_id=?",params![token,now+LEASE_SECONDS,id])?;
        audit(
            db,
            "verify",
            &envelope,
            json!({"result":"verified","lease_until":now+LEASE_SECONDS}),
            now,
        )?;
        return Ok(Some(Delivery {
            id,
            payload: payload?,
            token,
        }));
    }
    Ok(None)
}

pub(super) fn delivered(db: &Connection, delivery: &Delivery, now: i64) -> anyhow::Result<()> {
    let current: Option<String> = db.query_row(
        "SELECT token FROM delegated_relay_envelopes WHERE message_id=?",
        [&delivery.id],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        current.as_deref() == Some(&delivery.token),
        "Relay delivery lease was superseded"
    );
    let envelope = verify(db, &delivery.id, &delivery.payload)?;
    db.execute(
        "UPDATE delegated_messages SET delivered=1 WHERE id=?",
        [&delivery.id],
    )?;
    db.execute(
        "UPDATE delegated_relay_pairs SET delivered_seq=? WHERE source=? AND destination=?",
        params![envelope.seq, envelope.source, envelope.destination],
    )?;
    db.execute(
        "UPDATE delegated_relay_envelopes SET token=NULL,lease_until=0 WHERE message_id=?",
        [&delivery.id],
    )?;
    audit(
        db,
        "deliver",
        &envelope,
        json!({"result":"inbox accepted"}),
        now,
    )
}

pub(super) fn release(db: &Connection, delivery: &Delivery) -> anyhow::Result<()> {
    db.execute("UPDATE delegated_relay_envelopes SET token=NULL,lease_until=0 WHERE message_id=? AND token=?",params![delivery.id,delivery.token])?;
    Ok(())
}

pub(super) fn status(db: &Connection, run: &str) -> anyhow::Result<Value> {
    let mut stmt = db.prepare("SELECT message_id,reason,created_at FROM delegated_relay_incidents WHERE source=? OR destination=? ORDER BY id DESC LIMIT 100")?;
    let incidents = stmt.query_map([run,run], |r| Ok(json!({"kind":"integrity","message_id":r.get::<_,String>(0)?,"reason":r.get::<_,String>(1)?,"created_at":r.get::<_,String>(2)?})))?.collect::<Result<Vec<_>,_>>()?;
    let mut stmt = db.prepare("SELECT m.id,e.hold_reason FROM delegated_messages m JOIN delegated_relay_envelopes e ON e.message_id=m.id WHERE (m.source=? OR m.destination=?) AND m.delivered=0 AND e.rejected=0 AND e.hold_reason IS NOT NULL ORDER BY m.rowid LIMIT 100")?;
    let held = stmt
        .query_map([run, run], |r| {
            Ok(json!({"message_id":r.get::<_,String>(0)?,"reason":r.get::<_,String>(1)?}))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"mode":MODE,"incidents":incidents,"held":held}))
}

/// Verifies audit rows after `(position, digest)` and returns the last verified
/// link, whether every link held, and how many rows were read.
fn verify_chain(
    db: &Connection,
    mut position: i64,
    mut previous: String,
) -> anyhow::Result<(i64, String, bool, u64)> {
    let mut stmt = db.prepare(
        "SELECT position,record,digest FROM delegated_relay_audit WHERE position>? ORDER BY position",
    )?;
    let rows = stmt.query_map([position], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let (mut valid, mut read) = (true, 0);
    for row in rows {
        let (index, raw, digest) = row?;
        read += 1;
        let record: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        valid &= index == position + 1
            && record["position"] == index
            && record["previous_digest"] == previous
            && canon::digest_of(&record).ok().as_ref() == Some(&digest);
        position = index;
        previous = digest;
    }
    Ok((position, previous, valid, read))
}

/// Serves the audit chain for one run. By default only rows appended since the
/// persisted checkpoint are hashed, so the 5-second UI poll is O(new rows) rather
/// than O(history). `full` re-verifies the whole chain from genesis (used at
/// startup and on demand) and is the only mode that detects edits to rows
/// older than the checkpoint. Must run inside a write transaction.
pub(super) fn audit_report(db: &Connection, run: &str, full: bool) -> anyhow::Result<Value> {
    let head: (i64, String) = db.query_row(
        "SELECT position,digest FROM delegated_relay_head WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let checkpoint: Option<(i64, String, bool)> = db
        .query_row(
            "SELECT position,digest,valid FROM delegated_relay_verified WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let full = full || checkpoint.is_none();
    let (valid, position, digest, read) = match checkpoint.filter(|_| !full) {
        // A failed chain stays failed until a full re-verification clears it.
        Some((position, digest, false)) => (false, position, digest, 0),
        Some((position, digest, true)) => {
            // The anchor row must still hold the digest verified earlier; this
            // detects truncation of the tail and a rewound head in O(1).
            let anchored = position == 0
                || db
                    .query_row(
                        "SELECT digest FROM delegated_relay_audit WHERE position=?",
                        [position],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .as_ref()
                    == Some(&digest);
            let (end, last, linked, read) = verify_chain(db, position, digest.clone())?;
            let valid = anchored && linked && head == (end, last.clone());
            if valid {
                (true, end, last, read)
            } else {
                (false, position, digest, read)
            }
        }
        None => {
            let (end, last, linked, read) = verify_chain(db, 0, String::new())?;
            let valid = linked && head == (end, last.clone());
            (valid, end, last, read)
        }
    };
    db.execute(
        "INSERT INTO delegated_relay_verified VALUES(1,?,?,?) ON CONFLICT(singleton) DO UPDATE SET position=excluded.position,digest=excluded.digest,valid=excluded.valid",
        params![position, digest, valid],
    )?;
    let run_rows = "SELECT position,record,digest FROM delegated_relay_audit WHERE json_extract(record,'$.source')=?1 UNION SELECT position,record,digest FROM delegated_relay_audit WHERE json_extract(record,'$.destination')=?1";
    let total: i64 = db.query_row(
        &format!("SELECT count(*) FROM ({run_rows})"),
        [run],
        |r| r.get(0),
    )?;
    let mut stmt = db.prepare(&format!(
        "SELECT record,digest FROM ({run_rows}) ORDER BY position DESC LIMIT 200"
    ))?;
    let mut entries = stmt
        .query_map([run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|row| {
            let (raw, digest) = row?;
            let record: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            Ok(json!({"record":record,"digest":digest}))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    entries.reverse();
    Ok(json!({
        "mode":MODE,
        "chain_valid":valid,
        "verification":if full {"full"} else {"incremental"},
        "verified_rows":read,
        "verified_through":position,
        "head":{"position":head.0,"digest":head.1},
        "entries":entries,
        "total_entries":total,
    }))
}
