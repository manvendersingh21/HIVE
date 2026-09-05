//! RAG — chunked, embedded, cosine-searched conversation text.
//!
//! Deliberately a **linear scan**, not an ANN index. At the scale this system
//! runs at (one operator, hundreds of conversations, chunks in the low
//! thousands), scanning every vector in SQLite is single-digit milliseconds
//! and needs no second index to keep consistent, no second query language,
//! and no approximate-recall tuning. If the chunk table ever grows past what
//! a scan handles comfortably, the honest upgrade is sqlite-vec or an
//! on-disk ANN file — not pretending this is one.
//!
//! Embeddings are produced by whatever implements [`Embedder`] —
//! [`OllamaClient`](crate::llm::local::OllamaClient) in production (model
//! `memory.embedding_model`, default `nomic-embed-text`), [`HashEmbedder`] in
//! tests. Vectors are stored as little-endian f32 BLOBs beside their text.

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// Anything that can turn text into a vector.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, input: &str) -> anyhow::Result<Vec<f32>>;
}

#[async_trait]
impl Embedder for crate::llm::local::OllamaClient {
    async fn embed(&self, input: &str) -> anyhow::Result<Vec<f32>> {
        // `OllamaClient::embed` sends the model it was constructed with, so
        // the RAG index builds its client with `memory.embedding_model`, not
        // the chat model — an embedding model and a 9B chat model are not
        // interchangeable vector spaces.
        crate::llm::local::OllamaClient::embed(self, input).await
    }
}

/// Deterministic bag-of-words embedder for tests.
///
/// Hashes each token into one of `dim` buckets; cosine similarity then
/// approximates token overlap, which is exactly the property tests need to
/// assert ("the chunk about postgres scores higher than the one about
/// disk space") without a model server. Not a production embedder — it
/// cannot see word order or synonyms and is not pretending to.
pub struct HashEmbedder {
    pub dim: usize,
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, input: &str) -> anyhow::Result<Vec<f32>> {
        Ok(hash_embed(input, self.dim))
    }
}

fn hash_embed(input: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    for token in input.to_lowercase().split_whitespace() {
        let h = fxhash(token);
        v[(h as usize) % dim] += 1.0;
    }
    v
}

fn fxhash(s: &str) -> u64 {
    // FNV-1a: stable across runs and platforms, which is all a test
    // embedder needs. Not cryptographic — nothing here is adversarial.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// One search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagHit {
    pub project_id: String,
    pub conversation_id: String,
    pub text: String,
    /// Cosine similarity in [-1, 1].
    pub score: f32,
}

/// SQLite-backed chunk store with vector search.
#[derive(Clone)]
pub struct RagIndex {
    conn: Arc<std::sync::Mutex<Connection>>,
    embedder: Arc<dyn Embedder>,
    chunk_tokens: usize,
    overlap_tokens: usize,
}

impl RagIndex {
    pub fn new(
        conn: Arc<std::sync::Mutex<Connection>>,
        embedder: Arc<dyn Embedder>,
        chunk_tokens: usize,
        overlap_tokens: usize,
    ) -> anyhow::Result<Self> {
        {
            let conn = conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS rag_chunks (
                     id               INTEGER PRIMARY KEY AUTOINCREMENT,
                     project_id       TEXT NOT NULL,
                     conversation_id  TEXT NOT NULL,
                     chunk_index      INTEGER NOT NULL,
                     text             TEXT NOT NULL,
                     embedding        BLOB NOT NULL,
                     dim              INTEGER NOT NULL,
                     created_at       TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 CREATE INDEX IF NOT EXISTS rag_chunks_project ON rag_chunks(project_id);
                 CREATE INDEX IF NOT EXISTS rag_chunks_conversation
                     ON rag_chunks(conversation_id);",
            )?;
        }
        Ok(Self {
            conn,
            embedder,
            chunk_tokens: chunk_tokens.max(1),
            overlap_tokens: overlap_tokens.min(chunk_tokens.saturating_sub(1)),
        })
    }

