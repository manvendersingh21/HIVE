//! Hive CLI — command-line interface to the Hive distributed agent system.

use std::path::{Path, PathBuf};

mod approval;
mod master;

use approval::GatePolicy;
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
    Chat {
        /// Run in this process instead of using the master daemon.
        #[arg(long)]
        local: bool,
    },
    /// Submit a task directly.
    Task {
        #[arg(short, long)]
        description: String,
        /// Approve commands the safety watchdog flags, without asking.
        /// For non-interactive use, where a prompt would hang.
        #[arg(long)]
        yes: bool,
        /// Skip every flagged command instead of asking.
        #[arg(long, conflicts_with = "yes")]
        deny_flagged: bool,
        /// Run in this process instead of submitting to the master.
        ///
        /// Anything delegated to a worker is then supervised only until this
        /// process exits — which for `hive task` is almost immediately.
        #[arg(long)]
        local: bool,
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
        Commands::Chat { local } => run_chat(&cli.project_root, local).await,
        Commands::Task {
            description,
            yes,
            deny_flagged,
            local,
        } => {
            let policy = if yes {
                GatePolicy::AssumeYes
            } else if deny_flagged {
                GatePolicy::DenyAll
            } else {
                GatePolicy::Prompt
            };
            run_task(&cli.project_root, &description, policy, local).await
        }
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
    let workers_config =
        WorkersConfig::from_project_root(project_root).unwrap_or(WorkersConfig { workers: vec![] });

    let llm = LlmRouter::from_config(&config.llm);
    let workers = WorkerPool::new(workers_config.workers);
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

/// Submit a task, preferring the long-lived master.
///
/// Two problems are solved by routing through the master rather than doing the
/// work here:
///
/// * **Supervision outlives the command.** `WorkerPool::delegate` spawns its
///   watchdog as a task on the *caller's* runtime. In-process, `hive task`
///   exits moments later and that supervisor is aborted, leaving the remote
///   tmux session running unwatched. In the master — a daemon under
///   launchd/systemd — it runs for the session's whole life.
/// * **The safety gate applies either way.** Commands the watchdog's Tier-1
///   rules flag stop and ask before running, the same as in the web UI.
async fn run_task(
    project_root: &Path,
    description: &str,
    policy: GatePolicy,
    force_local: bool,
) -> anyhow::Result<()> {
    if !force_local {
        let url = master::MasterClient::default_url();
        if let Some(client) = master::MasterClient::connect(&url).await? {
            println!("Submitting to the master at {url}\n");
            return run_task_via_master(&client, description, policy).await;
        }
        println!(
            "No master reachable at {url} — running in this process.\n\
             WARNING: anything delegated to a worker will be supervised only until this\n\
             command exits, which is almost immediately. Start hive-web to get durable\n\
             supervision, or pass --local to silence this.\n"
        );
    }
    run_task_locally(project_root, description, policy).await
}

async fn run_task_via_master(
    client: &master::MasterClient,
    description: &str,
    policy: GatePolicy,
) -> anyhow::Result<()> {
    let reply = client.submit(description, None).await?;
    approval::print_plan(&reply.run);
    approval::print_outcomes(&reply.result);

    if reply.result.is_complete() {
        return Ok(());
    }

    let (approved, denied) = approval::decide(&reply.run, &reply.result, policy);
    let reply = client.approve(&reply.result.run_id, approved, denied).await?;
    approval::print_outcomes(&reply.result);
    Ok(())
}

/// In-process fallback, used when no master is running.
///
/// Still gated: the plan is built first, flagged steps are held, and only what
/// the user clears is executed.
async fn run_task_locally(
    project_root: &Path,
    description: &str,
    policy: GatePolicy,
) -> anyhow::Result<()> {
    let agent = build_agent(project_root).await?;
    run_request_on_agent(&agent, description, policy).await
}

/// Interactive chat.
///
/// Prefers the master for the same reasons as `hive task`. A long chat session
/// does supervise its own delegations while it is open, but it still ends when
/// you type `exit` — the master does not.
async fn run_chat(project_root: &Path, force_local: bool) -> anyhow::Result<()> {
    use std::io::{self, Write};

    let client = if force_local {
        None
    } else {
        let url = master::MasterClient::default_url();
        match master::MasterClient::connect(&url).await? {
            Some(client) => {
                println!("🐝 Hive — connected to the master at {url}.");
                Some(client)
            }
            None => {
                println!(
                    "🐝 Hive — no master reachable; running in this process.\n\
                     Delegated sessions lose their watchdog when you exit."
                );
                None
            }
        }
    };

    let agent = match &client {
        Some(_) => None,
        None => Some(build_agent(project_root).await?),
    };
    println!("Type a task, or 'exit' to quit.");

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

        // Prompting is right here: a chat session is interactive by definition.
        let outcome = match (&client, &agent) {
            (Some(client), _) => run_task_via_master(client, input, GatePolicy::Prompt).await,
            (None, Some(agent)) => {
                run_request_on_agent(agent, input, GatePolicy::Prompt).await
            }
            (None, None) => unreachable!("one of client or agent is always built"),
        };
        if let Err(e) = outcome {
            eprintln!("Error: {e}");
        }
    }

    Ok(())
}

/// One gated request against an in-process agent.
async fn run_request_on_agent(
    agent: &MasterAgent,
    description: &str,
    policy: GatePolicy,
) -> anyhow::Result<()> {
    use hive_core::agent::run::Approvals;

    let plan = agent.plan_run(description, None).await?;
    approval::print_plan(&plan);

    let result = agent.execute_run(&plan, &Approvals::none()).await;
    approval::print_outcomes(&result);
    if result.is_complete() {
        return Ok(());
    }

    let (approved, denied) = approval::decide(&plan, &result, policy);
    let mut approvals = Approvals::none();
    for id in approved {
        approvals.approve(id);
    }
    for id in denied {
        approvals.deny(id);
    }
    let result = agent.execute_run(&plan, &approvals).await;
    approval::print_outcomes(&result);
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
