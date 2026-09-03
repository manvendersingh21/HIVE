//! Master agent — ReAct-style reasoning loop for task planning and execution.

pub mod planner;

use std::sync::Arc;

use hive_common::config::WatchdogConfig;
use hive_common::{AgentResponse, TaskAssignment, TaskCommand};
use tracing::{info, warn};

use crate::llm::LlmRouter;
use crate::memory::MemorySystem;
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;
use crate::watchdog::Watchdog;
use crate::workers::WorkerPool;
use planner::Planner;

/// The master agent — central intelligence of the Hive system.
///
/// Receives user requests, plans tasks, classifies complexity,
/// routes to appropriate AI providers, and delegates to workers.
pub struct MasterAgent {
    /// Multi-provider LLM router.
    pub llm: Arc<LlmRouter>,
    /// Pool of worker machines for task delegation.
    pub workers: WorkerPool,
    /// Skill registry for custom tool definitions.
    pub skills: SkillRegistry,
    /// Memory system for project-scoped conversation history.
    pub memory: MemorySystem,
    /// Local tool registry (shell, file ops, git).
    pub tools: ToolRegistry,
    /// Safety watchdog applied to delegated (remote) sessions.
    pub watchdog: Arc<Watchdog>,
    planner: Planner,
}

impl MasterAgent {
    /// Create a new master agent with all subsystems, using the default
    /// watchdog configuration. See [`MasterAgent::with_watchdog_config`] to
    /// use a configured one (e.g. from `hive.toml`).
    pub fn new(llm: LlmRouter, workers: WorkerPool, skills: SkillRegistry, memory: MemorySystem) -> Self {
        Self::with_watchdog_config(llm, workers, skills, memory, WatchdogConfig::default())
    }

    /// Create a new master agent with an explicit watchdog configuration.
    pub fn with_watchdog_config(
        llm: LlmRouter,
        workers: WorkerPool,
        skills: SkillRegistry,
        memory: MemorySystem,
        watchdog_config: WatchdogConfig,
    ) -> Self {
        let watchdog = Watchdog::from_config(watchdog_config).unwrap_or_else(|e| {
            tracing::warn!("Invalid watchdog config ({e}), falling back to built-in defaults");
            Watchdog::new()
        });

        Self {
            llm: Arc::new(llm),
            workers,
            skills,
            memory,
            tools: ToolRegistry::new(),
            watchdog: Arc::new(watchdog),
            planner: Planner::new(),
        }
    }

    /// Handle a user request: plan, classify, route, and execute/delegate.
    ///
    /// Local subtasks run through the tool registry immediately. Subtasks
    /// that request a remote worker are delegated to a supervised tmux
    /// session on the least-loaded online worker — watched by the
    /// watchdog's Tier-1 (regex) and Tier-2 (periodic LLM review) checks,
    /// which pause (not kill) the session and leave a reattach command in
    /// the logs if something looks wrong. There is still no incident
    /// queue or push notification (Phase 10); watch `tracing` output for
    /// `WATCHDOG INCIDENT` lines.
    pub async fn handle_request(
        &self,
        user_input: &str,
        project_id: Option<&str>,
    ) -> anyhow::Result<AgentResponse> {
        info!("Handling user request: {}", user_input);

        // 1. Retrieve relevant context from memory (if project-scoped)
        let _context = if let Some(pid) = project_id {
            info!("Loading project context for '{}'", pid);
            Some(self.memory.retrieve_context(pid, user_input).await?)
        } else {
            None
        };

        // 2. Classify task complexity
        let complexity = self.llm.classify_complexity(user_input).await?;
        let provider = complexity.recommended_provider();
        info!("Task complexity: {complexity}, routing to {provider}");

        // 3. Plan: decompose into subtasks using the routed provider
        let plan = self.planner.plan(&self.llm, user_input, complexity).await?;
        info!(
            "Plan: {} ({} subtask(s))",
            plan.summary,
            plan.subtasks.len()
        );

        // 4. Execute local subtasks now; delegate remote ones to a worker
        let mut notes = Vec::new();
        let mut sessions = Vec::new();
        for subtask in &plan.subtasks {
            if subtask.requires_remote {
                match self.workers.select_worker() {
                    Some(worker) => {
                        let assignment = TaskAssignment::new(
                            subtask.description.clone(),
                            subtask
                                .commands
                                .iter()
                                .map(|c| TaskCommand::new(c.clone()))
                                .collect(),
                            format!("hive-{}", uuid::Uuid::new_v4()),
                        );
                        let assignment = if let Some(behavior) = &subtask.expected_behavior {
                            let mut a = assignment;
                            a.expected_behavior = Some(behavior.clone());
                            a
                        } else {
                            assignment
                        };

                        match self
                            .workers
                            .delegate(worker, assignment, self.llm.clone(), self.watchdog.clone())
                            .await
                        {
                            Ok(session) => {
                                notes.push(format!(
                                    "'{}' delegated to worker '{}' as tmux session '{}'",
                                    subtask.description, worker.info.name, session.session_name
                                ));
                                sessions.push(session);
                            }
                            Err(e) => {
                                warn!("Delegation failed for '{}': {e}", subtask.description);
                                notes.push(format!("'{}' delegation FAILED: {e}", subtask.description));
                            }
                        }
                    }
                    None => notes.push(format!(
                        "'{}' requested a remote worker but none are online",
                        subtask.description
                    )),
                }
                continue;
            }

            if subtask.commands.is_empty() {
                notes.push(format!("'{}' — no commands to run", subtask.description));
                continue;
            }

            for command in &subtask.commands {
                match self.tools.run_shell(command).await {
                    Ok(output) => {
                        info!("Ran `{command}`");
                        notes.push(format!("$ {command}\n{output}"));
                    }
                    Err(e) => {
                        warn!("Command failed: `{command}`: {e}");
                        notes.push(format!("$ {command}\nFAILED: {e}"));
                    }
                }
            }
        }

        let summary = if notes.is_empty() {
            plan.summary
        } else {
            format!("{}\n\n{}", plan.summary, notes.join("\n\n"))
        };

        // 5. Return summary with tmux session access info for anything delegated
        Ok(AgentResponse {
            summary,
            sessions,
            provider_used: provider,
            complexity,
        })
    }
}
