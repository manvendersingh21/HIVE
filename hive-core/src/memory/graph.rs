//! A small, persistent knowledge graph over SQLite.
//!
//! Entities are typed nodes (`machine`, `os`, `tool`, `arch`, …) carrying a
//! free-form JSON attribute bag; edges are typed relations between them. That
//! is deliberately generic: the first thing built on it is the machine graph
//! (see [`super::machines`]), but projects, conversations, and extracted
//! concepts are meant to land in the same two tables rather than growing a new
//! schema each time.
//!
//! Entity ids are caller-chosen and namespaced `kind:name` (`machine:lawfinder`),
//! which makes upserts idempotent — re-probing a machine updates it in place
//! instead of duplicating it.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// A typed node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    /// Namespaced id, `kind:name`.
    pub id: String,
    pub kind: String,
    pub name: String,
    /// Free-form attributes. Shape is per-`kind`, not enforced here.
    pub attrs: serde_json::Value,
}

impl Entity {
    pub fn new(kind: &str, name: &str, attrs: serde_json::Value) -> Self {
        Self {
            id: entity_id(kind, name),
            kind: kind.to_string(),
            name: name.to_string(),
            attrs,
        }
    }

    /// Read a string attribute, if present.
    pub fn attr_str(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).and_then(|v| v.as_str())
    }

    /// Read a numeric attribute, if present.
    pub fn attr_f64(&self, key: &str) -> Option<f64> {
        self.attrs.get(key).and_then(|v| v.as_f64())
    }
}

/// Build the canonical id for an entity.
pub fn entity_id(kind: &str, name: &str) -> String {
    format!("{kind}:{name}")
}

/// A typed, directed relation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    pub from: String,
    pub relation: String,
    pub to: String,
}

/// SQLite-backed knowledge graph. Cloning shares the same connection.
#[derive(Clone)]
pub struct KnowledgeGraph {
    conn: Arc<Mutex<Connection>>,
}

