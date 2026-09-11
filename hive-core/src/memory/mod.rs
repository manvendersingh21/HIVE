//! Memory system — project-scoped conversation history, knowledge graph, and RAG.
//!
//! One database, four stores: the knowledge graph (`graph.rs`), the project /
//! conversation / message registry (`projects.rs`), the RAG chunk index
//! (`rag.rs`), and entity embeddings for dedup (`extractor.rs`). Everything
//! shares one connection and one WAL so a turn's writes are ordered and the
//! file keeps the incident log's 0600 lock.
//!
//! Retrieval (`retrieve_context`) is three-legged: recent messages for
//! continuity, knowledge-graph entities for distilled facts, RAG chunks for
//! verbatim recall — capped together at `memory.max_context_tokens`.

pub mod chats;
pub mod extractor;
pub mod graph;
pub mod machines;
pub mod projects;
pub mod rag;

use std::path::Path;
use std::sync::Arc;

use graph::KnowledgeGraph;
use hive_common::config::HiveConfig;
use projects::ProjectRegistry;
use rag::{Embedder, RagIndex};

use crate::llm::local::OllamaClient;

/// Words too common to search the knowledge graph with.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "then", "for", "to", "of", "in", "on", "at", "by",
    "with", "from", "is", "are", "was", "were", "be", "been", "it", "its", "this", "that", "these",
    "those", "i", "you", "we", "they", "my", "your", "our", "their", "me", "him", "her", "them",
    "what", "which", "who", "when", "where", "why", "how", "do", "does", "did", "can", "could",
    "should", "would", "will", "shall", "not", "no", "yes", "about", "into", "over", "than",
    "also", "just", "some", "any", "all", "get", "got", "make", "made", "use", "used", "using",
];

/// The unified memory system.
#[derive(Clone)]
pub struct MemorySystem {
    /// Persistent entity/relation graph. The machine fleet lives in its
    /// global scope (see [`machines`]); conversation knowledge is
    /// project-scoped.
    pub graph: KnowledgeGraph,
    /// Project / conversation / message store.
    pub projects: ProjectRegistry,
    /// Embedded chunk index.
    pub rag: RagIndex,
    /// The embedding-model client (`memory.embedding_model`). Shared by the
    /// RAG index and the extractor's dedup pass — one vector space, because
    /// dedup compares candidate entity embeddings against stored ones.
    embed_client: Arc<dyn Embedder>,
    /// Master provider used for low-effort knowledge extraction.
    extract_llm: Option<Arc<dyn extractor::Completer>>,
    config: hive_common::config::MemoryConfig,
}

/// A conversation turn opened by [`MemorySystem::begin_turn`].
#[derive(Debug, Clone)]
pub struct Turn {
    pub conversation_id: String,
    pub context: RetrievedContext,
}

