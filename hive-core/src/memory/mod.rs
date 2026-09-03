//! Memory system — project-scoped conversation history, knowledge graph, and RAG.

pub mod graph;
pub mod machines;

use std::path::Path;

use graph::KnowledgeGraph;

/// The unified memory system.
///
/// The knowledge graph is live; RAG, the project registry, and conversation
/// history are still Phase 9.
pub struct MemorySystem {
    /// Persistent entity/relation graph. Currently holds the machine fleet
    /// (see [`machines`]); projects and conversations land here in Phase 9.
    pub graph: KnowledgeGraph,
    // TODO: RagIndex, ProjectRegistry
}

impl MemorySystem {
    /// Open the memory system backed by a database at `path`.
    ///
    /// A database that cannot be opened degrades to an in-memory graph with a
    /// warning rather than failing startup — losing memory should not stop the
    /// agent from answering.
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let graph = KnowledgeGraph::open(path).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "could not open knowledge graph on disk, using in-memory");
            KnowledgeGraph::in_memory().expect("in-memory SQLite is always available")
        });
        Self { graph }
    }

    /// Create a new memory system with an ephemeral graph.
    pub fn new() -> Self {
        Self {
            graph: KnowledgeGraph::in_memory().expect("in-memory SQLite is always available"),
        }
    }

    /// Retrieve relevant context for a user message within a project.
    pub async fn retrieve_context(
        &self,
        _project_id: &str,
        _user_input: &str,
    ) -> anyhow::Result<RetrievedContext> {
        // TODO: query KG + RAG + recent messages
        Ok(RetrievedContext {
            rag_chunks: vec![],
            kg_entities: vec![],
            recent_messages: vec![],
        })
    }
}

/// Context retrieved from memory for injection into LLM prompts.
#[derive(Debug, Clone)]
pub struct RetrievedContext {
    /// Relevant conversation chunks from RAG.
    pub rag_chunks: Vec<String>,
    /// Related knowledge graph entities.
    pub kg_entities: Vec<String>,
    /// Recent messages from the project.
    pub recent_messages: Vec<String>,
}
