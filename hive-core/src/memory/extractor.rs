//! Knowledge extraction — turn a transcript into graph entities and edges.
//!
//! Uses the configured master provider for extraction and passage embeddings for dedup.
//!
//! Every failure here is soft: an unreachable model, unparseable JSON, or a
//! failed embedding degrades to "nothing extracted this time" and the turn
//! still completes. Memory is an accelerator, never a dependency.

use std::sync::Arc;

use rusqlite::{params, Connection};

use crate::memory::graph::{scoped_entity_id, Entity, KnowledgeGraph};
use crate::memory::rag::{cosine, Embedder};

/// How the extraction call is made — satisfied by
/// [`OllamaClient`](crate::llm::local::OllamaClient) in production and by
/// closures in tests. Keeping it a one-method trait means the extractor can
/// be tested without a model server and without depending on the router.
#[async_trait::async_trait]
pub trait Completer: Send + Sync {
    async fn complete(&self, prompt: &str) -> anyhow::Result<String>;
}

#[async_trait::async_trait]
impl Completer for crate::llm::local::OllamaClient {
    async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        self.complete_raw(prompt).await
    }
}

#[async_trait::async_trait]
impl Completer for crate::llm::LlmRouter {
    async fn complete(&self, prompt: &str) -> anyhow::Result<String> {
        self.local_complete(prompt).await
    }
}

/// What one extraction pass produced, for logging and tests.
#[derive(Debug, Default, PartialEq)]
pub struct ExtractionOutcome {
    pub entities_created: usize,
    pub entities_merged: usize,
    pub relations_added: usize,
}

#[derive(Debug, serde::Deserialize)]
struct Candidate {
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, serde::Deserialize)]
struct RelationCandidate {
    from: String,
    relation: String,
    to: String,
}

#[derive(Debug, serde::Deserialize)]
struct Extraction {
    #[serde(default)]
    entities: Vec<Candidate>,
    #[serde(default)]
    relations: Vec<RelationCandidate>,
}

