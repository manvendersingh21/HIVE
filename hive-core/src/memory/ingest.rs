//! Local-only, resumable ingestion shared by completed chats and the nightly job.
//!
//! A watermark belongs to a source *version*, not a wall-clock cutoff: old failed
//! sources, in-place assistant edits and late runner events must remain eligible.
//! All inference happens before a transaction; graph, RAG and watermark then
//! commit together. The monotonic batch watermark reports durable progress.
use std::sync::Arc;

use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{
    extractor::{self, Completer},
    graph::{Entity, KnowledgeGraph},
    rag::{Embedder, HashEmbedder, RagIndex},
};
use crate::llm::local::OllamaClient;
use hive_common::config::{HiveConfig, MemoryConfig};

#[derive(Debug, Default, Serialize)]
pub struct IngestCounts {
    pub conversations: usize,
    pub runs: usize,
    pub chunks: usize,
    pub entities_created: usize,
    pub entities_merged: usize,
    pub relations: usize,
    pub lessons: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub skipped: bool,
    pub watermark: i64,
}

struct Models {
    embedder: Arc<dyn Embedder>,
    completer: Arc<dyn Completer>,
    probe: Option<Arc<OllamaClient>>,
    extraction_model: String,
    rag: RagIndex,
}

#[derive(Clone)]
pub struct Ingestor {
    graph: KnowledgeGraph,
    config: MemoryConfig,
    models: Arc<Result<Models, String>>,
    gate: Arc<tokio::sync::Mutex<()>>,
}

struct NoExtraction;
#[async_trait::async_trait]
impl Completer for NoExtraction {
    async fn complete(&self, _: &str) -> anyhow::Result<String> {
        Ok(r#"{"entities":[],"relations":[]}"#.into())
    }
}

#[derive(Clone)]
struct Source {
    kind: &'static str,
    id: String,
    project: String,
    text: String,
    revision: i64,
    lesson: Option<Value>,
}

impl Source {
    fn rag_id(&self) -> String {
        if self.kind == "conversation" {
            self.id.clone()
        } else {
            format!("{}:{}", self.kind, self.id)
        }
    }
    fn fingerprint(&self, models: &Models, config: &MemoryConfig) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!(
                "ingest-v1:{}:{}:{}:{}:{}:{}",
                self.project,
                models.rag.fingerprint(&self.text),
                models.extraction_model,
                config.knowledge_graph.max_entities_per_conversation,
                config.knowledge_graph.entity_dedup_threshold,
                self.revision
            ))
        )
    }
}

