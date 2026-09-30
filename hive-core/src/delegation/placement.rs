//! Build placement must leave enough space for compiler and frontend caches.
use super::{coordination::AcceptanceCheck, Assignment};
use crate::memory::graph::{entity_id, KnowledgeGraph};

pub const MIN_BUILD_DISK_GIB: f64 = 10.0;

pub fn validate_disk(a: &Assignment, graph: &KnowledgeGraph) -> anyhow::Result<()> {
    let text = format!(
        "{} {} {} {}",
        a.objective,
        a.acceptance_criteria.join(" "),
        a.acceptance_checks
            .iter()
            .filter_map(|check| match check {
                AcceptanceCheck::Command { argv, .. } => Some(argv.join(" ")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        a.required_capabilities.join(" ")
    )
    .to_lowercase();
    let build = [
        "rust",
        "cargo ",
        "frontend",
        "front-end",
        "npm ",
        "pnpm ",
        "yarn ",
        "next.js",
        "vite",
    ]
    .iter()
    .any(|word| text.contains(word));
    if build {
        if let Some(free) = graph
            .entity(&entity_id("machine", &a.device))?
            .and_then(|m| m.attr_f64("disk_free_gb"))
        {
            anyhow::ensure!(
                free.is_finite() && free >= MIN_BUILD_DISK_GIB,
                "Device {} has {:.1} GiB free disk; Rust/frontend builds require at least {} GiB",
                a.device,
                free,
                MIN_BUILD_DISK_GIB
            );
        }
    }
    Ok(())
}
