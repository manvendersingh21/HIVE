//! Diagnose real placement without executing any planned commands.
//! HIVE_PLAN_CHECK_DB must point to a disposable copy of the master database.
use hive_common::config::{HiveConfig, WorkersConfig};
use hive_core::{
    agent::MasterAgent, llm::LlmRouter, memory::MemorySystem, skills::SkillRegistry,
    workers::WorkerPool,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::path::Path::new(".");
    let config = HiveConfig::from_project_root(root)?;
    let workers = WorkersConfig::from_project_root(root)?;
    let db = std::env::var("HIVE_PLAN_CHECK_DB")?;
    let memory = MemorySystem::open_for_reindex(db, &config)?;
    let agent = MasterAgent::new(
        LlmRouter::from_config(&config.llm),
        WorkerPool::new(workers.workers),
        SkillRegistry::new(),
        memory,
    )
    .with_master_name("manus-mac-mini");
    let request = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let plan = agent.plan_run(&request, None).await?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}
