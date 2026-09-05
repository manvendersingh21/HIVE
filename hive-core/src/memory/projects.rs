//! Project registry and conversation persistence.
//!
//! Everything conversational is scoped by `project_id`: a project owns
//! conversations, conversations own messages, and the RAG index and knowledge
//! extraction both key off the same project. This store shares the knowledge
//! graph's connection and database file — one WAL, one set of tables, one
//! 0600 lock on the file — rather than opening a second database per concern.

use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// A registered project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    /// Slug-style id, used directly as the graph's `project_id`.
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// One conversation: a `hive task`, or one request inside a chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub started_at: String,
}

/// A persisted turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub conversation_id: String,
    pub project_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

/// A hit from free-text search over messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageHit {
    pub project_id: String,
    pub conversation_id: String,
    pub title: String,
    pub role: String,
    pub snippet: String,
}

/// SQLite-backed project/conversation/message store.
///
/// Cloning shares the same connection as the knowledge graph it was built
/// from.
#[derive(Clone)]
pub struct ProjectRegistry {
    conn: Arc<Mutex<Connection>>,
}

impl ProjectRegistry {
    /// Build the registry over the graph's shared connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> anyhow::Result<Self> {
        {
            let conn = conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS projects (
                     id         TEXT PRIMARY KEY,
                     name       TEXT NOT NULL,
                     created_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 CREATE TABLE IF NOT EXISTS conversations (
                     id         TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title      TEXT NOT NULL DEFAULT '',
                     started_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 CREATE INDEX IF NOT EXISTS conversations_project
                     ON conversations(project_id, started_at);
                 CREATE TABLE IF NOT EXISTS messages (
                     id               INTEGER PRIMARY KEY AUTOINCREMENT,
                     conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                     role             TEXT NOT NULL,
                     content          TEXT NOT NULL,
                     created_at       TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 CREATE INDEX IF NOT EXISTS messages_conversation
                     ON messages(conversation_id, id);",
            )?;
        }
        Ok(Self { conn })
    }

    /// Create a project, or return the existing one with the same id.
    ///
    /// Idempotent by design: `hive chat --project foo` on a machine that has
    /// never heard of `foo` should just work, not error — and re-running it
    /// must not reset anything.
    pub fn ensure_project(&self, id: &str, name: Option<&str>) -> anyhow::Result<Project> {
        let id = id.trim();
        anyhow::ensure!(!id.is_empty(), "project id must not be empty");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO projects (id, name) VALUES (?1, ?2)",
            params![id, name.unwrap_or(id)],
        )?;
        let row = conn.query_row(
            "SELECT id, name, created_at FROM projects WHERE id = ?1",
            params![id],
            |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    created_at: r.get(2)?,
                })
            },
        )?;
        Ok(row)
    }

    pub fn project(&self, id: &str) -> anyhow::Result<Option<Project>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, name, created_at FROM projects WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        created_at: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, created_at FROM projects ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Start a conversation inside a project, titled by the opening message.
    ///
    /// Every `hive task`/chat request that carries a project starts exactly
    /// one conversation: server-side, each HTTP request builds a fresh agent,
    /// so a "session" concept that outlived the request could not be observed
    /// anyway. Cross-turn recall comes from RAG and the knowledge graph, not
    /// from conversation identity — see `retrieve_context`.
    pub fn begin_conversation(&self, project_id: &str, title: &str) -> anyhow::Result<Conversation> {
        self.ensure_project(project_id, None)?;
        let id = format!("conv-{}", uuid::Uuid::new_v4());
        let title: String = title.chars().take(80).collect();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations (id, project_id, title) VALUES (?1, ?2, ?3)",
            params![id, project_id, title],
        )?;
        Ok(Conversation {
            id,
            project_id: project_id.to_string(),
            title,
            started_at: String::new(),
        })
    }

    pub fn append_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (conversation_id, role, content) VALUES (?1, ?2, ?3)",
            params![conversation_id, role, content],
        )?;
        Ok(())
    }

    /// The last `limit` messages in a project, oldest first — the "recent
    /// messages" third of `retrieve_context`.
    pub fn recent_messages(&self, project_id: &str, limit: usize) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.conversation_id, c.project_id, m.role, m.content, m.created_at
             FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.project_id = ?1
             ORDER BY m.id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project_id, limit as i64], |r| {
            Ok(StoredMessage {
                conversation_id: r.get(0)?,
                project_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        let mut out: Vec<StoredMessage> = rows.collect::<Result<_, _>>()?;
        out.reverse();
        Ok(out)
    }

    /// The full transcript of one conversation, oldest first.
    pub fn conversation_messages(&self, conversation_id: &str) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.conversation_id, c.project_id, m.role, m.content, m.created_at
             FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE m.conversation_id = ?1 ORDER BY m.id",
        )?;
        let rows = stmt.query_map(params![conversation_id], |r| {
            Ok(StoredMessage {
                conversation_id: r.get(0)?,
                project_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Substring search over every project's messages (or one project's),
    /// newest first. This is the substrate of `hive search` — LIKE is honest
    /// here: it works with no embedding model running, and the RAG cosine
    /// search runs beside it, not instead of it.
    pub fn search_messages(
        &self,
        query: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageHit>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.project_id, m.conversation_id, c.title, m.role, m.content
             FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE m.content LIKE '%' || ?1 || '%'
               AND (?2 IS NULL OR c.project_id = ?2)
             ORDER BY m.id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![query, project_id, limit as i64], |r| {
            let content: String = r.get(4)?;
            Ok(MessageHit {
                project_id: r.get(0)?,
                conversation_id: r.get(1)?,
                title: r.get(2)?,
                role: r.get(3)?,
                snippet: snippet_around(&content, query),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Row counts for `hive memory`'s status view.
    pub fn counts(&self) -> anyhow::Result<(usize, usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let count = |sql: &str| -> anyhow::Result<usize> {
            let n: i64 = conn.query_row(sql, [], |r| r.get(0))?;
            Ok(n as usize)
        };
        Ok((
            count("SELECT COUNT(*) FROM projects")?,
            count("SELECT COUNT(*) FROM conversations")?,
            count("SELECT COUNT(*) FROM messages")?,
        ))
    }
}

/// A one-line window around the first match, so `hive search` shows the hit
/// in context instead of a wall of text.
fn snippet_around(content: &str, query: &str) -> String {
    let lower = content.to_lowercase();
    let needle = query.to_lowercase();
    let Some(at) = lower.find(&needle) else {
        return content.chars().take(120).collect();
    };
    let start = at.saturating_sub(40);
    let end = (at + needle.len() + 80).min(content.len());
    let mut s: String = content
        .get(start..end)
        .unwrap_or(content)
        .chars()
        .flat_map(|c| if c == '\n' { vec![' '] } else { vec![c] })
        .collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if end < content.len() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::graph::KnowledgeGraph;

    fn registry() -> ProjectRegistry {
        let kg = KnowledgeGraph::in_memory().unwrap();
        ProjectRegistry::new(kg.shared_conn()).unwrap()
    }

    #[test]
    fn projects_are_idempotent_and_listable() {
        let reg = registry();
        let p1 = reg.ensure_project("webapp", Some("The webapp")).unwrap();
        let p2 = reg.ensure_project("webapp", None).unwrap();
        assert_eq!(p1.id, p2.id);
        assert_eq!(p2.name, "The webapp", "first name wins; ensure never resets");
        assert_eq!(reg.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn conversations_scope_and_persist_messages() {
        let reg = registry();
        let conv = reg.begin_conversation("webapp", "deploy the thing").unwrap();
        reg.append_message(&conv.id, "user", "deploy to staging").unwrap();
        reg.append_message(&conv.id, "assistant", "done: 3 steps").unwrap();

        let recent = reg.recent_messages("webapp", 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].role, "user");
        assert_eq!(recent[1].content, "done: 3 steps");

        let full = reg.conversation_messages(&conv.id).unwrap();
        assert_eq!(full.len(), 2);
    }

    #[test]
    fn recent_messages_stay_project_scoped() {
        let reg = registry();
        for project in ["alpha", "beta"] {
            let conv = reg.begin_conversation(project, "t").unwrap();
            reg.append_message(&conv.id, "user", &format!("note for {project}")).unwrap();
        }
        let recent = reg.recent_messages("alpha", 10).unwrap();
        assert_eq!(recent.len(), 1);
        assert!(recent[0].content.contains("alpha"));
    }

    #[test]
    fn search_finds_a_needle_with_context_and_respects_scope() {
        let reg = registry();
        let conv = reg.begin_conversation("webapp", "db choice").unwrap();
        reg.append_message(
            &conv.id,
            "assistant",
            "We decided to use sqlite over postgres because single-writer local deployments",
        )
        .unwrap();

        let hits = reg.search_messages("postgres", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].project_id, "webapp");
        assert!(hits[0].snippet.contains("sqlite"));

        assert!(reg.search_messages("postgres", Some("other"), 10).unwrap().is_empty());
    }

    #[test]
    fn counts_reflect_what_was_written() {
        let reg = registry();
        let conv = reg.begin_conversation("p", "t").unwrap();
        reg.append_message(&conv.id, "user", "x").unwrap();
        assert_eq!(reg.counts().unwrap(), (1, 1, 1));
    }
}
