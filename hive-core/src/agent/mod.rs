//! Master agent — ReAct-style reasoning loop for task planning and execution.

pub mod planner;

use hive_common::AgentResponse;
use tracing::{info, warn};

use crate::llm::LlmRouter;
use crate::memory::MemorySystem;
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;
use crate::workers::WorkerPool;
use planner::Planner;

/// The master agent — central intelligence of the Hive system.
///
/// Receives user requests, plans tasks, classifies complexity,
/// routes to appropriate AI providers, and delegates to workers.
pub struct MasterAgent {
    /// Multi-provider LLM router.
    pub llm: LlmRouter,
    /// Pool of worker machines for task delegation.
    pub workers: WorkerPool,
    /// Skill registry for custom tool definitions.
    pub skills: SkillRegistry,
    /// Memory system for project-scoped conversation history.
    pub memory: MemorySystem,
    /// Local tool registry (shell, file ops, git).
    pub tools: ToolRegistry,
    planner: Planner,
}

impl MasterAgent {
    /// Create a new master agent with all subsystems.
    pub fn new(
        llm: LlmRouter,
        workers: WorkerPool,
        skills: SkillRegistry,
        memory: MemorySystem,
    ) -> Self {
        Self {
            llm,
            workers,
            skills,
            memory,
            tools: ToolRegistry::new(),
            planner: Planner::new(),
        }
    }

    /// Handle a user request: plan, classify, route, and execute/delegate.
    ///
    /// Local subtasks run through the tool registry immediately. Subtasks
    /// that request a remote worker are noted but not yet delegated — SSH
    /// delegation and tmux session creation land in Phase 3/4. There is no
    /// safety watchdog yet (Phase 10): commands the plan produces run
    /// without confirmation, so treat this like any other unattended
    /// automation until that lands.
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

        // 4. Execute local subtasks now; note remote ones for Phase 3 delegation
        let mut notes = Vec::new();
        for subtask in &plan.subtasks {
            if subtask.requires_remote {
                match self.workers.select_worker() {
                    Some(worker) => notes.push(format!(
                        "'{}' would delegate to worker '{}' — SSH delegation lands in Phase 3",
                        subtask.description, worker.info.name
                    )),
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

        // 5. Return summary with tmux session access info (empty until Phase 3/4)
        Ok(AgentResponse {
            summary,
            sessions: vec![],
            provider_used: provider,
            complexity,
        })
    }
}
