//! Memory system — project-scoped conversation history, knowledge graph, and RAG.

/// The unified memory system.
pub struct MemorySystem {
    // TODO: KnowledgeGraph, RagIndex, ProjectRegistry, SqlitePool
}

impl MemorySystem {
    /// Create a new (empty) memory system.
    pub fn new() -> Self {
        Self {}
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
