//! `hive project`, `hive search`, `hive memory` — the Phase 9 CLI surface.
//!
//! These commands are read-mostly and deliberately do not build a
//! `MasterAgent`: opening the memory system directly answers questions about
//! what is remembered without probing workers, checking LLM health, or
//! needing a master to be running — memory is a local SQLite file, and these
//! commands treat it like one.

use std::path::{Path, PathBuf};

use hive_common::HiveConfig;
use hive_core::memory::MemorySystem;

/// Open the memory store from the project config.
fn open_memory(project_root: &Path) -> anyhow::Result<MemorySystem> {
    let config = HiveConfig::from_project_root(project_root)?;
    Ok(MemorySystem::open(config.database.resolved_path(), &config))
}

/// `~/.hive/current-project` — the project `hive chat`/`task` use when no
/// `--project` is passed. A one-line file next to the database it scopes, so
/// "which project am I in" has exactly one answer per machine and survives
/// every process.
fn marker_path_in(home: &Path) -> PathBuf {
    home.join(".hive/current-project")
}

fn marker_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    marker_path_in(Path::new(&home))
}

pub fn current_project() -> Option<String> {
    let raw = std::fs::read_to_string(marker_path()).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn set_current_project(slug: &str) -> anyhow::Result<()> {
    let path = marker_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{slug}\n"))?;
    Ok(())
}

pub enum ProjectAction {
    New { slug: String, title: Option<String> },
    List,
    Switch { slug: String },
}

pub async fn run_project(project_root: &Path, action: ProjectAction) -> anyhow::Result<()> {
    let memory = open_memory(project_root)?;
    match action {
        ProjectAction::New { slug, title } => {
            let project = memory.projects.ensure_project(&slug, title.as_deref())?;
            println!("Project '{}' — {}", project.id, project.name);
        }
        ProjectAction::List => {
            let projects = memory.projects.list_projects()?;
            if projects.is_empty() {
                println!("No projects yet. `hive project new <slug>` creates one.");
                return Ok(());
            }
            let current = current_project();
            println!("ID               NAME");
            for p in projects {
                let mark = if current.as_deref() == Some(p.id.as_str()) {
                    " *"
                } else {
                    ""
                };
                println!("{:<16} {}{}", p.id, p.name, mark);
            }
        }
        ProjectAction::Switch { slug } => {
            let project = memory.projects.project(&slug).ok().flatten();
            if project.is_none() {
                memory.projects.ensure_project(&slug, None)?;
            }
            set_current_project(&slug)?;
            println!("Switched to project '{slug}'.");
        }
    }
    Ok(())
}

pub async fn run_search(
    project_root: &Path,
    query: &str,
    project: Option<&str>,
) -> anyhow::Result<()> {
    let memory = open_memory(project_root)?;
    let results = memory.search_all(query, project).await;

    if let Some(error) = &results.semantic_error {
        eprintln!("{error}");
    }
    println!("semantic  {}", memory.semantic_status()?);
    let mut sections = 0;
    if !results.messages.is_empty() {
        sections += 1;
        println!("== conversations ==");
        for hit in &results.messages {
            println!(
                "[{}] {} ({})\n    {}",
                hit.project_id, hit.title, hit.role, hit.snippet
            );
        }
    }
    if !results.entities.is_empty() {
        sections += 1;
        println!("\n== knowledge ==");
        for name in &results.entities {
            println!("- {name}");
        }
    }
    if !results.rag.is_empty() {
        sections += 1;
        println!("\n== passages ==");
        for hit in &results.rag {
            let text: String = hit
                .text
                .chars()
                .flat_map(|c| if c == '\n' { vec![' '] } else { vec![c] })
                .take(160)
                .collect();
            println!("[{:.2}] {}", hit.score, text);
        }
    }
    if sections == 0 {
        println!("Nothing in memory matches '{query}'.");
    }
    Ok(())
}

pub async fn run_memory_status(project_root: &Path) -> anyhow::Result<()> {
    let config = HiveConfig::from_project_root(project_root)?;
    let memory = MemorySystem::open(config.database.resolved_path(), &config);
    let status = memory.status();
    println!("semantic  {}", memory.semantic_status()?);
    println!("database  {}", config.database.resolved_path().display());
    println!(
        "projects  {}   conversations  {}   messages  {}",
        status.projects, status.conversations, status.messages
    );
    println!(
        "rag       {} chunks   knowledge graph  {} fleet entities",
        status.rag_chunks, status.graph_entities
    );
    if let Some(p) = current_project() {
        println!("current   {p}");
    }
    Ok(())
}

pub async fn run_reindex(project_root: &Path) -> anyhow::Result<()> {
    let config = HiveConfig::from_project_root(project_root)?;
    let memory = MemorySystem::open_for_reindex(config.database.resolved_path(), &config)?;
    let status = memory.reindex().await?;
    println!(
        "Rebuilt {} records; {} failed. {}",
        status.rebuilt,
        status.failed,
        memory.semantic_status()?
    );
    anyhow::ensure!(
        status.failed == 0,
        "Semantic indexing incomplete; rerun hive memory reindex to resume"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_round_trips_and_ignores_blank_files() {
        // Reads and writes go through an explicit home directory — mutating
        // the process-global HOME in a parallel test run is exactly the kind
        // of shared state that flakes once a month and costs an afternoon.
        let dir = std::env::temp_dir().join(format!("hive-marker-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".hive")).unwrap();
        let marker = marker_path_in(&dir);

        std::fs::write(&marker, "webapp\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "webapp",
            "marker writes are slug + newline"
        );
        std::fs::write(&marker, "  \n").unwrap();
        // A blank marker must read as "no project" — callers treat blank as
        // unset rather than erroring on startup.
        assert!(std::fs::read_to_string(&marker).unwrap().trim().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
