//! Hive CLI — command-line interface to the Hive distributed agent system.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use hive_common::config::WorkersConfig;
use hive_common::HiveConfig;
use hive_core::agent::MasterAgent;
use hive_core::llm::LlmRouter;
use hive_core::memory::MemorySystem;
use hive_core::skills::SkillRegistry;
use hive_core::workers::WorkerPool;

#[derive(Parser)]
#[command(name = "hive", version, about = "Distributed agentic task system")]
struct Cli {
    /// Path to the project root (contains config/hive.toml and config/workers.toml).
    #[arg(long, global = true, default_value = ".")]
    project_root: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start interactive chat with the master agent.
    Chat,
    /// Submit a task directly.
    Task {
        #[arg(short, long)]
        description: String,
    },
    /// List active sessions across all workers.
    Sessions,
    /// Attach to a specific tmux session (via local terminal).
    Attach { session_id: String },
    /// Manage worker machines.
    Workers {
        #[command(subcommand)]
        action: WorkerAction,
    },
    /// Manage skills.
    Skills {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Fine-tuning data management.
    Finetune {
        #[command(subcommand)]
        action: FinetuneAction,
    },
    /// Start the web terminal server.
    Serve {
        #[arg(short, long, default_value = "0.0.0.0:8080")]
        bind: String,
    },
}

#[derive(Subcommand)]
enum WorkerAction {
    /// List configured workers.
    List,
}

#[derive(Subcommand)]
enum SkillAction {
    /// List loaded skills.
    List,
}

#[derive(Subcommand)]
enum FinetuneAction {
    /// Export collected training data.
    Export {
        #[arg(long, default_value = "sharegpt")]
        format: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hive_cli=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Chat => run_chat(&cli.project_root).await,
        Commands::Task { description } => run_task(&cli.project_root, &description).await,
        Commands::Sessions => {
            println!("No sessions yet — worker delegation is not implemented (Phase 3/4).");
            Ok(())
        }
        Commands::Attach { session_id } => {
            println!(
                "Cannot attach to '{session_id}' yet — the tmux/SSH bridge is not implemented (Phase 5)."
            );
            Ok(())
        }
        Commands::Workers { action } => match action {
            WorkerAction::List => run_workers_list(&cli.project_root),
        },
        Commands::Skills { action } => match action {
            SkillAction::List => {
                println!("No skills loaded yet — the skill loader is not implemented (Phase 7).");
                Ok(())
            }
        },
        Commands::Finetune { action } => match action {
            FinetuneAction::Export { format, output } => {
                println!(
                    "Fine-tuning export is not implemented yet (Phase 8): format={format}, output={}",
                    output.display()
                );
                Ok(())
            }
        },
        Commands::Serve { bind } => {
            println!(
                "hive-web is a separate binary. Run it directly:\n  HIVE_WEB_ADDR={bind} cargo run --bin hive-web"
            );
            Ok(())
        }
    }
}

/// Build a `MasterAgent` from the project's config, with worker health
/// checked live over SSH (so `select_worker`/delegation reflect reality
/// instead of every worker booting `Offline`).
async fn build_agent(project_root: &Path) -> anyhow::Result<MasterAgent> {
    let config = HiveConfig::from_project_root(project_root)?;
    let workers_config = WorkersConfig::from_project_root(project_root).unwrap_or(WorkersConfig { workers: vec![] });

    let llm = LlmRouter::from_config(&config.llm);
    let mut workers = WorkerPool::new(workers_config.workers);
    workers.refresh_health().await;
    let skills = SkillRegistry::new();
    let memory = MemorySystem::new();

    Ok(MasterAgent::with_watchdog_config(
        llm,
        workers,
        skills,
        memory,
        config.watchdog,
    ))
}

async fn run_task(project_root: &Path, description: &str) -> anyhow::Result<()> {
    let agent = build_agent(project_root).await?;
    let response = agent.handle_request(description, None).await?;

    println!("Summary: {}", response.summary);
    println!("Provider: {}", response.provider_used);
    println!("Complexity: {}", response.complexity);
    if response.sessions.is_empty() {
        println!("No sessions created (plan had no remote subtasks, or no worker was online).");
    } else {
        for session in &response.sessions {
            println!(
                "Session '{}' on worker '{}': {}",
                session.session_name, session.worker_name, session.state
            );
        }
    }
    Ok(())
}

async fn run_chat(project_root: &Path) -> anyhow::Result<()> {
    use std::io::{self, Write};

    let agent = build_agent(project_root).await?;
    println!("🐝 Hive Agent Ready. Type a task, or 'exit' to quit.");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break; // EOF
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        match agent.handle_request(input, None).await {
            Ok(response) => {
                println!("Summary: {}", response.summary);
                println!(
                    "Provider: {} ({})",
                    response.provider_used, response.complexity
                );
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    Ok(())
}

fn run_workers_list(project_root: &Path) -> anyhow::Result<()> {
    let config = WorkersConfig::from_project_root(project_root)?;
    if config.workers.is_empty() {
        println!("No workers configured.");
        return Ok(());
    }
    println!("{:<12} {:<20} {:<10} TAGS", "NAME", "HOST", "USER");
    for w in &config.workers {
        println!("{:<12} {:<20} {:<10} {:?}", w.name, w.host, w.user, w.tags);
    }
    Ok(())
}