impl Ingestor {
    pub(crate) fn new(graph: KnowledgeGraph, config: Option<&HiveConfig>) -> anyhow::Result<Self> {
        let memory = config.map(|c| c.memory.clone()).unwrap_or_default();
        let models = (|| -> anyhow::Result<Models> {
            let (embedder, completer, probe, extraction_model): (
                Arc<dyn Embedder>,
                Arc<dyn Completer>,
                _,
                _,
            ) = if let Some(c) = config {
                anyhow::ensure!(
                    c.llm.local.provider == "ollama",
                    "memory ingestion requires Ollama"
                );
                let local = Arc::new(
                    OllamaClient::local_only(
                        c.llm.local.base_url.clone(),
                        c.llm.local.model.clone(),
                    )?
                    .with_context(c.llm.local.max_context),
                );
                let embedder = Arc::new(OllamaClient::local_only(
                    c.llm.local.base_url.clone(),
                    c.memory.embedding_model.clone(),
                )?);
                (
                    embedder,
                    local.clone(),
                    Some(local),
                    c.llm.local.model.clone(),
                )
            } else {
                (
                    Arc::new(HashEmbedder { dim: 128 }),
                    Arc::new(NoExtraction),
                    None,
                    "disabled".into(),
                )
            };
            let rag = RagIndex::new(
                graph.shared_conn(),
                embedder.clone(),
                memory.chunk_size as usize,
                memory.chunk_overlap as usize,
            )?;
            Ok(Models {
                embedder,
                completer,
                probe,
                extraction_model,
                rag,
            })
        })()
        .map_err(|e| e.to_string());
        schema(&graph.shared_conn().lock().unwrap())?;
        extractor::create_table(&graph.shared_conn())?;
        Ok(Self {
            graph,
            config: memory,
            models: Arc::new(models),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Planner recall uses the same local vector space as automatic ingestion.
    pub async fn search(
        &self,
        project: &str,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<super::rag::RagHit>> {
        let models = self
            .models
            .as_ref()
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Some(probe) = &models.probe {
            probe
                .require_local_models(&[models.embedder.model()])
                .await?;
        }
        models.rag.search(Some(project), query, limit).await
    }

    pub fn auto_index(&self) -> bool {
        self.config.auto_index
    }

    pub fn nightly_config(&self) -> &hive_common::config::NightlyMemoryConfig {
        &self.config.nightly
    }

    /// At most one scheduled attempt per local date, including across restarts
    /// and a repeated DST hour. Manual triggers do not consume this claim.
    pub fn claim_nightly(&self, date: NaiveDate) -> anyhow::Result<bool> {
        Ok(self.graph.shared_conn().lock().unwrap().execute(
            "INSERT OR IGNORE INTO memory_nightly_attempts(day) VALUES (?1)",
            [date.to_string()],
        )? == 1)
    }

    pub fn watermark(&self) -> anyhow::Result<i64> {
        Ok(self.graph.shared_conn().lock().unwrap().query_row(
            "SELECT watermark FROM memory_ingest_state WHERE id=1",
            [],
            |r| r.get(0),
        )?)
    }

    pub async fn ingest(&self, date: NaiveDate) -> anyhow::Result<IngestCounts> {
        self.run(date, None).await
    }

    pub async fn conversation(&self, id: &str) -> anyhow::Result<IngestCounts> {
        self.run(chrono::Local::now().date_naive(), Some(id)).await
    }

    async fn run(
        &self,
        date: NaiveDate,
        conversation: Option<&str>,
    ) -> anyhow::Result<IngestCounts> {
        let _guard = self.gate.lock().await;
        let mut counts = IngestCounts {
            watermark: self.watermark()?,
            ..Default::default()
        };
        let models = match self.models.as_ref() {
            Ok(m) => m,
            Err(error) => {
                tracing::warn!(
                    error,
                    "memory ingest skipped: local Ollama configuration unavailable"
                );
                counts.skipped = true;
                return Ok(counts);
            }
        };
        if let Some(probe) = &models.probe {
            if let Err(error) = probe
                .require_local_models(&[&models.extraction_model, models.embedder.model()])
                .await
            {
                // One warning for the entire batch, not one per conversation.
                tracing::warn!(%error, "memory ingest skipped: local Ollama unavailable");
                counts.skipped = true;
                return Ok(counts);
            }
        }
        let conn = self.graph.shared_conn();
        let sources = sources(&conn.lock().unwrap(), conversation)?;
        let mut first_error = None;
        for source in sources {
            let fingerprint = source.fingerprint(models, &self.config);
            if current(&conn.lock().unwrap(), &source, &fingerprint)?
                && models.rag.is_current(&source.rag_id(), &source.text)?
            {
                counts.unchanged += 1;
                continue;
            }
            match self
                .ingest_source(models, &source, &fingerprint, date)
                .await
            {
                Ok(Some((chunks, extraction))) => {
                    if source.kind == "conversation" {
                        counts.conversations += 1;
                    } else {
                        counts.runs += 1;
                    }
                    counts.chunks += chunks;
                    counts.entities_created += extraction.entities_created;
                    counts.entities_merged += extraction.entities_merged;
                    counts.relations += extraction.relations_added;
                }
                Ok(None) => { /* Changed/deleted while inference ran: leave pending. */ }
                Err(e) => {
                    counts.failed += 1;
                    first_error.get_or_insert(e);
                }
            }
        }
        if conversation.is_none() {
            match self.lessons(models, date).await {
                Ok((lessons, chunks)) => {
                    counts.lessons = lessons;
                    counts.chunks += chunks;
                }
                Err(e) => {
                    counts.failed += 1;
                    first_error.get_or_insert(e);
                }
            }
        }
        if let Some(error) = first_error {
            tracing::warn!(%error, failed = counts.failed, "memory ingest incomplete; pending sources will be retried");
        }
        counts.watermark = self.watermark()?;
        Ok(counts)
    }

    async fn ingest_source(
        &self,
        models: &Models,
        source: &Source,
        fingerprint: &str,
        date: NaiveDate,
    ) -> anyhow::Result<Option<(usize, extractor::ExtractionOutcome)>> {
        let conn = self.graph.shared_conn();
        let chunks = models.rag.prepare(&source.text).await?;
        let extraction = extractor::prepare(
            &self.graph,
            &conn,
            models.embedder.as_ref(),
            models.completer.as_ref(),
            self.config.knowledge_graph.max_entities_per_conversation as usize,
            self.config.knowledge_graph.entity_dedup_threshold,
            &source.project,
            &source.text,
        )
        .await?;
        let mut db = conn.lock().unwrap();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Do not resurrect deleted history or mark a newer edit ingested. A
        // second process may also have finished the same source meanwhile.
        let fresh = load_source(&tx, source.kind, &source.id)?;
        if fresh
            .as_ref()
            .is_none_or(|s| s.fingerprint(models, &self.config) != fingerprint)
            || (current(&tx, source, fingerprint)?
                && models
                    .rag
                    .is_current_on(&tx, &source.rag_id(), &source.text)?)
        {
            return Ok(None);
        }
        tx.execute(
            "INSERT OR IGNORE INTO projects(id,name) VALUES (?1,?1)",
            [&source.project],
        )?;
        let outcome = extraction.store_on(&tx, &source.project, models.embedder.as_ref())?;
        models
            .rag
            .replace_on(&tx, &source.project, &source.rag_id(), &chunks)?;
        if let Some(lesson) = &source.lesson {
            tx.execute("INSERT OR IGNORE INTO memory_lesson_runs(day,run_id,fingerprint,details) VALUES (?1,?2,?3,?4)",
                params![date.to_string(),source.id,fingerprint,lesson.to_string()])?;
        }
        advance(&tx, source, fingerprint)?;
        tx.commit()?;
        Ok(Some((chunks.chunks.len(), outcome)))
    }

    async fn lessons(&self, models: &Models, date: NaiveDate) -> anyhow::Result<(usize, usize)> {
        let conn = self.graph.shared_conn();
        let details: Vec<Value> = {
            let db = conn.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT details FROM memory_lesson_runs WHERE day=?1 ORDER BY run_id,fingerprint",
            )?;
            let rows = stmt
                .query_map([date.to_string()], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows.iter()
                .map(|r| serde_json::from_str(r).map_err(Into::into))
                .collect::<anyhow::Result<_>>()?
        };
        // Evidence-based summary: never ask a model to invent why a run failed.
        // Earlier failed snapshots remain when the same run later succeeds.
        let description = format!(
            "Lessons learned for {date} (runs ingested on this local date).\n{}",
            if details.is_empty() {
                "No new delegated-run evidence was ingested.".into()
            } else {
                details.iter().map(|d| format!("{} on {}: run {}, state {}; reason: {}; retry/recovery evidence: {}; failure evidence: {}",
                d["agent"].as_str().unwrap_or("unknown"), d["device"].as_str().unwrap_or("unknown"),
                d["run_id"].as_str().unwrap_or("unknown"), d["state"].as_str().unwrap_or("unknown"),
                d["reason"], d["retries"], d["failures"])).collect::<Vec<_>>().join("\n")
            }
        );
        let source = Source {
            kind: "lessons",
            id: date.to_string(),
            project: "memory-lessons".into(),
            text: description.clone(),
            revision: 0,
            lesson: None,
        };
        let fingerprint = source.fingerprint(models, &self.config);
        if current(&conn.lock().unwrap(), &source, &fingerprint)? {
            return Ok((0, 0));
        }
        let chunks = models.rag.prepare(&description).await?;
        let mut db = conn.lock().unwrap();
        let tx = db.transaction()?;
        KnowledgeGraph::upsert_on(
            &tx,
            Some(&source.project),
            &Entity {
                id: source.rag_id(),
                kind: "lessons_learned".into(),
                name: format!("Lessons learned {date}"),
                attrs: json!({"date":date.to_string(),"description":description,"runs":details}),
            },
        )?;
        models
            .rag
            .replace_on(&tx, &source.project, &source.rag_id(), &chunks)?;
        advance(&tx, &source, &fingerprint)?;
        tx.commit()?;
        Ok((1, chunks.chunks.len()))
    }
}

fn schema(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS memory_ingest_state(id INTEGER PRIMARY KEY CHECK(id=1),watermark INTEGER NOT NULL DEFAULT 0);
        INSERT OR IGNORE INTO memory_ingest_state(id) VALUES (1);
        CREATE TABLE IF NOT EXISTS memory_ingested(kind TEXT NOT NULL,source_id TEXT NOT NULL,fingerprint TEXT NOT NULL,
            source_revision INTEGER NOT NULL,watermark INTEGER NOT NULL,ingested_at TEXT NOT NULL,PRIMARY KEY(kind,source_id));
        CREATE TABLE IF NOT EXISTS memory_nightly_attempts(day TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS memory_lesson_runs(day TEXT NOT NULL,run_id TEXT NOT NULL,fingerprint TEXT NOT NULL,details TEXT NOT NULL,
            PRIMARY KEY(day,run_id,fingerprint));")?;
    Ok(())
}

fn current(db: &Connection, source: &Source, fingerprint: &str) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM memory_ingested WHERE kind=?1 AND source_id=?2 AND fingerprint=?3)",
        params![source.kind,source.id,fingerprint], |r| r.get(0))?)
}

fn advance(db: &Connection, source: &Source, fingerprint: &str) -> anyhow::Result<()> {
    db.execute(
        "UPDATE memory_ingest_state SET watermark=watermark+1 WHERE id=1",
        [],
    )?;
    db.execute("INSERT INTO memory_ingested(kind,source_id,fingerprint,source_revision,watermark,ingested_at)
        VALUES (?1,?2,?3,?4,(SELECT watermark FROM memory_ingest_state WHERE id=1),strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        ON CONFLICT(kind,source_id) DO UPDATE SET fingerprint=excluded.fingerprint,source_revision=excluded.source_revision,
            watermark=excluded.watermark,ingested_at=excluded.ingested_at", params![source.kind,source.id,fingerprint,source.revision])?;
    Ok(())
}

fn has_table(db: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

fn sources(db: &Connection, conversation: Option<&str>) -> anyhow::Result<Vec<Source>> {
    let mut out = Vec::new();
    let ids = db
        .prepare("SELECT id FROM conversations WHERE ?1 IS NULL OR id=?1 ORDER BY started_at,id")?
        .query_map([conversation], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        if let Some(s) = load_source(db, "conversation", &id)? {
            out.push(s);
        }
    }
    if conversation.is_none() && has_table(db, "delegated_runs")? {
        let ids = db
            .prepare("SELECT id FROM delegated_runs ORDER BY id")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            if let Some(s) = load_source(db, "run", &id)? {
                out.push(s);
            }
        }
    }
    Ok(out)
}

fn load_source(db: &Connection, kind: &str, id: &str) -> anyhow::Result<Option<Source>> {
    if kind == "conversation" {
        if has_table(db, "web_chat_turns")? && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM web_chat_turns WHERE conversation_id=?1 AND status IN ('planning','executing','awaiting_approval'))", [id], |r| r.get::<_, bool>(0))? {
            return Ok(None);
        }
        let project: Option<String> = db
            .query_row(
                "SELECT project_id FROM conversations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(project) = project else {
            return Ok(None);
        };
        let mut stmt = db
            .prepare("SELECT id,role,content FROM messages WHERE conversation_id=?1 ORDER BY id")?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        return Ok(Some(Source {
            kind: "conversation",
            id: id.into(),
            project,
            revision: last.0,
            lesson: None,
            text: rows
                .iter()
                .map(|(_, role, text)| format!("{role}: {text}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }));
    }
    let row: Option<(String,String,String,String)> = db.query_row(
        "SELECT COALESCE(c.project_id,'delegated'),r.assignment,r.state,r.metadata FROM delegated_runs r LEFT JOIN conversations c ON c.id=r.conversation_id WHERE r.id=?1",
        [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((project, assignment, state, metadata)) = row else {
        return Ok(None);
    };
    let assignment: Value = serde_json::from_str(&assignment)?;
    let mut metadata: Value = serde_json::from_str(&metadata)?;
    // Heartbeats are not new transcript content, and must not cause nightly
    // re-extraction of every run that the coordinator continues to observe.
    if let Some(m) = metadata.as_object_mut() {
        for key in ["last_seen", "last_output_at", "updated_at", "heartbeat_at"] {
            m.remove(key);
        }
    }
    let mut stmt = db
        .prepare("SELECT seq,kind,payload FROM delegated_events WHERE run_id=?1 ORDER BY seq,id")?;
    let events = stmt
        .query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut text = format!(
        "Delegated run {id}\nAssignment: {assignment}\nState: {state}\nMetadata: {metadata}\n"
    );
    let mut failures = Vec::new();
    let mut retries = Vec::new();
    for (seq, kind, payload) in &events {
        text.push_str(&format!("{seq} {kind}: {payload}\n"));
        let value: Value = serde_json::from_str(payload)?;
        if matches!(
            kind.as_str(),
            "error" | "stalled" | "reconcile-required" | "quota-paused"
        ) || value["status"] == "failed"
            || value.get("error").is_some()
        {
            failures.push(value.clone());
        }
        if kind.contains("retry")
            || kind.contains("rework")
            || matches!(kind.as_str(), "resume-consumed" | "quota-resumed")
        {
            retries.push(value);
        }
    }
    for key in ["attempt", "attempts", "retries", "rework_count"] {
        if let Some(value) = metadata.get(key) {
            retries.push(json!({key:value}));
        }
    }
    let lesson = json!({"run_id":id,"agent":assignment["agent"],"device":assignment["device"],"state":state,
        "reason":metadata.get("reason").or_else(|| metadata.get("error")).unwrap_or(&Value::Null),"failures":failures,"retries":retries});
    Ok(Some(Source {
        kind: "run",
        id: id.into(),
        project,
        text,
        revision: events.last().map(|r| r.0).unwrap_or(0),
        lesson: Some(lesson),
    }))
}

#[cfg(test)]
mod tests;
