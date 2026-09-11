//! Master agent — ReAct-style reasoning loop for task planning and execution.

pub mod planner;
pub mod run;
pub mod workflow;

use std::sync::Arc;

use hive_common::config::WatchdogConfig;
use hive_common::{AgentResponse, SafetyAnalysis, Severity, TaskAssignment, TaskCommand};
use tracing::{info, warn};

use crate::llm::LlmRouter;
use crate::memory::{machines, MemorySystem};
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;
use crate::watchdog::interceptor::Interceptor;
use crate::watchdog::Watchdog;
use crate::workers::WorkerPool;
use planner::{FleetContext, Planner};
use run::{
    assess_command_with_interceptor, Approvals, Decision, PlannedRun, PlannedStep, RunResult,
    StepOutcome, StepStatus, StepTarget,
};

/// The capability every supervised remote subtask needs, whatever the work is.
///
/// Anything a caller asks for *beyond* this came from the planner because the
/// task genuinely needs it, and must not be silently substituted away — see
/// [`MasterAgent::choose_worker`].
const BASELINE_CAPABILITY: &str = "supervised-sessions";

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
    /// High-risk command interceptor — extends the watchdog with
    /// bulk-deletion patterns and diff-threshold analysis.
    pub interceptor: Interceptor,
    /// How this machine is named in the machine knowledge graph. Local
    /// subtasks run here, so the planner is told its OS by this name.
    master_name: String,
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
        let max_files = watchdog_config.max_files;
        let max_lines_deleted = watchdog_config.max_lines_deleted;

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
            interceptor: Interceptor::new(max_files, max_lines_deleted),
            watchdog: Arc::new(watchdog),
            master_name: default_master_name(),
            planner: Planner::new(),
        }
    }

    /// Set the name this machine is known by in the graph.
    pub fn with_master_name(mut self, name: impl Into<String>) -> Self {
        self.master_name = name.into();
        self
    }

    /// The name this machine is known by in the graph.
    pub fn master_name(&self) -> &str {
        &self.master_name
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

        // 1. Open a memory turn: persist the input under the project and
        //    fetch the context worth injecting. A broken memory degrades to
        //    an empty turn rather than failing the request.
        let turn = match project_id {
            Some(pid) => Some(self.memory.begin_turn(pid, user_input).await),
            None => None,
        };
        let context_block = render_turn_context(turn.as_ref());
        let skill = self.skills.resolve(user_input, &self.llm).await;

        // 2. Classify task complexity
        let complexity = self.llm.classify_complexity(user_input).await?;
        let provider = self
            .llm
            .effective_provider(complexity.recommended_provider());
        info!("Task complexity: {complexity}, routing to {provider}");

        // 3. Plan: decompose into subtasks using the routed provider
        let plan = self
            .planner
            .plan(
                &self.llm,
                user_input,
                complexity,
                &self.fleet_context(),
                context_block.as_deref(),
                skill,
            )
            .await?;
        if let Some(s) = skill {
            info!(skill = %s.name, "skill active for this request");
        }
        info!(
            "Plan: {} ({} subtask(s))",
            plan.summary,
            plan.subtasks.len()
        );

        // Validate every destination before executing any part of the plan.
        let targets = plan
            .subtasks
            .iter()
            .map(|s| self.subtask_target(s))
            .collect::<anyhow::Result<Vec<_>>>()?;

        // 4. Execute local subtasks now; delegate remote ones to a worker
        let mut notes = Vec::new();
        let mut sessions = Vec::new();
        for (subtask, target) in plan.subtasks.iter().zip(targets) {
            if let StepTarget::Remote { worker: name } = target {
                match self
                    .workers
                    .workers
                    .iter()
                    .find(|w| w.info.name == name && w.is_online())
                {
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
                // A confirmation-gated skill cannot run through this path:
                // handle_request has no approval flow, so the commands are
                // refused with a pointer to the surfaces that do gate —
                // same posture as the Tier-1 interceptor below.
                if let Some(s) = &skill {
                    if s.require_confirmation {
                        warn!(skill = %s.name, "skill requires confirmation; refusing in the one-shot path");
                        notes.push(format!(
                            "⚠ SKILL GATE: skill '{}' requires confirmation. \
                             Use `hive task` or the web chat to approve it.\n  $ {command}",
                            s.name
                        ));
                        continue;
                    }
                }
                // Safety gate: check against the interceptor before running.
                // handle_request has no approval flow, so flagged commands are
                // refused outright — better than silent execution.
                if let Some(analysis) =
                    assess_command_with_interceptor(&self.watchdog, &self.interceptor, command)
                {
                    warn!(
                        command = %command,
                        reason = %analysis.reason,
                        "handle_request: blocked by safety interceptor"
                    );
                    notes.push(format!(
                        "⚠ BLOCKED: $ {command}\n  {}\n  \
                         Use `hive task` for an interactive approval flow.",
                        analysis.reason
                    ));
                    // Send iMessage notification for the blocked command
                    let summary = format!("{} — {}", command, analysis.reason);
                    let _ = crate::watchdog::notifier::send_imessage_alert(&summary).await;
                    continue;
                }

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

        // 5b. Close the memory turn: persist the answer, re-index the
        //     conversation, extract knowledge. Best-effort by contract.
        if let Some(turn) = &turn {
            self.memory
                .complete_turn(&turn.conversation_id, &summary)
                .await;
        }

        // 6. Return summary with tmux session access info for anything delegated
        Ok(AgentResponse {
            summary,
            sessions,
            provider_used: plan.provider_used,
            model: plan.model_used,
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

        let turn = match project_id {
            Some(pid) => Some(self.memory.begin_turn(pid, user_input).await),
            None => None,
        };
        let context_block = render_turn_context(turn.as_ref());
        self.plan_with_context(user_input, context_block, turn.map(|t| t.conversation_id))
            .await
    }

    /// Web history is already durably saved by the caller, which also owns the
    /// reply. Keep it separate from the CLI's automatic one-turn persistence.
    pub async fn plan_chat_run(
        &self,
        user_input: &str,
        history: Vec<String>,
    ) -> anyhow::Result<PlannedRun> {
        let context = crate::memory::RetrievedContext {
            recent_messages: history,
            rag_chunks: vec![],
            kg_entities: vec![],
        };
        self.plan_with_context(user_input, Some(context.render()), None)
            .await
    }

    async fn plan_with_context(
        &self,
        user_input: &str,
        context_block: Option<String>,
        conversation_id: Option<String>,
    ) -> anyhow::Result<PlannedRun> {
        let skill = self.skills.resolve(user_input, &self.llm).await;

        let complexity = self.llm.classify_complexity(user_input).await?;
        let provider = self
            .llm
            .effective_provider(complexity.recommended_provider());
        info!("Task complexity: {complexity}, routing to {provider}");

        let plan = self
            .planner
            .plan(
                &self.llm,
                user_input,
                complexity,
                &self.fleet_context(),
                context_block.as_deref(),
                skill,
            )
            .await?;
        if let Some(s) = skill {
            info!(skill = %s.name, "skill active for this request");
        }

        self.materialize_plan(
            user_input,
            plan,
            complexity,
            provider,
            conversation_id,
            skill,
        )
    }

    fn materialize_plan(
        &self,
        user_input: &str,
        plan: planner::TaskPlan,
        complexity: hive_common::Complexity,
        provider: hive_common::AiProvider,
        conversation_id: Option<String>,
        skill: Option<&crate::skills::Skill>,
    ) -> anyhow::Result<PlannedRun> {
        let mut steps = Vec::new();
        for subtask in &plan.subtasks {
            let target = self.subtask_target(subtask)?;

            for file in &subtask.files {
                let command = format!("write_file {}\n{}", file.path, file.content);
                steps.push(PlannedStep {
                    id: steps.len(),
                    description: subtask.description.clone(),
                    command: command.clone(),
                    file: Some(file.clone()),
                    target: target.clone(),
                    risk: assess_command_with_interceptor(
                        &self.watchdog,
                        &self.interceptor,
                        &command,
                    ),
                });
            }
            for command in &subtask.commands {
                steps.push(PlannedStep {
                    id: steps.len(),
                    description: subtask.description.clone(),
                    command: command.clone(),
                    file: None,
                    risk: assess_command_with_interceptor(
                        &self.watchdog,
                        &self.interceptor,
                        command,
                    ),
                    target: target.clone(),
                });
            }

            if subtask.commands.is_empty() && subtask.files.is_empty() {
                steps.push(PlannedStep {
                    id: steps.len(),
                    description: subtask.description.clone(),
                    command: String::new(),
                    file: None,
                    target: target.clone(),
                    risk: None,
                });
            }
        }

        // A confirmation-gated skill marks every local step for approval —
        // even ones the Tier-1 rules would allow. The gate is the existing
        // one (`PlannedStep::needs_approval` → the interactive/web approval
        // flow), not a new execution path; Phase 10 depends on there being
        // no other way around the watchdog.
        if let Some(s) = skill.filter(|s| s.require_confirmation) {
            for step in &mut steps {
                if step.risk.is_none() {
                    step.risk = Some(SafetyAnalysis {
                        is_safe: false,
                        severity: Severity::Low,
                        category: None,
                        reason: format!("skill '{}' requires confirmation before running", s.name),
                        suggested_action: "review".into(),
                    });
                }
            }
        }

        // Explicit configured names in the request take precedence over model
        // metadata. A model sometimes copies the entire fleet into `targets`.
        let named = self.named_request_targets(user_input);
        let destination_names = if named.is_empty() {
            &plan.targets
        } else {
            &named
        };
        let mut targets = Vec::new();
        for name in destination_names {
            let task: planner::SubTask = serde_json::from_value(serde_json::json!({
                "description":"task destination", "target_machine":name, "commands":[]
            }))?;
            let target = self.subtask_target(&task)?;
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        if targets.is_empty() {
            for step in &steps {
                if !targets.contains(&step.target) {
                    targets.push(step.target.clone());
                }
            }
        }
        Ok(PlannedRun {
            phase: plan.phase,
            targets,
            id: format!("run-{}", uuid::Uuid::new_v4()),
            user_input: user_input.to_string(),
            summary: plan.summary,
            complexity,
            routed_provider: provider,
            provider: plan.provider_used,
            model: plan.model_used.clone(),
            steps,
            conversation_id,
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

        let mut halted = false;
        for step in &plan.steps {
            if halted {
                outcomes.push(StepOutcome {
                    id: step.id,
                    command: step.command.clone(),
                    status: StepStatus::Pending,
                    output: "Not run: an earlier step failed or requires review.".into(),
                });
                continue;
            }
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
                        halted = true;
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
                        halted = true;
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

            let command = match &step.file {
                Some(file) => match workflow::file_command(file) {
                    Ok(command) => command,
                    Err(e) => {
                        outcomes.push(StepOutcome {
                            id: step.id,
                            command: step.command.clone(),
                            status: StepStatus::Failed,
                            output: e.to_string(),
                        });
                        halted = true;
                        continue;
                    }
                },
                None => step.command.clone(),
            };
            // Reject malformed shell syntax before creating a remote session.
            let syntax = tokio::process::Command::new("bash")
                .args(["-n", "-c", &command])
                .output()
                .await;
            if let Ok(out) = &syntax {
                if !out.status.success() {
                    outcomes.push(StepOutcome {
                        id: step.id,
                        command: step.command.clone(),
                        status: StepStatus::Failed,
                        output: format!(
                            "Shell syntax rejected before execution: {}",
                            String::from_utf8_lossy(&out.stderr)
                        ),
                    });
                    halted = true;
                    continue;
                }
            }
            match &step.target {
                StepTarget::Local => match self.tools.run_shell(&command).await {
                    Ok(output) => outcomes.push(StepOutcome {
                        id: step.id,
                        command: step.command.clone(),
                        status: if output.starts_with("exit_code: 0\n") {
                            StepStatus::Executed
                        } else {
                            StepStatus::Failed
                        },
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
                    // Honor the machine the plan named, and only that machine.
                    //
                    // Planning already asked the knowledge graph for a host with
                    // the capabilities this step needs, so the name is a
                    // decision, not a hint. Silently substituting the
                    // least-loaded worker undoes that: a CUDA step planned for
                    // the GPU box would run on one without a GPU and fail
                    // confusingly. Only an unnamed target (the graph had no
                    // opinion) falls back to least-loaded.
                    let selected = if worker.is_empty() {
                        self.workers.select_worker()
                    } else {
                        self.workers
                            .workers
                            .iter()
                            .find(|w| &w.info.name == worker && w.is_online())
                    };

                    let Some(node) = selected else {
                        outcomes.push(StepOutcome {
                            id: step.id,
                            command: step.command.clone(),
                            status: StepStatus::Failed,
                            output: if worker.is_empty() {
                                "No worker is online to take this step.".to_string()
                            } else {
                                format!(
                                    "Worker '{worker}' was planned for this step but is not \
                                     online. Not substituting another machine — it was chosen \
                                     for capabilities the others may not have."
                                )
                            },
                        });
                        halted = true;
                        continue;
                    };

                    let assignment = TaskAssignment::new(
                        step.description.clone(),
                        vec![TaskCommand::new(command)],
                        format!("hive-{}", uuid::Uuid::new_v4()),
                    );

                    match self
                        .workers
                        .delegate(node, assignment, self.llm.clone(), self.watchdog.clone())
                        .await
                    {
                        Ok(session) => {
                            match self.workers.wait_for_completion(&session, 120).await {
                                Ok((finished, output)) => {
                                    outcomes.push(StepOutcome {
                                        id: step.id,
                                        command: step.command.clone(),
                                        status: if finished.state
                                            == hive_common::TaskState::Completed
                                        {
                                            StepStatus::Executed
                                        } else {
                                            StepStatus::Failed
                                        },
                                        output,
                                    });
                                    sessions.push(finished);
                                }
                                Err(e) => {
                                    outcomes.push(StepOutcome {
                                        id: step.id,
                                        command: step.command.clone(),
                                        status: StepStatus::Delegated,
                                        output: e.to_string(),
                                    });
                                    sessions.push(session);
                                }
                            }
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
            halted = outcomes
                .last()
                .is_some_and(|o| matches!(o.status, StepStatus::Failed | StepStatus::Delegated));
        }

        let result = RunResult {
            run_id: plan.id.clone(),
            summary: plan.summary.clone(),
            complexity: plan.complexity.clone(),
            provider: plan.provider.clone(),
            outcomes,
            sessions,
            awaiting_approval: awaiting,
        };

        // Close the memory turn once the run is actually finished — not while
        // steps still await approval. A parked plan has answered nothing;
        // persisting it would teach memory that a half-run was the outcome.
        if result.is_complete() {
            if let Some(conv) = plan.conversation_id.as_deref().filter(|c| !c.is_empty()) {
                let assistant = render_run_result(&result);
                self.memory.complete_turn(conv, &assistant).await;
            }
        }

        result
    }

    // ----------------------------------------------------------- machine graph

    /// Render the machine graph for the planner prompt.
    ///
    /// A graph read that fails degrades to empty context rather than failing
    /// the request — a planner without fleet facts is worse, not broken.
    fn fleet_context(&self) -> FleetContext {
        let local_os = self
            .memory
            .graph
            .entity(&format!("machine:{}", self.master_name))
            .ok()
            .flatten()
            .and_then(|m| m.attr_str("os").map(str::to_string))
            .unwrap_or_default();

        FleetContext {
            local_machine: self.master_name.clone(),
            local_os,
            description: format!(
                "{}\nConfigured workers (use these exact target_machine names): {}",
                machines::describe_for_prompt(&self.memory.graph).unwrap_or_default(),
                self.workers
                    .workers
                    .iter()
                    .map(|w| w.info.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn named_request_targets(&self, input: &str) -> Vec<String> {
        let text = input.to_lowercase();
        self.workers
            .workers
            .iter()
            .map(|w| w.info.name.as_str())
            .chain(std::iter::once(self.master_name.as_str()))
            .filter(|name| {
                let name = name.to_lowercase();
                text.match_indices(&name).any(|(start, _)| {
                    let boundary = |c: char| !c.is_alphanumeric() && c != '-' && c != '_';
                    text[..start].chars().next_back().is_none_or(boundary)
                        && text[start + name.len()..]
                            .chars()
                            .next()
                            .is_none_or(boundary)
                })
            })
            .map(str::to_owned)
            .collect()
    }

    fn subtask_target(&self, task: &planner::SubTask) -> anyhow::Result<StepTarget> {
        if let Some(name) = task
            .target_machine
            .as_deref()
            .filter(|name| *name != "auto")
        {
            if name == "local" || name == self.master_name {
                return Ok(StepTarget::Local);
            }
            let worker = self
                .workers
                .workers
                .iter()
                .find(|w| w.info.name == name)
                .ok_or_else(|| {
                    anyhow::anyhow!("Unknown target machine '{name}'; no commands were executed")
                })?;
            // Capabilities are hints for automatic placement, not a veto on
            // an explicitly named destination. The model can invent unrelated
            // requirements, and the graph can be unseeded or temporarily stale.
            // Installation tasks also intentionally target missing software.
            // Execution still requires a healthy SSH/tmux worker and passes
            // through the existing watchdog; never substitute another host.
            // Preserve the destination even when offline; execution reports that
            // failure instead of substituting another online worker.
            return Ok(StepTarget::Remote {
                worker: worker.info.name.clone(),
            });
        }
        if !task.requires_remote && task.target_machine.as_deref() != Some("auto") {
            return Ok(StepTarget::Local);
        }
        let mut needed = vec![BASELINE_CAPABILITY];
        needed.extend(task.required_capabilities.iter().map(String::as_str));
        let worker = self
            .choose_worker(&needed)
            .ok_or_else(|| anyhow::anyhow!("No online worker meets the requested capabilities"))?;
        Ok(StepTarget::Remote {
            worker: worker.info.name.clone(),
        })
    }

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

        let matched = ranked.iter().find_map(|m| {
            self.workers
                .workers
                .iter()
                .find(|w| w.info.name == m.name && w.is_online())
        });
        if matched.is_some() {
            return matched;
        }

        // Nothing in the graph matched. Falling back to "any online worker" is
        // right for an ordinary command — the graph may simply not be seeded
        // yet — but wrong when the caller asked for something specific:
        // running a CUDA job on a box with no GPU fails in a far more
        // confusing way than being told no machine has `gpu-compute`.
        let asked_for_specifics = capabilities.iter().any(|c| *c != BASELINE_CAPABILITY);
        if asked_for_specifics {
            warn!(
                required = ?capabilities,
                "no online worker has the required capabilities"
            );
            return None;
        }
        self.workers.select_worker()
    }

    /// Re-probe every machine (the master and all configured workers) and
    /// refresh the knowledge graph.
    ///
    /// Workers are probed concurrently: an unreachable one costs a connect
    /// timeout, and serializing those would make startup scale with the number
    /// of offline machines.
    pub async fn refresh_machine_graph(&self) -> anyhow::Result<usize> {
        // Probing a worker requires SSH anyway, so refresh reachability while
        // we are out there rather than making a second round of connections.
        let local = machines::probe_local(&self.master_name).await;
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

        // Drop machines that are no longer part of the fleet. Without this a
        // worker removed from `workers.toml` lingers in the graph forever,
        // permanently "offline" — and the graph exists to answer where work
        // should run, so a decommissioned host is worse than absent: it invites
        // the planner to keep proposing it.
        let known: Vec<String> = std::iter::once(self.master_name.clone())
            .chain(self.workers.workers.iter().map(|w| w.info.name.clone()))
            .collect();
        let pruned = machines::prune_unknown(&self.memory.graph, &known)?;
        if !pruned.is_empty() {
            info!(retired = ?pruned, "removed machines no longer in the fleet");
        }

        info!(machines = count, "machine knowledge graph refreshed");
        Ok(count)
    }
}

/// Short hostname, used when the caller does not name the master explicitly.
///
/// `uname -n` rather than `hostname`: Arch Linux ships no `hostname` binary, and
/// a worker that cannot name itself lands in the machine graph unnamed.
fn default_master_name() -> String {
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "master".to_string())
}

/// Render a memory turn's retrieved context for the planner prompt, or None
/// when there is nothing worth injecting. The renderer already frames the
/// content as background rather than instructions — memory is untrusted
/// prior conversation, and the framing lives with the data, not at each
/// call site.
fn render_turn_context(turn: Option<&crate::memory::Turn>) -> Option<String> {
    let t = turn?;
    if t.context.is_empty() {
        return None;
    }
    Some(t.context.render())
}

/// The persisted form of a finished run: the summary plus what each step
/// actually did, trimmed so one runaway command output cannot dominate the
/// memory index.
fn render_run_result(result: &crate::agent::run::RunResult) -> String {
    let mut text = result.summary.clone();
    for o in &result.outcomes {
        if o.command.is_empty() {
            continue;
        }
        let output: String = o.output.chars().take(400).collect();
        text.push_str(&format!("\n\n$ {} [{:?}]\n{}", o.command, o.status, output));
    }
    text
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use hive_common::protocol::{WorkerInfo, WorkerStatus};

    fn agent() -> MasterAgent {
        let pool = WorkerPool::new(
            ["archlinux-worker", "mac-air", "cis-linux2", "cis-a6000"]
                .iter()
                .map(|name| WorkerInfo {
                    name: (*name).into(),
                    host: (*name).into(),
                    user: "test".into(),
                    port: None,
                    tags: vec![],
                })
                .collect(),
        );
        pool.workers[0].set_status(WorkerStatus::Online);
        MasterAgent::new(
            LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            pool,
            SkillRegistry::new(),
            MemorySystem::new(),
        )
        .with_master_name("mac-mini")
    }

    fn task(target: &str, remote: bool) -> planner::SubTask {
        serde_json::from_value(serde_json::json!({
            "description": "create session", "target_machine": target,
            "requires_remote": remote, "commands": ["tmux new-session -d -s ws-share"]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn explicit_request_destinations_override_fleet_copied_by_model() {
        let agent = agent();
        let plan = serde_json::from_value(serde_json::json!({
            "targets":["mac-air","archlinux-worker","cis-linux2","cis-a6000"],
            "summary":"work", "subtasks":[task("mac-air",true)]
        }))
        .unwrap();
        let run = agent
            .materialize_plan(
                "Use mac-air and archlinux-worker.",
                plan,
                hive_common::Complexity::Simple,
                hive_common::AiProvider::Local,
                None,
                None,
            )
            .unwrap();
        assert_eq!(run.targets.len(), 2);
        assert!(run.targets.contains(&StepTarget::Remote {
            worker: "mac-air".into()
        }));
        assert!(run.targets.contains(&StepTarget::Remote {
            worker: "archlinux-worker".into()
        }));
        assert!(agent
            .named_request_targets("mac-air-backup and cis-a6000-test")
            .is_empty());
    }

    #[tokio::test]
    async fn named_air_stays_remote_even_if_model_sets_local_and_air_is_offline() {
        let agent = agent();
        assert_eq!(
            agent.subtask_target(&task("mac-air", false)).unwrap(),
            StepTarget::Remote {
                worker: "mac-air".into()
            }
        );
        assert_eq!(
            agent
                .subtask_target(&task("archlinux-worker", true))
                .unwrap(),
            StepTarget::Remote {
                worker: "archlinux-worker".into()
            }
        );
        assert!(agent.subtask_target(&task("unknown-laptop", true)).is_err());
        assert_eq!(
            agent.subtask_target(&task("mac-mini", true)).unwrap(),
            StepTarget::Local
        );
    }

    #[tokio::test]
    async fn failing_command_stops_dependents_and_reports_real_exit_code() {
        let agent = agent();
        let marker = std::env::temp_dir().join(format!("hive-dependency-{}", uuid::Uuid::new_v4()));
        let plan: PlannedRun = serde_json::from_value(serde_json::json!({
            "id":"r", "model":"test", "user_input":"do work", "summary":"work", "complexity":"simple",
            "routed_provider":"local", "provider":"local", "steps":[
                {"id":0,"description":"fail","command":"(printf 'broken' >&2; exit 12) | cat","target":{"kind":"local"},"risk":null},
                {"id":1,"description":"dependent","command":format!("touch {}", workflow::shell_quote(&marker.to_string_lossy())),"target":{"kind":"local"},"risk":null}
            ]
        })).unwrap();
        let result = agent.execute_run(&plan, &Approvals::none()).await;
        assert_eq!(result.outcomes[0].status, StepStatus::Failed);
        assert!(result.outcomes[0].output.contains("exit_code: 12"));
        assert!(result.outcomes[0].output.contains("broken"));
        assert_eq!(result.outcomes[1].status, StepStatus::Pending);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn malformed_remote_shell_is_rejected_before_dispatch() {
        let agent = agent();
        let plan: PlannedRun = serde_json::from_value(serde_json::json!({
            "id":"r", "model":"test", "user_input":"do work", "summary":"work", "complexity":"simple",
            "routed_provider":"local", "provider":"local", "steps":[
                {"id":0,"description":"bad quoting","command":"echo 'unterminated","target":{"kind":"remote","worker":"mac-air"},"risk":null}
            ]
        })).unwrap();
        let result = agent.execute_run(&plan, &Approvals::none()).await;
        assert_eq!(result.outcomes[0].status, StepStatus::Failed);
        assert!(result.outcomes[0].output.contains("Shell syntax rejected"));
        assert!(result.sessions.is_empty());
    }

    #[tokio::test]
    async fn all_named_workers_remain_placeable_without_capability_metadata() {
        let agent = agent();
        for name in ["mac-air", "archlinux-worker", "cis-linux2", "cis-a6000"] {
            let mut task = task(name, false);
            task.required_capabilities = vec!["websocket-server".into(), "file-sharing".into()];
            assert_eq!(
                agent.subtask_target(&task).unwrap(),
                StepTarget::Remote {
                    worker: name.into()
                }
            );
        }
    }

    #[tokio::test]
    async fn named_worker_ignores_model_capability_hints_but_auto_placement_enforces_them() {
        let agent = agent();
        let mut task = task("mac-air", true);
        task.required_capabilities = vec!["gpu-compute".into()];
        assert_eq!(
            agent.subtask_target(&task).unwrap(),
            StepTarget::Remote {
                worker: "mac-air".into()
            }
        );
        task.target_machine = Some("auto".into());
        assert!(agent.subtask_target(&task).is_err());
        task.target_machine = None;
        assert!(agent.subtask_target(&task).is_err());
        let context = agent.fleet_context();
        assert!(context.description.contains("mac-air"));
        assert!(context.description.contains("archlinux-worker"));
    }
}
