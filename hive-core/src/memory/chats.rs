//! Durable web turns layered over the shared conversation/message tables.
//! Reading history never executes work. Interrupted runs require a new request.
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ChatStore {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Serialize)]
pub struct ChatSummary {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub updated_at: String,
    pub message_count: i64,
}

#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub created_at: String,
    pub turn_id: Option<String>,
    pub status: Option<String>,
    pub reply: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct SavedTurn {
    pub id: String,
    pub conversation_id: String,
    pub user_input: String,
    pub user_message_id: i64,
    pub status: String,
    pub reply: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("Chat not found")]
    NotFound,
    #[error(
        "This chat already has a request running or awaiting approval. Open it to see its status."
    )]
    Busy,
    #[error("This request ID was already used for another message")]
    RequestConflict,
}

#[derive(Debug)]
pub enum StartTurn {
    New(SavedTurn),
    Existing(SavedTurn),
}

impl ChatStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> anyhow::Result<Self> {
        conn.lock().unwrap().execute_batch(
            "CREATE TABLE IF NOT EXISTS web_chat_turns (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                user_message_id INTEGER NOT NULL REFERENCES messages(id),
                assistant_message_id INTEGER NOT NULL UNIQUE REFERENCES messages(id),
                run_id TEXT UNIQUE,
                status TEXT NOT NULL,
                reply TEXT
            );
            CREATE UNIQUE INDEX IF NOT EXISTS web_chat_active ON web_chat_turns(conversation_id)
                WHERE status IN ('planning', 'executing', 'awaiting_approval');",
        )?;
        Ok(Self { conn })
    }

    /// Called once by the server at startup, never by a read endpoint.
    pub fn recover_interrupted(&self) -> anyhow::Result<usize> {
        let mut db = self.conn.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("UPDATE messages SET content = 'The server restarted before this request finished. Some commands may have run. Check their results before submitting again.' WHERE id IN (SELECT assistant_message_id FROM web_chat_turns WHERE status IN ('planning','executing'))", [])?;
        let n = tx.execute("UPDATE web_chat_turns SET status = 'interrupted' WHERE status IN ('planning','executing')", [])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn create(&self, project: Option<&str>) -> anyhow::Result<ChatSummary> {
        let project = project.unwrap_or("web");
        anyhow::ensure!(!project.trim().is_empty(), "Project must not be empty");
        let id = format!("conv-{}", uuid::Uuid::new_v4());
        {
            let mut db = self.conn.lock().unwrap();
            let tx = db.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO projects (id,name) VALUES (?1,?1)",
                [project],
            )?;
            tx.execute(
                "INSERT INTO conversations (id,project_id,title) VALUES (?1,?2,'New chat')",
                params![id, project],
            )?;
            tx.commit()?;
        }
        self.get(&id)?.ok_or_else(|| ChatError::NotFound.into())
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<ChatSummary>> {
        let db = self.conn.lock().unwrap();
        Ok(db.query_row("SELECT c.id,c.project_id,c.title,COALESCE(MAX(m.created_at),c.started_at),COUNT(m.id) FROM conversations c LEFT JOIN messages m ON m.conversation_id=c.id WHERE c.id=?1 GROUP BY c.id", [id], summary_row).optional()?)
    }

    pub fn list(&self, query: &str, offset: usize) -> anyhow::Result<Vec<ChatSummary>> {
        let db = self.conn.lock().unwrap();
        let mut stmt = db.prepare("SELECT c.id,c.project_id,c.title,COALESCE(MAX(m.created_at),c.started_at),COUNT(m.id) FROM conversations c LEFT JOIN messages m ON m.conversation_id=c.id WHERE ?1='' OR instr(lower(c.title),lower(?1))>0 OR EXISTS (SELECT 1 FROM messages s WHERE s.conversation_id=c.id AND instr(lower(s.content),lower(?1))>0) GROUP BY c.id ORDER BY COALESCE(MAX(m.id),0) DESC,c.started_at DESC,c.id LIMIT 50 OFFSET ?2")?;
        let rows = stmt
            .query_map(params![query, offset as i64], summary_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn messages(&self, id: &str) -> anyhow::Result<Vec<ChatMessage>> {
        let db = self.conn.lock().unwrap();
        let mut stmt = db.prepare("SELECT m.id,m.role,m.content,m.created_at,t.id,t.status,t.reply FROM messages m LEFT JOIN web_chat_turns t ON t.assistant_message_id=m.id WHERE m.conversation_id=?1 ORDER BY m.id")?;
        let rows = stmt
            .query_map([id], |r| {
                let reply: Option<String> = r.get(6)?;
                Ok(ChatMessage {
                    id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    created_at: r.get(3)?,
                    turn_id: r.get(4)?,
                    status: r.get(5)?,
                    reply: reply.and_then(|s| serde_json::from_str(&s).ok()),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn turn(&self, id: &str) -> anyhow::Result<Option<SavedTurn>> {
        let db = self.conn.lock().unwrap();
        find_turn(&db, id)
    }

    pub fn turn_for_run(&self, id: &str) -> anyhow::Result<Option<SavedTurn>> {
        let db = self.conn.lock().unwrap();
        let id: Option<String> = db
            .query_row("SELECT id FROM web_chat_turns WHERE run_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        id.map(|id| find_turn(&db, &id))
            .transpose()
            .map(Option::flatten)
    }

    /// Save both the user message and its reply slot before contacting a model.
    pub fn begin(&self, chat: &str, id: &str, text: &str) -> anyhow::Result<StartTurn> {
        let mut db = self.conn.lock().unwrap();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(turn) = find_turn(&tx, id)? {
            if turn.conversation_id != chat || turn.user_input != text {
                return Err(ChatError::RequestConflict.into());
            }
            return Ok(StartTurn::Existing(turn));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)",
            [chat],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(ChatError::NotFound.into());
        }
        let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM web_chat_turns WHERE conversation_id=?1 AND status IN ('planning','executing','awaiting_approval'))",[chat],|r|r.get(0))?;
        if busy {
            return Err(ChatError::Busy.into());
        }
        let title: String = text.chars().take(80).collect();
        tx.execute("UPDATE conversations SET title=?2 WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1)",params![chat,title])?;
        tx.execute(
            "INSERT INTO messages(conversation_id,role,content) VALUES (?1,'user',?2)",
            params![chat, text],
        )?;
        let user_id = tx.last_insert_rowid();
        tx.execute("INSERT INTO messages(conversation_id,role,content) VALUES (?1,'assistant','Planning…')",[chat])?;
        let assistant_id = tx.last_insert_rowid();
        tx.execute("INSERT INTO web_chat_turns(id,conversation_id,user_message_id,assistant_message_id,status) VALUES (?1,?2,?3,?4,'planning')",params![id,chat,user_id,assistant_id])?;
        tx.commit()?;
        Ok(StartTurn::New(SavedTurn {
            id: id.into(),
            conversation_id: chat.into(),
            user_input: text.into(),
            user_message_id: user_id,
            status: "planning".into(),
            reply: None,
        }))
    }

    pub fn context(&self, turn: &SavedTurn) -> anyhow::Result<Vec<String>> {
        let db = self.conn.lock().unwrap();
        let mut stmt=db.prepare("SELECT role,content FROM messages WHERE conversation_id=?1 AND id<?2 ORDER BY id DESC LIMIT 10")?;
        let mut rows = stmt
            .query_map(params![turn.conversation_id, turn.user_message_id], |r| {
                let role: String = r.get(0)?;
                let text: String = r.get(1)?;
                Ok(format!(
                    "{role}: {}",
                    text.chars().take(600).collect::<String>()
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn executing(&self, id: &str, run_id: &str) -> anyhow::Result<()> {
        let n=self.conn.lock().unwrap().execute("UPDATE web_chat_turns SET status='executing',run_id=?2 WHERE id=?1 AND status='planning'",params![id,run_id])?;
        anyhow::ensure!(n == 1, "Request is no longer ready to execute");
        Ok(())
    }

    /// Atomic claim prevents two tabs from approving and executing the same run.
    pub fn claim_approval(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.conn.lock().unwrap().execute("UPDATE web_chat_turns SET status='executing' WHERE id=?1 AND status='awaiting_approval'",[id])? == 1)
    }

    pub fn finish(
        &self,
        id: &str,
        status: &str,
        text: &str,
        reply: Option<&Value>,
    ) -> anyhow::Result<()> {
        let mut db = self.conn.lock().unwrap();
        let tx = db.transaction()?;
        let n=tx.execute("UPDATE web_chat_turns SET status=?2,reply=?3 WHERE id=?1 AND status IN ('planning','executing')",params![id,status,reply.map(Value::to_string)])?;
        anyhow::ensure!(n == 1, "Request was already completed or interrupted");
        tx.execute("UPDATE messages SET content=?2 WHERE id=(SELECT assistant_message_id FROM web_chat_turns WHERE id=?1)",params![id,text])?;
        tx.commit()?;
        Ok(())
    }
}

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChatSummary> {
    Ok(ChatSummary {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        updated_at: r.get(3)?,
        message_count: r.get(4)?,
    })
}
fn find_turn(db: &Connection, id: &str) -> anyhow::Result<Option<SavedTurn>> {
    Ok(db.query_row("SELECT t.id,t.conversation_id,m.content,t.user_message_id,t.status,t.reply FROM web_chat_turns t JOIN messages m ON m.id=t.user_message_id WHERE t.id=?1",[id],|r| {
        let reply:Option<String>=r.get(5)?;
        Ok(SavedTurn{id:r.get(0)?,conversation_id:r.get(1)?,user_input:r.get(2)?,user_message_id:r.get(3)?,status:r.get(4)?,reply:reply.and_then(|s|serde_json::from_str(&s).ok())})
    }).optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{graph::KnowledgeGraph, projects::ProjectRegistry};

    fn store() -> ChatStore {
        let graph = KnowledgeGraph::in_memory().unwrap();
        ProjectRegistry::new(graph.shared_conn()).unwrap();
        ChatStore::new(graph.shared_conn()).unwrap()
    }

    #[test]
    fn saves_before_planning_and_deduplicates_requests_without_crossing_chats() {
        let store = store();
        let a = store.create(None).unwrap();
        let b = store.create(None).unwrap();
        let turn = match store.begin(&a.id, "request", "hello").unwrap() {
            StartTurn::New(t) => t,
            _ => panic!(),
        };
        assert_eq!(store.messages(&a.id).unwrap().len(), 2);
        assert!(matches!(
            store.begin(&a.id, "request", "hello").unwrap(),
            StartTurn::Existing(_)
        ));
        assert!(store.begin(&a.id, "request", "different").is_err());
        assert!(store.begin(&b.id, "request", "hello").is_err());
        assert!(store
            .begin(&a.id, "second", "next")
            .unwrap_err()
            .is::<ChatError>());
        assert!(store.messages(&b.id).unwrap().is_empty());
        store
            .finish(&turn.id, "failed", "Model unavailable", None)
            .unwrap();
        let next = match store.begin(&a.id, "second", "follow up").unwrap() {
            StartTurn::New(t) => t,
            _ => panic!(),
        };
        assert_eq!(
            store.context(&next).unwrap(),
            vec!["user: hello", "assistant: Model unavailable"]
        );
        assert_eq!(store.list("unavailable", 0).unwrap().len(), 1);
        assert!(store
            .context(&match store.begin(&b.id, "third", "isolated").unwrap() {
                StartTurn::New(t) => t,
                _ => panic!(),
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reopening_preserves_legacy_history_replies_and_pending_approvals() {
        let path = std::env::temp_dir().join(format!("hive-chat-{}.db", uuid::Uuid::new_v4()));
        let (chat_id, reply) = {
            let graph = KnowledgeGraph::open(&path).unwrap();
            let registry = ProjectRegistry::new(graph.shared_conn()).unwrap();
            let legacy = registry.begin_conversation("old", "Legacy chat").unwrap();
            registry
                .append_message(&legacy.id, "user", "old message")
                .unwrap();
            let store = ChatStore::new(graph.shared_conn()).unwrap();
            assert_eq!(store.list("old message", 0).unwrap()[0].id, legacy.id);
            let chat = store.create(None).unwrap();
            store.begin(&chat.id, "one", "first question").unwrap();
            store.executing("one", "run-one").unwrap();
            let reply =
                serde_json::json!({"run":{"id":"run-one"},"result":{"awaiting_approval":[1]}});
            store
                .finish("one", "awaiting_approval", "Approve step 1", Some(&reply))
                .unwrap();
            (chat.id, reply)
        };
        {
            let graph = KnowledgeGraph::open(&path).unwrap();
            ProjectRegistry::new(graph.shared_conn()).unwrap();
            let store = ChatStore::new(graph.shared_conn()).unwrap();
            assert_eq!(store.recover_interrupted().unwrap(), 0);
            assert_eq!(
                store.messages(&chat_id).unwrap()[1].reply,
                Some(reply.clone())
            );
            assert_eq!(
                store.turn_for_run("run-one").unwrap().unwrap().status,
                "awaiting_approval"
            );
            assert!(store.claim_approval("one").unwrap());
            assert!(!store.claim_approval("one").unwrap());
            store
                .finish("one", "completed", "Command finished", Some(&reply))
                .unwrap();
            assert_eq!(store.messages(&chat_id).unwrap().len(), 2);
            assert!(!store.claim_approval("one").unwrap());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn restart_marks_unfinished_work_interrupted_without_replaying_it() {
        let store = store();
        let chat = store.create(None).unwrap();
        store.begin(&chat.id, "request", "run something").unwrap();
        store.executing("request", "run").unwrap();
        assert_eq!(store.recover_interrupted().unwrap(), 1);
        assert_eq!(store.recover_interrupted().unwrap(), 0);
        let messages = store.messages(&chat.id).unwrap();
        assert_eq!(messages[1].status.as_deref(), Some("interrupted"));
        assert!(messages[1].content.contains("Some commands may have run"));
        assert!(!store.claim_approval("request").unwrap());
        assert!(matches!(
            store.begin(&chat.id, "request", "run something").unwrap(),
            StartTurn::Existing(_)
        ));
        assert!(matches!(
            store.begin(&chat.id, "new", "new request").unwrap(),
            StartTurn::New(_)
        ));
    }
}
