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
//!
//! # Project scoping
//!
//! Entities carry a nullable `project_id`. NULL is the fleet/global scope —
//! everything `machines.rs` writes lives there, untouched. Conversation-scoped
//! entities (extracted concepts, decisions) set `project_id` and namespace their
//! ids as `p:<project>:<kind>:<name>` (see [`scoped_entity_id`]), so the primary
//! key stays unique across scopes without rebuilding the table and invalidating
//! every existing database. The legacy read methods ([`entities_of_kind`],
//! [`snapshot`]) are explicitly *unscoped* reads — the machine UI must never see
//! a project's conversation concepts wander into it — and scoped access goes
//! through [`upsert_entity_scoped`] / [`entities_in_project`] /
//! [`search_entities`]. Databases created before the column existed are
//! migrated in place by [`KnowledgeGraph::open`]; the live `~/.hive/hive.db`
//! must survive, not be recreated.

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

/// Build the id for a project-scoped entity.
///
/// The `p:` prefix plus project name keeps scoped ids in a namespace of their
/// own, so `concept:latency` in one project can never collide with — or
/// upsert over — the same name in another, or in the fleet scope. Callers
/// pass the result as the entity's `id`; [`KnowledgeGraph::upsert_entity_scoped`]
/// records the `project_id` column alongside it.
pub fn scoped_entity_id(project_id: &str, kind: &str, name: &str) -> String {
    format!("p:{project_id}:{kind}:{name}")
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
    ///
    /// The database and its SQLite sidecars are forced to mode 0600 before
    /// schema setup: conversation transcripts and extracted
    /// knowledge land in the same database as the incident log, and flagged
    /// output is kept verbatim there deliberately (see `watchdog/incidents.rs`)
    /// — so the memory tables inherit the same lock rather than becoming the
    /// soft end of the file.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::from_connection(crate::private_db::open(path.as_ref())?)
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
        // In-place migration for databases created before project scoping
        // existed. SQLite cannot add a column inside CREATE TABLE IF NOT
        // EXISTS, and recreating the table would strand the live database's
        // machine fleet — so the column is added lazily, guarded by a
        // table_info probe.
        ensure_column(&conn, "entities", "project_id", "ALTER TABLE entities ADD COLUMN project_id TEXT")?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS entities_project ON entities(project_id);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// The shared connection, for sibling stores (projects, RAG) that live in
    /// the same database file. One connection pool per process, one WAL —
    /// separate connections to the same file would work, but they would also
    /// re-run schema setup and fight over write locks for no gain.
    pub fn shared_conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Insert or replace a **fleet-scope** entity (project_id NULL), keyed on
    /// its id. Everything `machines.rs` writes goes through here and must keep
    /// landing in the global scope.
    pub fn upsert_entity(&self, entity: &Entity) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, attrs, updated_at, project_id)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), NULL)
             ON CONFLICT(id) DO UPDATE SET
                 kind = excluded.kind,
                 name = excluded.name,
                 attrs = excluded.attrs,
                 updated_at = excluded.updated_at,
                 project_id = NULL",
            params![
                entity.id,
                entity.kind,
                entity.name,
                entity.attrs.to_string()
            ],
        )?;
        Ok(())
    }

    /// Insert or replace a **project-scoped** entity. The caller is expected
    /// to have built the id with [`scoped_entity_id`] — the graph does not
    /// force it, but an un-namespaced id here would silently collide with the
    /// fleet namespace.
    pub fn upsert_entity_scoped(
        &self,
        project_id: &str,
        entity: &Entity,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, attrs, updated_at, project_id)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5)
             ON CONFLICT(id) DO UPDATE SET
                 kind = excluded.kind,
                 name = excluded.name,
                 attrs = excluded.attrs,
                 updated_at = excluded.updated_at,
                 project_id = excluded.project_id",
            params![
                entity.id,
                entity.kind,
                entity.name,
                entity.attrs.to_string(),
                project_id
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

    /// Delete an entity and every edge touching it.
    ///
    /// Edges cascade via `ON DELETE CASCADE`, so retiring a machine also drops
    /// its `has_tool` / `runs_os` relations rather than leaving them dangling.
    /// Returns whether anything was removed.
    pub fn remove_entity(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute("DELETE FROM entities WHERE id = ?1", params![id])?;
        Ok(removed > 0)
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

    /// Entities of `kind` in the **fleet scope**. Project-scoped entities are
    /// invisible here on purpose: `machines.rs` asks this question, and a
    /// conversation's extracted `concept:gpu` must never masquerade as fleet
    /// inventory.
    pub fn entities_of_kind(&self, kind: &str) -> anyhow::Result<Vec<Entity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, name, attrs FROM entities
             WHERE kind = ?1 AND project_id IS NULL ORDER BY name",
        )?;
        let rows = stmt.query_map(params![kind], row_to_entity)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every entity in a project's scope.
    pub fn entities_in_project(&self, project_id: &str) -> anyhow::Result<Vec<Entity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, name, attrs FROM entities
             WHERE project_id = ?1 ORDER BY kind, name",
        )?;
        let rows = stmt.query_map(params![project_id], row_to_entity)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Project-scoped entities matching any of `terms`, by substring over
    /// name, kind, and the attribute bag.
    ///
    /// This is deliberately keyword matching, not embeddings: the entity set
    /// a 9B model extracts from a conversation is small (≤20 per conversation)
    /// and human-readable, and `retrieve_context` already pays for one
    /// embedding round-trip on the RAG side — a second one here would double
    /// the latency of every turn to rank a handful of rows.
    pub fn search_entities(
        &self,
        project_id: &str,
        terms: &[&str],
        limit: usize,
    ) -> anyhow::Result<Vec<Entity>> {
        if terms.is_empty() {
            return Ok(vec![]);
        }
        // Fetch the project's entities (a small set — extraction is capped)
        // and match in Rust: the terms are user input, not a trusted pattern,
        // and LIKE-escaping arbitrary text to search five rows is ceremony.
        let terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        Ok(self
            .entities_in_project(project_id)?
            .into_iter()
            .filter(|e| {
                let hay = format!("{} {} {}", e.name, e.kind, e.attrs).to_lowercase();
                terms.iter().any(|t| hay.contains(t))
            })
            .take(limit)
            .collect())
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

    /// Whole-graph dump of the **fleet scope**, for the UI and for
    /// `hive graph show`. Project-scoped entities are excluded: this snapshot
    /// feeds the machine-graph views, where conversation concepts would be
    /// noise at best and a phantom machine at worst.
    pub fn snapshot(&self) -> anyhow::Result<GraphSnapshot> {
        let conn = self.conn.lock().unwrap();
        let mut e = conn.prepare(
            "SELECT id, kind, name, attrs FROM entities
             WHERE project_id IS NULL ORDER BY kind, name",
        )?;
        let entities: Vec<Entity> = e.query_map([], row_to_entity)?.collect::<Result<_, _>>()?;
        let mut g = conn.prepare(
            "SELECT from_id, relation, to_id FROM edges
             WHERE from_id IN (SELECT id FROM entities WHERE project_id IS NULL)",
        )?;
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

/// Add `column` to `table` if it does not exist yet.
///
/// `CREATE TABLE IF NOT EXISTS` cannot evolve an existing table, and the
/// databases that need this column already hold live data — so the migration
/// is a guarded `ALTER TABLE`, not a recreate.
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    if !existing.contains(&column.to_string()) {
        conn.execute_batch(ddl)?;
    }
    Ok(())
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
    fn removing_an_entity_takes_its_edges_with_it() {
        let kg = graph();
        kg.upsert_entity(&Entity::new("machine", "retired", json!({}))).unwrap();
        kg.upsert_entity(&Entity::new("tool", "claude", json!({}))).unwrap();
        kg.add_edge("machine:retired", "has_tool", "tool:claude").unwrap();

        assert!(kg.remove_entity("machine:retired").unwrap());
        assert!(kg.entity("machine:retired").unwrap().is_none());
        // The edge must not survive its endpoint.
        assert!(kg.sources_of("has_tool", "tool:claude").unwrap().is_empty());
        // The tool itself is shared, so it stays.
        assert!(kg.entity("tool:claude").unwrap().is_some());
        // Removing something absent is not an error.
        assert!(!kg.remove_entity("machine:retired").unwrap());
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

    #[test]
    fn scoped_entities_are_invisible_to_fleet_reads() {
        let kg = graph();
        kg.upsert_entity(&Entity::new("machine", "a", json!({})))
            .unwrap();
        let id = scoped_entity_id("webapp", "concept", "latency");
        kg.upsert_entity_scoped(
            "webapp",
            &Entity {
                id: id.clone(),
                kind: "concept".into(),
                name: "latency".into(),
                attrs: json!({"description": "p99 under 200ms"}),
            },
        )
        .unwrap();

        // Fleet reads never see the project's concept — not by kind…
        assert_eq!(kg.entities_of_kind("concept").unwrap().len(), 0);
        // …and not through the snapshot the machine UI reads.
        let snap = kg.snapshot().unwrap();
        assert_eq!(snap.entities.len(), 1);
        assert!(snap.entities.iter().all(|e| e.id == "machine:a"));
        // But the scoped read does, and id-addressed reads work across scopes.
        assert_eq!(kg.entities_in_project("webapp").unwrap().len(), 1);
        assert_eq!(
            kg.entity(&id).unwrap().unwrap().attr_str("description"),
            Some("p99 under 200ms")
        );
    }

    #[test]
    fn same_name_in_two_projects_never_collides() {
        let kg = graph();
        for project in ["alpha", "beta"] {
            kg.upsert_entity_scoped(
                project,
                &Entity {
                    id: scoped_entity_id(project, "concept", "latency"),
                    kind: "concept".into(),
                    name: "latency".into(),
                    attrs: json!({"project": project}),
                },
            )
            .unwrap();
        }
        assert_eq!(kg.entities_in_project("alpha").unwrap().len(), 1);
        assert_eq!(kg.entities_in_project("beta").unwrap().len(), 1);
        assert_eq!(
            kg.entity(&scoped_entity_id("alpha", "concept", "latency"))
                .unwrap()
                .unwrap()
                .attr_str("project"),
            Some("alpha")
        );
    }

    #[test]
    fn search_entities_matches_names_and_attributes() {
        let kg = graph();
        kg.upsert_entity_scoped(
            "webapp",
            &Entity {
                id: scoped_entity_id("webapp", "decision", "sqlite-over-postgres"),
                kind: "decision".into(),
                name: "sqlite-over-postgres".into(),
                attrs: json!({"why": "single-writer local deployments"}),
            },
        )
        .unwrap();
        let hits = kg.search_entities("webapp", &["postgres"], 8).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(kg.search_entities("webapp", &["unrelated"], 8).unwrap().is_empty());
        // Scope isolation: another project's terms find nothing here.
        assert!(kg.search_entities("other", &["postgres"], 8).unwrap().is_empty());
    }

    #[test]
    fn a_legacy_database_migrates_in_place_without_losing_the_fleet() {
        let dir = std::env::temp_dir().join(format!("hive-graph-migrate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        {
            // A database exactly as the pre-scoping schema created it.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE entities (
                     id     TEXT PRIMARY KEY,
                     kind   TEXT NOT NULL,
                     name   TEXT NOT NULL,
                     attrs  TEXT NOT NULL DEFAULT '{}',
                     updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 INSERT INTO entities (id, kind, name, attrs)
                     VALUES ('machine:lawfinder', 'machine', 'lawfinder', '{}');",
            )
            .unwrap();
        }
        let kg = KnowledgeGraph::open(&path).expect("legacy db opens after migration");
        // The machine survived, in the fleet scope.
        assert!(kg.entity("machine:lawfinder").unwrap().is_some());
        assert_eq!(kg.entities_of_kind("machine").unwrap().len(), 1);
        // And scoped writes work on the same file.
        kg.upsert_entity_scoped(
            "proj",
            &Entity {
                id: scoped_entity_id("proj", "concept", "x"),
                kind: "concept".into(),
                name: "x".into(),
                attrs: json!({}),
            },
        )
        .unwrap();
        assert_eq!(kg.entities_in_project("proj").unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_database_is_not_world_readable() {
        let dir = std::env::temp_dir().join(format!("hive-graph-mode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hive.db");
        let graph = KnowledgeGraph::open(&path).unwrap();
        graph
            .upsert_entity(&Entity::new("concept", "private", json!({})))
            .unwrap();
        crate::private_db::tests::assert_private(&path);
        drop(graph);
        std::fs::remove_dir_all(&dir).ok();
    }
}