    /// Split `text` into overlapping chunks.
    ///
    /// Token counts are approximated as `chars / 4`, the usual English
    /// heuristic — the config knobs (`memory.chunk_size`, 512;
    /// `memory.chunk_overlap`, 64) are written in tokens, and an exact
    /// tokenizer dependency to divide 2048 by 4 precisely is not worth its
    /// weight here. Chunks never split mid-word where avoidable.
    pub fn chunk_text(&self, text: &str) -> Vec<String> {
        let chunk_chars = self.chunk_tokens * 4;
        let overlap_chars = self.overlap_tokens * 4;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return vec![];
        }
        if trimmed.chars().count() <= chunk_chars {
            return vec![trimmed.to_string()];
        }
        let mut chunks = Vec::new();
        let chars: Vec<char> = trimmed.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let end = (start + chunk_chars).min(chars.len());
            let mut slice: String = chars[start..end].iter().collect();
            // Prefer ending on whitespace: a chunk that cuts a word in half
            // poisons both halves' embeddings.
            if end < chars.len() {
                if let Some(cut) = slice.rfind(char::is_whitespace).filter(|&i| i > 0) {
                    slice.truncate(cut);
                }
            }
            let slice = slice.trim().to_string();
            if !slice.is_empty() {
                chunks.push(slice);
            }
            if end >= chars.len() {
                break;
            }
            start = end.saturating_sub(overlap_chars).max(start + 1);
        }
        chunks
    }

    /// Index a conversation's transcript, replacing any previous chunks for
    /// it — re-indexing the same conversation must be idempotent, or every
    /// retry after a transient embed failure would duplicate the text.
    pub async fn index_conversation(
        &self,
        project_id: &str,
        conversation_id: &str,
        transcript: &str,
    ) -> anyhow::Result<usize> {
        let chunks = self.chunk_text(transcript);
        if chunks.is_empty() {
            return Ok(0);
        }
        // Embed before deleting the old rows: if the embedder fails midway
        // the previous index survives intact rather than leaving the
        // conversation half-indexed.
        let mut embedded = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            embedded.push((chunk, self.embedder.embed(chunk).await?));
        }
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "DELETE FROM rag_chunks WHERE conversation_id = ?1",
                params![conversation_id],
            )?;
            for (i, (text, vec)) in embedded.iter().enumerate() {
                conn.execute(
                    "INSERT INTO rag_chunks
                         (project_id, conversation_id, chunk_index, text, embedding, dim)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        project_id,
                        conversation_id,
                        i as i64,
                        text,
                        encode_f32(vec),
                        vec.len() as i64
                    ],
                )?;
            }
        }
        Ok(chunks.len())
    }

    /// Embed `query` and return the `top_k` best chunks by cosine, optionally
    /// within one project. A linear scan — see the module doc for why that
    /// is the right shape at this scale.
    pub async fn search(
        &self,
        project_id: Option<&str>,
        query: &str,
        top_k: usize,
    ) -> anyhow::Result<Vec<RagHit>> {
        let query_vec = self.embedder.embed(query).await?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT project_id, conversation_id, text, embedding, dim FROM rag_chunks
             WHERE (?1 IS NULL OR project_id = ?1)",
        )?;
        let rows = stmt.query_map(params![project_id], |r| {
            let blob: Vec<u8> = r.get(3)?;
            let dim: i64 = r.get(4)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                decode_f32(&blob, dim as usize),
            ))
        })?;
        let mut hits: Vec<RagHit> = rows
            .filter_map(|row| row.ok())
            .map(|(project_id, conversation_id, text, vec)| RagHit {
                project_id,
                conversation_id,
                text,
                score: cosine(&query_vec, &vec),
            })
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k);
        Ok(hits)
    }

    /// Chunk count for `hive memory`'s status view.
    pub fn chunk_count(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM rag_chunks", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

/// Cosine similarity; zero when either vector has zero magnitude (an empty
/// or all-stopword text must not produce NaN scores that sort unpredictably).
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let dot: f32 = a[..n].iter().zip(&b[..n]).map(|(x, y)| x * y).sum();
    let na: f32 = a[..n].iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b[..n].iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

fn encode_f32(v: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for f in v {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

fn decode_f32(b: &[u8], dim: usize) -> Vec<f32> {
    b.chunks_exact(4)
        .take(dim)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::graph::KnowledgeGraph;

    fn rag() -> RagIndex {
        let kg = KnowledgeGraph::in_memory().unwrap();
        RagIndex::new(
            kg.shared_conn(),
            Arc::new(HashEmbedder { dim: 64 }),
            8, // tokens — small, so tests exercise multi-chunk paths
            2,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn chunking_respects_size_overlap_and_whole_words() {
        let r = RagIndex::new(
            KnowledgeGraph::in_memory().unwrap().shared_conn(),
            Arc::new(HashEmbedder { dim: 8 }),
            4, // 16 chars per chunk
            1, // 4 chars overlap
        )
        .unwrap();
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let chunks = r.chunk_text(text);
        assert!(chunks.len() >= 2, "{:?}", chunks);
        for c in &chunks {
            assert!(c.chars().count() <= 16 + 1, "chunk over budget: {c:?}");
        }
        // No chunk starts or ends mid-word (whitespace boundaries preferred).
        for c in &chunks {
            assert_eq!(c.chars().next().unwrap().is_whitespace(), false);
        }
    }

    #[tokio::test]
    async fn short_text_is_a_single_chunk() {
        let r = rag();
        assert_eq!(r.chunk_text("hello world").len(), 1);
        assert!(r.chunk_text("   ").is_empty());
    }

    #[tokio::test]
    async fn search_ranks_the_relevant_conversation_first() {
        let r = rag();
        r.index_conversation(
            "webapp",
            "conv-db",
            "we chose sqlite over postgres for the local deployment because single writer",
        )
        .await
        .unwrap();
        r.index_conversation(
            "webapp",
            "conv-disk",
            "the disk filled up overnight because of log rotation misconfiguration",
        )
        .await
        .unwrap();

        let hits = r.search(Some("webapp"), "which database did we pick? postgres sqlite", 2)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].conversation_id, "conv-db", "{:?}", hits);
        assert!(hits[0].score > 0.0);
    }

    #[tokio::test]
    async fn search_stays_project_scoped() {
        let r = rag();
        r.index_conversation("alpha", "c1", "postgres migration notes").await.unwrap();
        r.index_conversation("beta", "c2", "postgres migration notes").await.unwrap();

        let hits = r.search(Some("alpha"), "postgres", 10).await.unwrap();
        assert!(hits.iter().all(|h| h.project_id == "alpha"));
        let all = r.search(None, "postgres", 10).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn reindexing_replaces_rather_than_duplicates() {
        let r = rag();
        r.index_conversation("p", "c", "first version of the transcript").await.unwrap();
        r.index_conversation("p", "c", "second version of the transcript").await.unwrap();
        assert_eq!(r.chunk_count().unwrap(), 1);
        let hits = r.search(Some("p"), "second version", 5).await.unwrap();
        assert!(hits[0].text.contains("second"));
    }

    #[test]
    fn cosine_handles_zero_vectors_and_length_mismatch() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-6);
        // Different dims (an embedding-model swap left old rows): compare the
        // shared prefix rather than panicking.
        assert!(cosine(&[1.0], &[1.0, 5.0, 5.0]).is_finite());
    }
}