/// Extract entities/relations from `transcript` and merge them into the
/// project-scoped knowledge graph.
///
/// * Candidates are capped at `max_entities` (config
///   `memory.knowledge_graph.max_entities_per_conversation`, default 20) —
///   the cap is also stated in the prompt, and enforced again after parsing,
///   because a 9B model under instruction drift will occasionally forget.
/// * A candidate whose embedding is within
///   `entity_dedup_threshold` (cosine, default 0.85) of an existing
///   project entity is **merged** (attrs refreshed) rather than duplicated —
///   "postgres" and "PostgreSQL database" must not become two nodes.
/// * Entity vectors live in `kg_embeddings` beside the graph, so dedup costs
///   one embed per candidate, not per existing entity.
#[allow(clippy::too_many_arguments)]
pub async fn extract_and_store(
    kg: &KnowledgeGraph,
    conn: &Arc<std::sync::Mutex<Connection>>,
    embedder: &dyn Embedder,
    completer: &dyn Completer,
    max_entities: usize,
    dedup_threshold: f64,
    project_id: &str,
    transcript: &str,
) -> anyhow::Result<ExtractionOutcome> {
    create_table(conn)?;

    let prompt = build_prompt(transcript, max_entities);
    let response = match completer.complete(&prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "knowledge extraction skipped: master model unavailable");
            return Ok(ExtractionOutcome::default());
        }
    };
    let Some(extraction) = parse_extraction(&response) else {
        tracing::warn!("knowledge extraction skipped: response was not parseable JSON");
        return Ok(ExtractionOutcome::default());
    };

    let mut outcome = ExtractionOutcome::default();

    // Existing project entities with their embeddings, for dedup.
    let existing = load_project_embeddings(conn, project_id, embedder)?;

    // name (lowercased) -> final entity id, for relation endpoint mapping.
    let mut name_to_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for candidate in extraction.entities.into_iter().take(max_entities) {
        let name = candidate.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let kind = if candidate.kind.trim().is_empty() {
            "concept".to_string()
        } else {
            candidate.kind.trim().to_lowercase()
        };
        let text = format!("{name}: {}", candidate.description);

        // Dedup: nearest existing embedding above the threshold wins.
        let mut merged_into: Option<String> = None;
        let candidate_vec = embedder.embed(&text).await.ok();
        if let Some(vec) = &candidate_vec {
            let mut best: Option<(f32, &str)> = None;
            for (id, existing_vec) in &existing {
                if vec.len() != existing_vec.len() {
                    continue;
                }
                let score = cosine(vec, existing_vec);
                if best.map(|(b, _)| score > b).unwrap_or(true) {
                    best = Some((score, id.as_str()));
                }
            }
            if let Some((score, id)) = best {
                if score as f64 >= dedup_threshold {
                    merged_into = Some(id.to_string());
                }
            }
        }

        match merged_into {
            Some(id) => {
                // Refresh the surviving entity's description: the newer
                // mention is the more current one, and stale knowledge is
                // worse than none.
                if let Ok(Some(mut entity)) = kg.entity(&id) {
                    entity.attrs.as_object_mut().map(|m| {
                        m.insert("description".into(), candidate.description.clone().into())
                    });
                    kg.upsert_entity_scoped(project_id, &entity)?;
                    let source = format!("{}: {}", entity.name, candidate.description);
                    if let Ok(vec) = embedder.embed(&source).await {
                        store_embedding(conn, &id, project_id, &vec, embedder, &source)?;
                    }
                }
                outcome.entities_merged += 1;
                name_to_id
                    .entry(name.to_lowercase())
                    .or_insert_with(|| id.clone());
            }
            None => {
                let id = scoped_entity_id(project_id, &kind, &name);
                let mut attrs = serde_json::Map::new();
                if !candidate.description.is_empty() {
                    attrs.insert("description".into(), candidate.description.clone().into());
                }
                let entity = Entity {
                    id: id.clone(),
                    kind,
                    name,
                    attrs: serde_json::Value::Object(attrs),
                };
                kg.upsert_entity_scoped(project_id, &entity)?;
                if let Some(vec) = &candidate_vec {
                    let _ = store_embedding(conn, &id, project_id, vec, embedder, &text);
                }
                outcome.entities_created += 1;
                name_to_id.entry(entity.name.to_lowercase()).or_insert(id);
            }
        }
    }

    for rel in &extraction.relations {
        let (Some(from), Some(to)) = (
            name_to_id.get(&rel.from.trim().to_lowercase()),
            name_to_id.get(&rel.to.trim().to_lowercase()),
        ) else {
            // A relation to something not extracted this round (or already
            // merged under another name) is dropped, not guessed into
            // existence.
            continue;
        };
        let relation = normalize_relation(&rel.relation);
        if relation.is_empty() {
            continue;
        }
        kg.add_edge(from, &relation, to)?;
        outcome.relations_added += 1;
    }

    Ok(outcome)
}

fn build_prompt(transcript: &str, max_entities: usize) -> String {
    // Bound the transcript: a long conversation pasted whole does not make a
    // 9B model extract better, it makes it run out of context.
    let transcript: String = transcript.chars().take(6000).collect();
    format!(
        "You extract a knowledge graph from a conversation transcript.\n\n\
         Transcript:\n{transcript}\n\n\
         Extract at most {max_entities} entities and the relations between them. \
         An entity is a durable concept, decision, person, tool, file, or error worth \
         remembering — not a transient word.\n\
         kind must be one of: concept, decision, person, tool, file, error.\n\
         relation must be 1-3 lowercase words (e.g. \"depends on\", \"decided\", \"caused\").\n\n\
         Respond with ONLY a JSON object of this exact shape, no prose, no markdown fences:\n\
         {{\n  \
           \"entities\": [{{\"name\": \"...\", \"kind\": \"concept\", \"description\": \"one sentence\"}}],\n  \
           \"relations\": [{{\"from\": \"entity name\", \"relation\": \"...\", \"to\": \"entity name\"}}]\n\
         }}"
    )
}

/// Parse the model's response, tolerating fences and surrounding prose the
/// way `Planner::extract_plan` does — a 9B model asked for "only JSON" will
/// sometimes open with a sentence anyway.
fn parse_extraction(response: &str) -> Option<Extraction> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&response[start..=end]).ok()
}

