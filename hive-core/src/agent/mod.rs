//! Master agent — ReAct-style reasoning loop for task planning and execution.

pub mod planner;
pub mod run;

use std::sync::Arc;

use hive_common::config::WatchdogConfig;
use hive_common::{AgentResponse, TaskAssignment, TaskCommand};
use tracing::{info, warn};

use crate::llm::LlmRouter;
use crate::memory::{machines, MemorySystem};
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;
use crate::watchdog::Watchdog;
use crate::workers::WorkerPool;
use planner::Planner;
use run::{
    assess_command, Approvals, Decision, PlannedRun, PlannedStep, RunResult, StepOutcome,
    StepStatus, StepTarget,
};

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
    pub fn new(
        llm: LlmRouter,
        workers: WorkerPool,
        skills: SkillRegistry,
        memory: MemorySystem,
    ) -> Self {
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
                                notes.push(format!(
                                    "'{}' delegation FAILED: {e}",
                                    subtask.description
                                ));
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
    // ---------------------------------------------------------------- planning

    /// Phase one: classify, route, and plan — without running anything.
    ///
    /// Every command is checked against the watchdog's Tier-1 rules here, so
    /// the caller can show which steps will need approval *before* the first
    /// one executes. Pair with [`MasterAgent::execute_run`].
    pub async fn plan_run(
        &self,
        user_input: &str,
        project_id: Option<&str>,
    ) -> anyhow::Result<PlannedRun> {
        info!("Planning run for: {}", user_input);

        if let Some(pid) = project_id {
            let _context = self.memory.retrieve_context(pid, user_input).await?;
        }

        let complexity = self.llm.classify_complexity(user_input).await?;
        let provider = complexity.recommended_provider();
        info!("Task complexity: {complexity}, routing to {provider}");

        let plan = self.planner.plan(&self.llm, user_input, complexity).await?;

        let mut steps = Vec::new();
        for subtask in &plan.subtasks {
            let target = if subtask.requires_remote {
                match self.choose_worker(&["supervised-sessions"]) {
                    Some(worker) => StepTarget::Remote {
                        worker: worker.info.name.clone(),
                    },
                    // Named so the UI can say *which* worker was wanted even
                    // when none is available.
                    None => StepTarget::Remote {
                        worker: String::new(),
                    },
                }
            } else {
                StepTarget::Local
            };

            for command in &subtask.commands {
                steps.push(PlannedStep {
                    id: steps.len(),
                    description: subtask.description.clone(),
                    command: command.clone(),
                    // Remote commands are supervised live by the watchdog once
                    // delegated; the pre-flight gate is for local execution,
                    // which has no such supervision.
                    risk: match target {
                        StepTarget::Local => assess_command(&self.watchdog, command),
                        StepTarget::Remote { .. } => None,
                    },
                    target: target.clone(),
                });
            }

            if subtask.commands.is_empty() {
                steps.push(PlannedStep {
                    id: steps.len(),
                    description: subtask.description.clone(),
                    command: String::new(),
                    target: target.clone(),
                    risk: None,
                });
            }
        }

        Ok(PlannedRun {
            id: format!("run-{}", uuid::Uuid::new_v4()),
            user_input: user_input.to_string(),
            summary: plan.summary,
            complexity,
            routed_provider: provider,
            provider: plan.provider_used,
            steps,
        })
    }

    /// Phase two: execute the steps the caller has cleared.
    ///
    /// Gated steps without an explicit approval come back as
    /// [`StepStatus::AwaitingApproval`] and are simply not run — call again
    /// with an updated [`Approvals`] to continue.
    pub async fn execute_run(&self, plan: &PlannedRun, approvals: &Approvals) -> RunResult {
        let mut outcomes = Vec::new();
        let mut sessions = Vec::new();
        let mut awaiting = Vec::new();

        for step in &plan.steps {
            if step.command.is_empty() {
                outcomes.push(StepOutcome {
                    id: step.id,
                    command: String::new(),
                    status: StepStatus::Skipped,
                    output: format!("{} — no commands to run", step.description),
                });
                continue;
            }

            if step.needs_approval() {
                match approvals.decision(step.id) {
                    Decision::Pending => {
                        awaiting.push(step.id);
                        outcomes.push(StepOutcome {
                            id: step.id,
                            command: step.command.clone(),
                            status: StepStatus::AwaitingApproval,
                            output: step
                                .risk
                                .as_ref()
                                .map(|r| r.reason.clone())
                                .unwrap_or_default(),
                        });
                        continue;
                    }
                    Decision::Denied => {
                        outcomes.push(StepOutcome {
                            id: step.id,
                            command: step.command.clone(),
                            status: StepStatus::Denied,
                            output: "Rejected by the user; not run.".into(),
                        });
                        continue;
                    }
                    Decision::Approved => {
                        warn!(command = %step.command, "running a Tier-1 flagged command on user approval");
                    }
                }
            }

            match &step.target {
                StepTarget::Local => match self.tools.run_shell(&step.command).await {
                    Ok(output) => outcomes.push(StepOutcome {
                        id: step.id,
                        command: step.command.clone(),
                        status: StepStatus::Executed,
                        output,
                    }),
                    Err(e) => outcomes.push(StepOutcome {
                        id: step.id,
                        command: step.command.clone(),
                        status: StepStatus::Failed,
                        output: e.to_string(),
                    }),
                },
                StepTarget::Remote { worker } => {
                    let selected = self
                        .workers
                        .workers
                        .iter()
                        .find(|w| &w.info.name == worker)
                        .or_else(|| self.workers.select_worker());

                    let Some(node) = selected else {
                        outcomes.push(StepOutcome {
                            id: step.id,
                            command: step.command.clone(),
                            status: StepStatus::Failed,
                            output: "No worker is online to take this step.".into(),
                        });
                        continue;
                    };

                    let assignment = TaskAssignment::new(
                        step.description.clone(),
                        vec![TaskCommand::new(step.command.clone())],
                        format!("hive-{}", uuid::Uuid::new_v4()),
                    );

                    match self
                        .workers
                        .delegate(node, assignment, self.llm.clone(), self.watchdog.clone())
                        .await
                    {
                        Ok(session) => {
                            outcomes.push(StepOutcome {
                                id: step.id,
                                command: step.command.clone(),
                                status: StepStatus::Delegated,
                                output: format!(
                                    "Delegated to '{}' as tmux session '{}'.",
                                    node.info.name, session.session_name
                                ),
                            });
                            sessions.push(session);
                        }
                        Err(e) => outcomes.push(StepOutcome {
                            id: step.id,
                            command: step.command.clone(),
                            status: StepStatus::Failed,
                            output: format!("Delegation failed: {e}"),
                        }),
                    }
                }
            }
        }

        RunResult {
            run_id: plan.id.clone(),
            summary: plan.summary.clone(),
            complexity: plan.complexity.clone(),
            provider: plan.provider.clone(),
            outcomes,
            sessions,
            awaiting_approval: awaiting,
        }
    }

    // ----------------------------------------------------------- machine graph

    /// Pick a worker that has every one of `capabilities`, consulting the
    /// machine knowledge graph.
    ///
    /// With one worker this is barely more than `select_worker`. It exists
    /// because the graph is where placement decisions are meant to live once
    /// there is more than one machine to choose between — the query stays the
    /// same, the answer gets more interesting.
    pub fn choose_worker(&self, capabilities: &[&str]) -> Option<&crate::workers::WorkerNode> {
        let ranked = machines::machines_with_capabilities(&self.memory.graph, capabilities)
            .unwrap_or_default();

        ranked
            .iter()
            .find_map(|m| {
                self.workers.workers.iter().find(|w| {
                    w.info.name == m.name && w.status == hive_common::WorkerStatus::Online
                })
            })
            .or_else(|| self.workers.select_worker())
    }

    /// Re-probe every machine (the master and all configured workers) and
    /// refresh the knowledge graph.
    ///
    /// Workers are probed concurrently: an unreachable one costs a connect
    /// timeout, and serializing those would make startup scale with the number
    /// of offline machines.
    pub async fn refresh_machine_graph(&self, master_name: &str) -> anyhow::Result<usize> {
        let local = machines::probe_local(master_name).await;
        machines::project_into_graph(&self.memory.graph, &local)?;

        let probes = self.workers.workers.iter().map(|w| {
            let name = w.info.name.clone();
            let target = w.info.ssh_target();
            let tags = w.info.tags.clone();
            async move { machines::probe_remote(&name, &target, tags).await }
        });

        let results = futures::future::join_all(probes).await;
        let mut count = 1;
        for facts in results {
            machines::project_into_graph(&self.memory.graph, &facts)?;
            count += 1;
        }

        info!(machines = count, "machine knowledge graph refreshed");
        Ok(count)
    }
}