impl KnowledgeGraph {
    /// Open (creating if needed) a graph at `path`. Parent directories are
    /// created — the configured default lives under `~/.hive/`, which will not
    /// exist on a fresh machine.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// An ephemeral in-memory graph. Used by tests, and as the fallback when
    /// the on-disk database can't be opened — a broken db file should degrade
    /// the agent's memory, not stop it from starting.
    pub fn in_memory() -> anyhow::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS entities (
                 id     TEXT PRIMARY KEY,
                 kind   TEXT NOT NULL,
                 name   TEXT NOT NULL,
                 attrs  TEXT NOT NULL DEFAULT '{}',
                 updated_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS entities_kind ON entities(kind);
             CREATE TABLE IF NOT EXISTS edges (
                 from_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
                 relation TEXT NOT NULL,
                 to_id    TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
                 PRIMARY KEY (from_id, relation, to_id)
             );
             CREATE INDEX IF NOT EXISTS edges_to ON edges(to_id, relation);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert or replace an entity, keyed on its id.
    pub fn upsert_entity(&self, entity: &Entity) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, attrs, updated_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                 kind = excluded.kind,
                 name = excluded.name,
                 attrs = excluded.attrs,
                 updated_at = excluded.updated_at",
            params![
                entity.id,
                entity.kind,
                entity.name,
                entity.attrs.to_string()
            ],
        )?;
        Ok(())
    }

    /// Add an edge. Both endpoints must already exist.
    pub fn add_edge(&self, from: &str, relation: &str, to: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO edges (from_id, relation, to_id) VALUES (?1, ?2, ?3)",
            params![from, relation, to],
        )?;
        Ok(())
    }

    /// Drop every edge leaving `from` under `relation`. Used before re-writing
    /// a machine's tool set, so tools that were uninstalled actually disappear.
    pub fn clear_relation(&self, from: &str, relation: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM edges WHERE from_id = ?1 AND relation = ?2",
            params![from, relation],
        )?;
        Ok(())
    }

    pub fn entity(&self, id: &str) -> anyhow::Result<Option<Entity>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, kind, name, attrs FROM entities WHERE id = ?1",
                params![id],
                row_to_entity,
            )
            .optional()?;
        Ok(row)
    }

    pub fn entities_of_kind(&self, kind: &str) -> anyhow::Result<Vec<Entity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, kind, name, attrs FROM entities WHERE kind = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![kind], row_to_entity)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Entities reachable from `from` along `relation`.
    pub fn neighbors(&self, from: &str, relation: &str) -> anyhow::Result<Vec<Entity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.kind, e.name, e.attrs
             FROM edges g JOIN entities e ON e.id = g.to_id
             WHERE g.from_id = ?1 AND g.relation = ?2
             ORDER BY e.name",
        )?;
        let rows = stmt.query_map(params![from, relation], row_to_entity)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Entities pointing *at* `to` along `relation` — "who has this tool?".
    pub fn sources_of(&self, relation: &str, to: &str) -> anyhow::Result<Vec<Entity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.kind, e.name, e.attrs
             FROM edges g JOIN entities e ON e.id = g.from_id
             WHERE g.to_id = ?1 AND g.relation = ?2
             ORDER BY e.name",
        )?;
        let rows = stmt.query_map(params![to, relation], row_to_entity)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn edges_from(&self, from: &str) -> anyhow::Result<Vec<Edge>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT from_id, relation, to_id FROM edges WHERE from_id = ?1")?;
        let rows = stmt.query_map(params![from], |r| {
            Ok(Edge {
                from: r.get(0)?,
                relation: r.get(1)?,
                to: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Whole-graph dump, for the UI and for `hive graph show`.
    pub fn snapshot(&self) -> anyhow::Result<GraphSnapshot> {
        let conn = self.conn.lock().unwrap();
        let mut e =
            conn.prepare("SELECT id, kind, name, attrs FROM entities ORDER BY kind, name")?;
        let entities: Vec<Entity> = e.query_map([], row_to_entity)?.collect::<Result<_, _>>()?;
        let mut g = conn.prepare("SELECT from_id, relation, to_id FROM edges")?;
        let edges: Vec<Edge> = g
            .query_map([], |r| {
                Ok(Edge {
                    from: r.get(0)?,
                    relation: r.get(1)?,
                    to: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(GraphSnapshot { entities, edges })
    }
}

/// A full graph dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub entities: Vec<Entity>,
    pub edges: Vec<Edge>,
}

fn row_to_entity(row: &rusqlite::Row) -> rusqlite::Result<Entity> {
    let attrs: String = row.get(3)?;
    Ok(Entity {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        attrs: serde_json::from_str(&attrs).unwrap_or(serde_json::Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph() -> KnowledgeGraph {
        KnowledgeGraph::in_memory().expect("in-memory graph opens")
    }

    #[test]
    fn upsert_is_idempotent_and_updates_in_place() {
        let kg = graph();
        let mut m = Entity::new("machine", "lawfinder", json!({"cores": 2}));
        kg.upsert_entity(&m).unwrap();
        m.attrs = json!({"cores": 8});
        kg.upsert_entity(&m).unwrap();

        assert_eq!(kg.entities_of_kind("machine").unwrap().len(), 1);
        let stored = kg.entity("machine:lawfinder").unwrap().expect("exists");
        assert_eq!(stored.attr_f64("cores"), Some(8.0));
    }

    #[test]
    fn traverses_edges_in_both_directions() {
        let kg = graph();
        for (kind, name) in [
            ("machine", "lawfinder"),
            ("machine", "mini"),
            ("tool", "claude"),
        ] {
            kg.upsert_entity(&Entity::new(kind, name, json!({})))
                .unwrap();
        }
        kg.add_edge("machine:lawfinder", "has_tool", "tool:claude")
            .unwrap();
        kg.add_edge("machine:mini", "has_tool", "tool:claude")
            .unwrap();

        let tools = kg.neighbors("machine:lawfinder", "has_tool").unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "claude");

        // The query that actually matters: which machines can run claude?
        let machines = kg.sources_of("has_tool", "tool:claude").unwrap();
        assert_eq!(
            machines.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["lawfinder", "mini"]
        );
    }

    #[test]
    fn duplicate_edges_collapse() {
        let kg = graph();
        kg.upsert_entity(&Entity::new("machine", "a", json!({})))
            .unwrap();
        kg.upsert_entity(&Entity::new("tool", "git", json!({})))
            .unwrap();
        kg.add_edge("machine:a", "has_tool", "tool:git").unwrap();
        kg.add_edge("machine:a", "has_tool", "tool:git").unwrap();
        assert_eq!(kg.edges_from("machine:a").unwrap().len(), 1);
    }

    #[test]
    fn clear_relation_removes_stale_tools() {
        let kg = graph();
        kg.upsert_entity(&Entity::new("machine", "a", json!({})))
            .unwrap();
        for t in ["git", "docker"] {
            kg.upsert_entity(&Entity::new("tool", t, json!({})))
                .unwrap();
            kg.add_edge("machine:a", "has_tool", &entity_id("tool", t))
                .unwrap();
        }
        kg.clear_relation("machine:a", "has_tool").unwrap();
        assert!(kg.neighbors("machine:a", "has_tool").unwrap().is_empty());
    }

    #[test]
    fn snapshot_returns_everything() {
        let kg = graph();
        kg.upsert_entity(&Entity::new("machine", "a", json!({})))
            .unwrap();
        kg.upsert_entity(&Entity::new("os", "ubuntu", json!({})))
            .unwrap();
        kg.add_edge("machine:a", "runs_os", "os:ubuntu").unwrap();
        let snap = kg.snapshot().unwrap();
        assert_eq!(snap.entities.len(), 2);
        assert_eq!(snap.edges.len(), 1);
    }
}