fn normalize_relation(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in raw.trim().chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

pub(crate) fn create_table(conn: &Arc<std::sync::Mutex<Connection>>) -> anyhow::Result<()> {
    let conn = conn.lock().unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kg_embeddings (
             entity_id  TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             embedding  BLOB NOT NULL,
             dim        INTEGER NOT NULL
         );",
    )?;
    crate::memory::rag::migrate_space(&conn, "kg_embeddings")?;
    Ok(())
}

type ExistingEmbeddings = Vec<(String, Vec<f32>)>;

fn load_project_embeddings(
    conn: &Arc<std::sync::Mutex<Connection>>,
    project_id: &str,
    embedder: &dyn Embedder,
) -> anyhow::Result<ExistingEmbeddings> {
    let conn = conn.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT k.entity_id, k.embedding, k.dim FROM kg_embeddings k
         JOIN entities e ON e.id = k.entity_id
         WHERE k.project_id = ?1 AND e.project_id = ?1 AND k.provider = ?2 AND k.model = ?3",
    )?;
    let rows = stmt.query_map(
        params![project_id, embedder.provider(), embedder.model()],
        |r| {
            let blob: Vec<u8> = r.get(1)?;
            let dim: i64 = r.get(2)?;
            Ok((
                r.get::<_, String>(0)?,
                blob.chunks_exact(4)
                    .take(dim as usize)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect::<Vec<f32>>(),
            ))
        },
    )?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(crate) fn store_embedding(
    conn: &Arc<std::sync::Mutex<Connection>>,
    entity_id: &str,
    project_id: &str,
    vec: &[f32],
    embedder: &dyn Embedder,
    source: &str,
) -> anyhow::Result<()> {
    let mut blob = Vec::with_capacity(vec.len() * 4);
    for f in vec {
        blob.extend_from_slice(&f.to_le_bytes());
    }
    let conn = conn.lock().unwrap();
    conn.execute(
        "INSERT INTO kg_embeddings (entity_id, project_id, embedding, dim, provider, model, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(entity_id) DO UPDATE SET
             embedding = excluded.embedding, dim = excluded.dim, provider = excluded.provider, model = excluded.model, source = excluded.source",
        params![entity_id, project_id, blob, vec.len() as i64, embedder.provider(), embedder.model(), source],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::rag::HashEmbedder;

    struct FixedCompleter(String);
    #[async_trait::async_trait]
    impl Completer for FixedCompleter {
        async fn complete(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok(self.0.clone())
        }
    }
    struct FailingCompleter;
    #[async_trait::async_trait]
    impl Completer for FailingCompleter {
        async fn complete(&self, _prompt: &str) -> anyhow::Result<String> {
            anyhow::bail!("model offline")
        }
    }

    fn setup() -> (
        KnowledgeGraph,
        Arc<std::sync::Mutex<Connection>>,
        Arc<dyn Embedder>,
    ) {
        let kg = KnowledgeGraph::in_memory().unwrap();
        let conn = kg.shared_conn();
        (kg, conn, Arc::new(HashEmbedder { dim: 128 }))
    }

    #[tokio::test]
    async fn dedup_never_merges_different_spaces_even_at_zero_threshold() {
        for mismatch in ["model", "provider", "dimensions"] {
            let (kg, conn, embedder) = setup();
            let first =
                r#"{"entities":[{"name":"postgres","kind":"tool","description":"database"}]}"#;
            extract_and_store(
                &kg,
                &conn,
                embedder.as_ref(),
                &FixedCompleter(first.into()),
                20,
                0.85,
                "p",
                "t",
            )
            .await
            .unwrap();
            let sql = match mismatch {
                "model" => "UPDATE kg_embeddings SET model = 'another'",
                "provider" => "UPDATE kg_embeddings SET provider = 'another'",
                _ => "UPDATE kg_embeddings SET dim = 1",
            };
            conn.lock().unwrap().execute(sql, []).unwrap();
            let second =
                r#"{"entities":[{"name":"sqlite","kind":"tool","description":"database"}]}"#;
            let out = extract_and_store(
                &kg,
                &conn,
                embedder.as_ref(),
                &FixedCompleter(second.into()),
                20,
                0.0,
                "p",
                "t",
            )
            .await
            .unwrap();
            assert_eq!(out.entities_merged, 0, "{mismatch}");
            assert_eq!(out.entities_created, 1, "{mismatch}");
        }
    }

    #[tokio::test]
    async fn extracts_creates_and_links_entities() {
        let (kg, conn, embedder) = setup();
        let out = extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FixedCompleter(
                r#"{"entities":[
                     {"name":"postgres","kind":"tool","description":"the chosen database"},
                     {"name":"nightly cron","kind":"error","description":"fails at 2am"}
                   ],
                   "relations":[{"from":"nightly cron","relation":"Writes To","to":"postgres"}]}"#
                    .into(),
            ),
            20,
            0.85,
            "webapp",
            "we picked postgres; the nightly cron writes to it",
        )
        .await
        .unwrap();

        assert_eq!(
            out,
            ExtractionOutcome {
                entities_created: 2,
                entities_merged: 0,
                relations_added: 1
            }
        );
        assert_eq!(kg.entities_in_project("webapp").unwrap().len(), 2);
        let edges = kg
            .edges_from(&scoped_entity_id("webapp", "error", "nightly cron"))
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, "writes-to");
        assert_eq!(edges[0].to, scoped_entity_id("webapp", "tool", "postgres"));
    }

    #[tokio::test]
    async fn near_duplicate_entities_merge_instead_of_duplicating() {
        let (kg, conn, embedder) = setup();
        let json = r#"{"entities":[{"name":"postgres","kind":"tool","description":"postgres database choice"}]}"#;
        extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FixedCompleter(json.into()),
            20,
            0.85,
            "p",
            "t",
        )
        .await
        .unwrap();
        // Same text -> cosine 1.0 -> merge, not a second entity.
        let out = extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FixedCompleter(json.into()),
            20,
            0.85,
            "p",
            "t",
        )
        .await
        .unwrap();
        assert_eq!(out.entities_merged, 1);
        assert_eq!(kg.entities_in_project("p").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_entity_cap_is_enforced_after_parsing() {
        let (kg, conn, embedder) = setup();
        let many: Vec<String> = (0..40)
            .map(|i| format!(r#"{{"name":"thing {i}","kind":"concept","description":"unique content number {i} various words {i}"}}"#))
            .collect();
        let json = format!(r#"{{"entities":[{}],"relations":[]}}"#, many.join(","));
        let out = extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FixedCompleter(json),
            5,
            0.99,
            "p",
            "t",
        )
        .await
        .unwrap();
        assert_eq!(out.entities_created, 5, "cap enforced after parsing");
    }

    #[tokio::test]
    async fn relations_to_unknown_entities_are_dropped() {
        let (kg, conn, embedder) = setup();
        let out = extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FixedCompleter(
                r#"{"entities":[{"name":"a","kind":"concept","description":"a thing"}],
                   "relations":[{"from":"a","relation":"uses","to":"ghost"}]}"#
                    .into(),
            ),
            20,
            0.85,
            "p",
            "t",
        )
        .await
        .unwrap();
        assert_eq!(out.relations_added, 0);
    }

    #[tokio::test]
    async fn prose_around_the_json_is_tolerated() {
        let (kg, conn, embedder) = setup();
        let out = extract_and_store(
            &kg, &conn, embedder.as_ref(),
            &FixedCompleter(
                "Here is the extraction:\n```json\n{\"entities\":[{\"name\":\"x\",\"description\":\"d\"}],\"relations\":[]}\n```\nDone.".into(),
            ),
            20, 0.85, "p", "t",
        )
        .await
        .unwrap();
        assert_eq!(out.entities_created, 1);
        assert_eq!(kg.entities_in_project("p").unwrap()[0].kind, "concept");
    }

    #[tokio::test]
    async fn an_unreachable_model_is_a_no_op_not_an_error() {
        let (kg, conn, embedder) = setup();
        let out = extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FailingCompleter,
            20,
            0.85,
            "p",
            "t",
        )
        .await
        .unwrap();
        assert_eq!(out, ExtractionOutcome::default());
    }

    #[tokio::test]
    async fn garbage_output_is_a_no_op_not_an_error() {
        let (kg, conn, embedder) = setup();
        let out = extract_and_store(
            &kg,
            &conn,
            embedder.as_ref(),
            &FixedCompleter("I could not parse that transcript, sorry.".into()),
            20,
            0.85,
            "p",
            "t",
        )
        .await
        .unwrap();
        assert_eq!(out, ExtractionOutcome::default());
    }
}