impl MemorySystem {
    /// Open the memory system backed by a database at `path`.
    ///
    /// A database that cannot be opened degrades to an in-memory graph with a
    /// warning rather than failing startup — losing memory should not stop the
    /// agent from answering.
    pub fn open(path: impl AsRef<Path>, config: &HiveConfig) -> Self {
        let path = path.as_ref();
        let graph = KnowledgeGraph::open(path).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "could not open knowledge graph on disk, using in-memory");
            KnowledgeGraph::in_memory().expect("in-memory SQLite is always available")
        });
        Self::from_parts(graph, Some(config), path)
    }

    /// Migration must fail if its durable database cannot be opened.
    pub fn open_for_reindex(path: impl AsRef<Path>, config: &HiveConfig) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let graph = KnowledgeGraph::open(path)?;
        Ok(Self::from_parts(graph, Some(config), path))
    }

    /// Create a new memory system with an ephemeral graph.
    ///
    /// Tests and throwaway agents: nothing persists, embeddings are the
    /// deterministic test embedder, and knowledge extraction is disabled.
    /// Production callers use [`MemorySystem::open`] — an agent built on
    /// `new()` forgets everything when it exits.
    pub fn new() -> Self {
        let graph = KnowledgeGraph::in_memory().expect("in-memory SQLite is always available");
        Self::from_parts(graph, None, Path::new(":memory:"))
    }

    fn from_parts(graph: KnowledgeGraph, config: Option<&HiveConfig>, path: &Path) -> Self {
        let conn = graph.shared_conn();
        let memory_config = config.map(|c| c.memory.clone()).unwrap_or_default();
        let projects = ProjectRegistry::new(conn.clone())
            .unwrap_or_else(|e| panic!("cannot create project tables in {}: {e}", path.display()));
        let (embed_client, extract_llm): (
            Arc<dyn Embedder>,
            Option<Arc<dyn extractor::Completer>>,
        ) = match config {
            Some(c) => {
                let embed: Arc<dyn Embedder> = if c.memory.embedding_provider
                    == Some(hive_common::config::EmbeddingProvider::Nvidia)
                {
                    let mut cfg = c.llm.nvidia.clone();
                    cfg.model = c.memory.embedding_model.clone();
                    Arc::new(crate::llm::nvidia::NvidiaClient::for_embeddings(&cfg))
                } else {
                    Arc::new(OllamaClient::new(
                        c.llm.local.base_url.clone(),
                        c.memory.embedding_model.clone(),
                    ))
                };
                (
                    embed,
                    Some(Arc::new(crate::llm::LlmRouter::from_config(&c.llm))),
                )
            }
            None => (Arc::new(rag::HashEmbedder { dim: 128 }), None),
        };
        let rag = RagIndex::new(
            conn.clone(),
            embed_client.clone(),
            memory_config.chunk_size as usize,
            memory_config.chunk_overlap as usize,
        )
        .unwrap_or_else(|e| panic!("cannot create rag tables in {}: {e}", path.display()));
        Self {
            graph,
            projects,
            rag,
            embed_client,
            extract_llm,
            config: memory_config,
        }
    }

    /// Open a turn: persist the user's message and fetch the context worth
    /// injecting into this request's prompts.
    ///
    /// Failures here must not fail the request — a broken memory is a
    /// degraded agent, not a dead one — so DB errors are logged and the turn
    /// proceeds with empty context (and no persistence).
    pub async fn begin_turn(&self, project_id: &str, user_input: &str) -> Turn {
        let context = self.retrieve_context(project_id, user_input).await;
        let conversation = (|| -> anyhow::Result<String> {
            let conv = self.projects.begin_conversation(project_id, user_input)?;
            self.projects.append_message(&conv.id, "user", user_input)?;
            Ok(conv.id)
        })();
        match conversation {
            Ok(id) => Turn {
                conversation_id: id,
                context,
            },
            Err(e) => {
                tracing::warn!(error = %e, "could not persist the incoming turn; continuing without");
                Turn {
                    conversation_id: String::new(),
                    context,
                }
            }
        }
    }

    /// Close a turn: persist the assistant's answer, then — when
    /// `memory.auto_index` allows — re-index the conversation into RAG and
    /// extract knowledge into the graph. Indexing and extraction are
    /// best-effort with logs; a turn never fails because memory did.
    pub async fn complete_turn(&self, conversation_id: &str, assistant_text: &str) {
        if conversation_id.is_empty() {
            return;
        }
        if let Err(e) = self
            .projects
            .append_message(conversation_id, "assistant", assistant_text)
        {
            tracing::warn!(error = %e, "could not persist the assistant's reply");
            return;
        }
        if !self.config.auto_index {
            return;
        }
        let Ok(messages) = self.projects.conversation_messages(conversation_id) else {
            return;
        };
        let Some(project_id) = messages.first().map(|m| m.project_id.clone()) else {
            return;
        };
        let transcript: String = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        match self
            .rag
            .index_conversation(&project_id, conversation_id, &transcript)
            .await
        {
            Ok(n) => tracing::debug!(chunks = n, "conversation indexed into rag"),
            Err(e) => tracing::warn!(error = %e, "rag indexing skipped"),
        }

        let Some(llm) = self.extract_llm.clone() else {
            return;
        };
        let outcome = extractor::extract_and_store(
            &self.graph,
            &self.graph.shared_conn(),
            self.embed_client.as_ref(),
            llm.as_ref(),
            self.config.knowledge_graph.max_entities_per_conversation as usize,
            self.config.knowledge_graph.entity_dedup_threshold,
            &project_id,
            &transcript,
        )
        .await;
        match outcome {
            Ok(o) if o.entities_created > 0 || o.relations_added > 0 => tracing::info!(
                created = o.entities_created,
                merged = o.entities_merged,
                relations = o.relations_added,
                "knowledge extracted into graph"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "knowledge extraction failed"),
        }
    }

    /// Retrieve relevant context for a user message within a project:
    /// recent messages, matching knowledge-graph entities, and the best
    /// RAG chunks, capped together at `memory.max_context_tokens`
    /// (approximated as chars/4).
    pub async fn retrieve_context(&self, project_id: &str, user_input: &str) -> RetrievedContext {
        let mut out = RetrievedContext {
            rag_chunks: vec![],
            kg_entities: vec![],
            recent_messages: vec![],
        };
        if self.projects.project(project_id).ok().flatten().is_none() {
            // Unknown project: nothing has been recorded under it yet.
            return out;
        }

        // Recent messages: continuity first, they are the cheapest signal.
        if let Ok(recent) = self.projects.recent_messages(project_id, 8) {
            out.recent_messages = recent
                .iter()
                .map(|m| {
                    let content: String = ellipsize(&m.content, 400);
                    format!("{}: {content}", m.role)
                })
                .collect();
        }

        // Knowledge graph: keyword match on the input's meaningful tokens.
        let terms: Vec<String> = user_input
            .split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
            .collect();
        let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        if let Ok(entities) = self.graph.search_entities(project_id, &term_refs, 8) {
            out.kg_entities = entities
                .iter()
                .map(|e| {
                    let desc = e.attr_str("description").unwrap_or("");
                    if desc.is_empty() {
                        format!("{} ({})", e.name, e.kind)
                    } else {
                        format!("{} ({}): {}", e.name, e.kind, desc)
                    }
                })
                .collect();
        }

        // RAG: cosine over the embedded chunks.
        if !user_input.trim().is_empty() {
            match self.rag.search(Some(project_id), user_input, 3).await {
                Ok(hits) => {
                    out.rag_chunks = hits
                        .into_iter()
                        .filter(|h| h.score > 0.15)
                        .map(|h| ellipsize(&h.text, 600))
                        .collect()
                }
                Err(e) => tracing::debug!(error = %e, "rag search unavailable"),
            }
        }

        apply_budget(&mut out, self.config.max_context_tokens);
        out
    }

    /// Cross-store free-text search for `hive search`: message hits, RAG
    /// hits, and matching graph entities across every project.
    pub async fn search_all(&self, query: &str, project_id: Option<&str>) -> SearchResults {
        let messages = self
            .projects
            .search_messages(query, project_id, 20)
            .unwrap_or_default();
        let (rag, semantic_error) = match self.rag.search(project_id, query, 10).await {
            Ok(hits) => (hits, None),
            Err(e) => (vec![], Some(format!("Semantic retrieval unavailable: {e}; history and keyword search remain available"))),
        };
        let mut entities = Vec::new();
        let projects: Vec<String> = match project_id {
            Some(p) => vec![p.to_string()],
            None => self
                .projects
                .list_projects()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.id)
                .collect(),
        };
        for p in &projects {
            if let Ok(found) = self.graph.search_entities(p, &[query], 10) {
                entities.extend(found);
            }
        }
        SearchResults {
            semantic_error,
            messages,
            rag,
            entities: entities.into_iter().map(|e| e.name).collect(),
        }
    }

    /// Enumerate durable source records, including ones whose first embedding failed.
    fn conversation_sources(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let conn = self.graph.shared_conn();
        let rows: Vec<(String, String)> = {
            let db = conn.lock().unwrap();
            let mut stmt =
                db.prepare("SELECT id, project_id FROM conversations ORDER BY started_at, id")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            rows
        };
        rows.into_iter()
            .map(|(id, project)| {
                let text = self
                    .projects
                    .conversation_messages(&id)?
                    .iter()
                    .map(|m| format!("{}: {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok((id, project, text))
            })
            .collect()
    }

    fn entity_sources(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let conn = self.graph.shared_conn();
        extractor::create_table(&conn)?;
        let db = conn.lock().unwrap();
        // Fleet facts have NULL project_id and are consumed directly by the
        // planner. Only project knowledge participates in semantic dedup/search.
        let mut stmt = db.prepare(
            "SELECT id, project_id, name, attrs FROM entities WHERE project_id IS NOT NULL ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let attrs: String = r.get(3)?;
                let attrs: serde_json::Value = serde_json::from_str(&attrs).unwrap_or_default();
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    format!(
                        "{}: {}",
                        r.get::<_, String>(2)?,
                        attrs["description"].as_str().unwrap_or("")
                    ),
                ))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    fn entity_current(&self, id: &str, text: &str) -> anyhow::Result<bool> {
        let conn = self.graph.shared_conn();
        let db = conn.lock().unwrap();
        Ok(db.query_row("SELECT COUNT(*) FROM kg_embeddings WHERE entity_id = ?1 AND provider = ?2 AND model = ?3 AND source = ?4", rusqlite::params![id, self.embed_client.provider(), self.embed_client.model(), text], |r| r.get::<_, i64>(0))? > 0)
    }

    /// Resume from committed conversations/entities. Each replacement is atomic;
    /// failed requests leave the old vectors and all source records intact.
    pub async fn reindex(&self) -> anyhow::Result<ReindexStatus> {
        let mut status = ReindexStatus::default();
        for (id, project, text) in self.conversation_sources()? {
            if text.trim().is_empty() || self.rag.is_current(&id, &text)? {
                continue;
            }
            match self.rag.index_conversation(&project, &id, &text).await {
                Ok(_) => status.rebuilt += 1,
                Err(e) => {
                    status.failed += 1;
                    tracing::warn!(conversation = %id, error = %e, "semantic indexing incomplete; rerun hive memory reindex");
                }
            }
        }
        for (id, project, text) in self.entity_sources()? {
            if self.entity_current(&id, &text)? {
                continue;
            }
            let result = async {
                let vec = self.embed_client.embed(&text).await?;
                extractor::store_embedding(
                    &self.graph.shared_conn(),
                    &id,
                    &project,
                    &vec,
                    self.embed_client.as_ref(),
                    &text,
                )
            }
            .await;
            match result {
                Ok(()) => status.rebuilt += 1,
                Err(e) => {
                    status.failed += 1;
                    tracing::warn!(entity = %id, error = %e, "entity indexing incomplete; rerun hive memory reindex");
                }
            }
        }
        Ok(status)
    }

    pub fn semantic_status(&self) -> anyhow::Result<String> {
        let mut missing = 0;
        for (id, _, text) in self.conversation_sources()? {
            if !text.trim().is_empty() && !self.rag.is_current(&id, &text)? {
                missing += 1;
            }
        }
        for (id, _, text) in self.entity_sources()? {
            if !self.entity_current(&id, &text)? {
                missing += 1;
            }
        }
        Ok(format!(
            "{}/{}: {} records need indexing{}",
            self.embed_client.provider(),
            self.embed_client.model(),
            missing,
            if missing > 0 {
                " (incomplete; run hive memory reindex)"
            } else {
                ""
            }
        ))
    }

    /// Status for `hive memory`: what is in the store right now.
    pub fn status(&self) -> MemoryStatus {
        let (projects, conversations, messages) = self.projects.counts().unwrap_or((0, 0, 0));
        MemoryStatus {
            projects,
            conversations,
            messages,
            rag_chunks: self.rag.chunk_count().unwrap_or(0),
            graph_entities: self.graph.snapshot().map(|s| s.entities.len()).unwrap_or(0),
        }
    }
}

#[derive(Debug, Default)]
pub struct ReindexStatus {
    pub rebuilt: usize,
    pub failed: usize,
}

impl Default for MemorySystem {
    fn default() -> Self {
        Self::new()
    }
}

/// A coarse token budget over the three retrieved sets. Order of
/// preservation when the budget bites: recent messages, then entities,
/// then chunks — verbatim recall is the last thing worth spending tokens
/// on when the distilled facts already fit.
fn apply_budget(ctx: &mut RetrievedContext, max_tokens: u32) {
    let budget = (max_tokens as usize).saturating_mul(4);
    let mut used = 0;
    let mut take = |v: &mut Vec<String>| {
        let mut kept = Vec::with_capacity(v.len());
        for s in v.drain(..) {
            let cost = s.len() + 1;
            if used + cost > budget {
                break;
            }
            used += cost;
            kept.push(s);
        }
        *v = kept;
    };
    take(&mut ctx.recent_messages);
    take(&mut ctx.kg_entities);
    take(&mut ctx.rag_chunks);
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Context retrieved from memory for injection into LLM prompts.
#[derive(Debug, Clone, Default)]
pub struct RetrievedContext {
    /// Relevant conversation chunks from RAG.
    pub rag_chunks: Vec<String>,
    /// Related knowledge graph entities.
    pub kg_entities: Vec<String>,
    /// Recent messages from the project.
    pub recent_messages: Vec<String>,
}

impl RetrievedContext {
    pub fn is_empty(&self) -> bool {
        self.rag_chunks.is_empty() && self.kg_entities.is_empty() && self.recent_messages.is_empty()
    }

    /// Render as the block injected into planner prompts.
    ///
    /// Framed as background context, never as instructions: retrieved text
    /// is prior conversation, which is untrusted input — a prompt-injection
    /// wall between memory and instructions is cheaper than the incident.
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        if !self.recent_messages.is_empty() {
            parts.push(format!(
                "Recent conversation:\n{}",
                self.recent_messages.join("\n")
            ));
        }
        if !self.kg_entities.is_empty() {
            parts.push(format!(
                "Known facts:\n{}",
                self.kg_entities
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !self.rag_chunks.is_empty() {
            parts.push(format!(
                "Relevant earlier notes:\n{}",
                self.rag_chunks
                    .iter()
                    .map(|s| format!("- {}", ellipsize(s, 400)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        parts.join("\n\n")
    }
}

/// Results of `hive search`.
#[derive(Debug, Clone)]
pub struct SearchResults {
    pub semantic_error: Option<String>,
    pub messages: Vec<projects::MessageHit>,
    pub rag: Vec<rag::RagHit>,
    pub entities: Vec<String>,
}

/// Counts for `hive memory`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryStatus {
    pub projects: usize,
    pub conversations: usize,
    pub messages: usize,
    pub rag_chunks: usize,
    pub graph_entities: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_with_history() -> MemorySystem {
        let mem = MemorySystem::new();
        let t1 = mem
            .begin_turn("webapp", "we chose sqlite over postgres for local deploys")
            .await;
        mem.complete_turn(&t1.conversation_id, "recorded: sqlite chosen")
            .await;
        let t2 = mem
            .begin_turn("webapp", "the nightly cron job failed at 2am")
            .await;
        mem.complete_turn(&t2.conversation_id, "fixed the cron schedule")
            .await;
        mem
    }

    #[tokio::test]
    async fn reindex_preserves_unscoped_fleet_and_indexes_project_knowledge() {
        let mem = MemorySystem::new();
        let machine = graph::Entity::new("machine", "worker", serde_json::json!({}));
        let tool = graph::Entity::new("tool", "cargo", serde_json::json!({}));
        mem.graph.upsert_entity(&machine).unwrap();
        mem.graph.upsert_entity(&tool).unwrap();
        mem.graph
            .add_edge(&machine.id, "has_tool", &tool.id)
            .unwrap();
        let before = mem.graph.snapshot().unwrap();
        let conv = mem.projects.begin_conversation("p", "database").unwrap();
        mem.projects
            .append_message(&conv.id, "user", "use sqlite")
            .unwrap();
        let entity = graph::Entity::new("tool", "sqlite", serde_json::json!({}));
        mem.graph.upsert_entity_scoped("p", &entity).unwrap();
        assert!(mem
            .semantic_status()
            .unwrap()
            .contains("2 records need indexing"));
        assert_eq!(mem.reindex().await.unwrap().rebuilt, 2);
        assert!(mem
            .semantic_status()
            .unwrap()
            .contains("0 records need indexing"));
        assert_eq!(mem.reindex().await.unwrap().rebuilt, 0);
        let after = mem.graph.snapshot().unwrap();
        assert_eq!(after.entities, before.entities);
        assert_eq!(after.edges, before.edges);
        assert_eq!(mem.projects.counts().unwrap(), (1, 1, 1));
        let conn = mem.graph.shared_conn();
        let count: i64 = conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM kg_embeddings WHERE entity_id = ?1 OR entity_id = ?2",
                rusqlite::params![machine.id, tool.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    struct ControlledEmbedder {
        model: &'static str,
        remaining: std::sync::atomic::AtomicIsize,
    }
    #[async_trait::async_trait]
    impl Embedder for ControlledEmbedder {
        fn provider(&self) -> &str {
            "test"
        }
        async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            use std::sync::atomic::Ordering;
            if self.remaining.fetch_sub(1, Ordering::SeqCst) <= 0 {
                anyhow::bail!("interrupted embedding service");
            }
            rag::HashEmbedder { dim: 16 }.embed(text).await
        }
        fn model(&self) -> &str {
            self.model
        }
    }
    fn replace_embedder(mem: &mut MemorySystem, model: &'static str, remaining: isize) {
        let embedder = Arc::new(ControlledEmbedder {
            model,
            remaining: std::sync::atomic::AtomicIsize::new(remaining),
        });
        mem.rag = RagIndex::new(mem.graph.shared_conn(), embedder.clone(), 512, 64).unwrap();
        mem.embed_client = embedder;
    }

    #[tokio::test]
    async fn legacy_vector_schema_migrates_without_comparing_unknown_spaces() {
        let graph = KnowledgeGraph::in_memory().unwrap();
        let conn = graph.shared_conn();
        conn.lock().unwrap().execute_batch("CREATE TABLE rag_chunks (id INTEGER PRIMARY KEY, project_id TEXT, conversation_id TEXT, chunk_index INTEGER, text TEXT, embedding BLOB, dim INTEGER); CREATE TABLE kg_embeddings (entity_id TEXT PRIMARY KEY, project_id TEXT, embedding BLOB, dim INTEGER);").unwrap();
        let mem = MemorySystem::from_parts(graph, None, Path::new(":memory:"));
        mem.projects.ensure_project("p", None).unwrap();
        let conv = mem.projects.begin_conversation("p", "legacy").unwrap();
        mem.projects
            .append_message(&conv.id, "user", "postgres decision")
            .unwrap();
        conn.lock().unwrap().execute("INSERT INTO rag_chunks (project_id, conversation_id, chunk_index, text, embedding, dim) VALUES ('p', ?1, 0, 'legacy postgres', ?2, 128)", rusqlite::params![conv.id, vec![0u8;512]]).unwrap();
        assert!(mem
            .rag
            .search(Some("p"), "postgres", 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(mem.rag.chunk_count().unwrap(), 1);
        assert_eq!(mem.reindex().await.unwrap().rebuilt, 1);
        assert_eq!(
            mem.rag
                .search(Some("p"), "postgres", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(mem.projects.counts().unwrap(), (1, 1, 1));
    }

    #[tokio::test]
    async fn interrupted_reindex_resumes_preserving_history_facts_and_old_vectors() {
        let mut mem = MemorySystem::new();
        mem.projects.ensure_project("p", None).unwrap();
        for text in ["postgres decision", "sqlite decision"] {
            let c = mem.projects.begin_conversation("p", text).unwrap();
            mem.projects.append_message(&c.id, "user", text).unwrap();
        }
        let a = graph::Entity::new(
            "tool",
            "postgres",
            serde_json::json!({"description":"database"}),
        );
        let b = graph::Entity::new(
            "tool",
            "sqlite",
            serde_json::json!({"description":"database"}),
        );
        mem.graph.upsert_entity_scoped("p", &a).unwrap();
        mem.graph.upsert_entity_scoped("p", &b).unwrap();
        mem.graph.add_edge(&a.id, "replaces", &b.id).unwrap();
        assert_eq!(mem.reindex().await.unwrap().rebuilt, 4);
        let old_count = mem.rag.chunk_count().unwrap();
        replace_embedder(&mut mem, "new-space", 1);
        let partial = mem.reindex().await.unwrap();
        assert_eq!(partial.rebuilt, 1);
        assert_eq!(partial.failed, 3);
        assert_eq!(mem.rag.chunk_count().unwrap(), old_count);
        assert!(mem
            .semantic_status()
            .unwrap()
            .contains("3 records need indexing"));
        let results = mem.search_all("postgres", Some("p")).await;
        assert!(!results.messages.is_empty());
        assert!(results.semantic_error.is_some());
        replace_embedder(&mut mem, "new-space", 10);
        assert_eq!(mem.reindex().await.unwrap().rebuilt, 3);
        assert_eq!(mem.reindex().await.unwrap().rebuilt, 0);
        assert!(mem
            .semantic_status()
            .unwrap()
            .contains("0 records need indexing"));
        assert_eq!(mem.projects.counts().unwrap(), (1, 2, 2));
        assert_eq!(mem.graph.edges_from(&a.id).unwrap().len(), 1);
        assert_eq!(
            mem.rag
                .search(Some("p"), "postgres", 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn embedding_spaces_are_isolated_even_with_equal_dimensions() {
        let mut mem = MemorySystem::new();
        replace_embedder(&mut mem, "old-space", 10);
        mem.rag
            .index_conversation("p", "c", "postgres")
            .await
            .unwrap();
        replace_embedder(&mut mem, "new-space", 10);
        assert!(mem
            .rag
            .search(Some("p"), "postgres", 10)
            .await
            .unwrap()
            .is_empty());
        let conn = mem.graph.shared_conn();
        conn.lock()
            .unwrap()
            .execute(
                "UPDATE rag_chunks SET model = 'new-space', provider = 'another-provider'",
                [],
            )
            .unwrap();
        assert!(mem
            .rag
            .search(Some("p"), "postgres", 10)
            .await
            .unwrap()
            .is_empty());
        conn.lock()
            .unwrap()
            .execute("UPDATE rag_chunks SET provider = 'test', dim = 8", [])
            .unwrap();
        assert!(mem
            .rag
            .search(Some("p"), "postgres", 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn sql_failure_rolls_back_entire_conversation_replacement() {
        let mem = MemorySystem::new();
        mem.rag
            .index_conversation("p", "c", "old transcript")
            .await
            .unwrap();
        let conn = mem.graph.shared_conn();
        conn.lock().unwrap().execute_batch("CREATE TRIGGER reject_second_chunk BEFORE INSERT ON rag_chunks WHEN NEW.chunk_index = 1 BEGIN SELECT RAISE(ABORT, 'disk failure'); END;").unwrap();
        let long = "replacement ".repeat(1000);
        assert!(mem.rag.index_conversation("p", "c", &long).await.is_err());
        assert_eq!(mem.rag.chunk_count().unwrap(), 1);
        assert!(mem.rag.is_current("c", "old transcript").unwrap());
        assert!(!mem.rag.is_current("c", &long).unwrap());
        assert_eq!(
            mem.rag.search(None, "old transcript", 1).await.unwrap()[0].text,
            "old transcript"
        );
    }

    #[tokio::test]
    async fn retrieve_context_returns_recent_messages_and_rag_hits() {
        let mem = memory_with_history().await;
        let ctx = mem
            .retrieve_context("webapp", "which database did we pick? sqlite postgres")
            .await;
        assert!(
            !ctx.recent_messages.is_empty(),
            "recent messages: {:?}",
            ctx.recent_messages
        );
        assert!(
            ctx.rag_chunks.iter().any(|c| c.contains("postgres")),
            "rag should surface the db decision: {:?}",
            ctx.rag_chunks
        );
    }

    #[tokio::test]
    async fn retrieve_context_is_project_scoped() {
        let mem = memory_with_history().await;
        let ctx = mem
            .retrieve_context("other-project", "database sqlite postgres")
            .await;
        assert!(
            ctx.is_empty(),
            "another project must not see webapp's memory"
        );
    }

    #[tokio::test]
    async fn unknown_project_retrieves_nothing() {
        let mem = MemorySystem::new();
        let ctx = mem.retrieve_context("ghost", "anything").await;
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn begin_and_complete_turn_persist_both_sides() {
        let mem = MemorySystem::new();
        let turn = mem.begin_turn("p", "question one").await;
        mem.complete_turn(&turn.conversation_id, "answer one").await;
        let recent = mem.projects.recent_messages("p", 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].role, "user");
        assert_eq!(recent[1].role, "assistant");
        assert_eq!(recent[1].content, "answer one");
    }

    #[tokio::test]
    async fn budget_caps_the_rendered_context() {
        let mut ctx = RetrievedContext {
            recent_messages: vec!["a".repeat(199)],
            kg_entities: vec!["b".repeat(10)],
            rag_chunks: vec!["c".repeat(10)],
        };
        apply_budget(&mut ctx, 50); // 200 chars
                                    // The recent message (199 + separator) alone exhausts the budget.
        assert_eq!(ctx.recent_messages.len(), 1);
        assert!(ctx.kg_entities.is_empty());
        assert!(ctx.rag_chunks.is_empty());
    }

    #[test]
    fn render_labels_each_section() {
        let ctx = RetrievedContext {
            recent_messages: vec!["user: hi".into()],
            kg_entities: vec!["postgres (tool): chosen".into()],
            rag_chunks: vec!["chunk text".into()],
        };
        let rendered = ctx.render();
        assert!(rendered.contains("Recent conversation:"));
        assert!(rendered.contains("Known facts:"));
        assert!(rendered.contains("Relevant earlier notes:"));
    }

    #[test]
    fn search_terms_filter_stopwords() {
        // exercised indirectly through retrieve_context; kept here as a
        // reminder that the STOPWORDS list is load-bearing for KG matching
        assert!(STOPWORDS.contains(&"the"));
    }
}
