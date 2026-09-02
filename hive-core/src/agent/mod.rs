//! Master agent — ReAct-style reasoning loop for task planning and execution.

pub mod planner;

use hive_common::AgentResponse;
use tracing::info;

use crate::llm::LlmRouter;
use crate::memory::MemorySystem;
use crate::skills::SkillRegistry;
use crate::workers::WorkerPool;

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
        }
    }

    /// Handle a user request: plan, classify, route, and delegate.
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
        info!("Task complexity: {}", complexity);

        // 3. Plan the task using the appropriate provider
        let provider = complexity.recommended_provider();
        info!("Routing to provider: {}", provider);

        // TODO: Full implementation — plan task, decompose into subtasks,
        // delegate to workers, create tmux sessions, return response.

        let response = AgentResponse {
            summary: format!("Task received: {}", user_input),
            sessions: vec![],
            provider_used: provider,
            complexity,
        };

        Ok(response)
    }
}
