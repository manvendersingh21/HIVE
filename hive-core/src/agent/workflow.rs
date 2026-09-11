//! Feedback-driven work rounds. The web layer checkpoints each round before
//! execution and persists observations before asking the model what to do next.
use serde::{Deserialize, Serialize};

use super::{
    planner::PlanPhase,
    run::{PlannedRun, RunResult, StepStatus},
    MasterAgent,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_error: Option<String>,
    #[serde(default)]
    pub verified_targets: Vec<super::run::StepTarget>,
    pub round: usize,
    pub round_start: usize,
    pub verified: bool,
    pub status: String,
}

impl Default for WorkflowState {
    fn default() -> Self {
        Self {
            planning_error: None,
            verified_targets: vec![],
            round: 1,
            round_start: 0,
            verified: false,
            status: "running".into(),
        }
    }
}

impl WorkflowState {
    pub fn observe(&mut self, phase: PlanPhase, result: &RunResult, run: &PlannedRun) {
        if phase == PlanPhase::Work {
            self.verified = false;
            self.verified_targets.clear();
        }
        if phase == PlanPhase::Verify {
            let success = result
                .outcomes
                .iter()
                .any(|o| o.status == StepStatus::Executed)
                && result
                    .outcomes
                    .iter()
                    .all(|o| matches!(o.status, StepStatus::Executed | StepStatus::Skipped));
            if success {
                for outcome in &result.outcomes {
                    if outcome.status == StepStatus::Executed {
                        if let Some(step) = run.steps.iter().find(|s| s.id == outcome.id) {
                            if !self.verified_targets.contains(&step.target) {
                                self.verified_targets.push(step.target.clone());
                            }
                        }
                    }
                }
            }
            self.verified = success
                && run
                    .targets
                    .iter()
                    .all(|t| self.verified_targets.contains(t));
        }
    }

    pub fn can_finish(&self, result: &RunResult) -> bool {
        self.verified
            && result.awaiting_approval.is_empty()
            && !result.outcomes.iter().skip(self.round_start).any(|o| {
                matches!(
                    o.status,
                    StepStatus::Failed
                        | StepStatus::Pending
                        | StepStatus::Delegated
                        | StepStatus::Denied
                )
            })
    }
}

impl MasterAgent {
    pub async fn continue_run(
        &self,
        run: &PlannedRun,
        result: &RunResult,
        state: &WorkflowState,
    ) -> anyhow::Result<PlannedRun> {
        // Keep recent diagnostics detailed and older successful work identifiable.
        // Never feed a regenerated plan as if it were an observation.
        let mut context = format!("Execution feedback for the SAME user request. Round {}. Successful functional verification recorded: {}.\nDo not repeat completed steps. Continue implementation or verification; fix failed commands using their actual errors. A final complete response requires a successful verify round.\n", state.round, state.verified);
        context.push_str(&format!("\nFixed destinations for this task: {:?}. Do not execute on any other destination. Verified destinations so far: {:?}.\n", run.targets, state.verified_targets));
        if let Some(error) = &state.planning_error {
            let instruction = if state.verified {
                "Verification has already succeeded. Return phase=complete with subtasks=[] and an evidence-based summary if the request is satisfied. Otherwise identify one specifically unproven requirement and check only that."
            } else {
                "Return a much smaller round: only ONE short command or ONE concise source file. Continue from saved observations; no completed actions need replaying."
            };
            context.push_str(&format!("\nYour previous planning attempt failed before any new actions ran: {error}\n{instruction}\n"));
        }
        let recent = result.outcomes.len().saturating_sub(8);
        for (index, o) in result.outcomes.iter().enumerate() {
            let step = run.steps.iter().find(|s| s.id == o.id);
            let where_ = step.map(|s| format!("{:?}", s.target)).unwrap_or_default();
            let command: String = o
                .command
                .chars()
                .take(if index >= recent { 1800 } else { 180 })
                .collect();
            let output: String = o
                .output
                .chars()
                .take(if index >= recent { 4500 } else { 250 })
                .collect();
            context.push_str(&format!(
                "\nStep {} on {} [{:?}]\n{}\nObserved output:\n{}\n",
                o.id, where_, o.status, command, output
            ));
        }
        let skill = self.skills.resolve(&run.user_input, &self.llm).await;
        let plan = self
            .planner
            .plan(
                &self.llm,
                &run.user_input,
                run.complexity,
                &self.fleet_context(),
                Some(&context),
                skill,
            )
            .await?;
        self.materialize_plan(
            &run.user_input,
            plan,
            run.complexity,
            run.routed_provider,
            None,
            skill,
        )
    }
}

pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

pub(crate) fn file_command(file: &super::planner::FileWrite) -> anyhow::Result<String> {
    let data = serde_json::to_string(file)?;
    // The JSON argument is data, not interpolated Python or shell code. Preserve
    // existing permissions and publish the complete file atomically.
    let script = r#"import json,os,pathlib,stat,sys,tempfile
v=json.loads(sys.argv[1]); p=pathlib.Path(v['path']).expanduser()
if not p.is_absolute(): raise ValueError('file path must be absolute or start with ~/')
if p.is_symlink(): raise ValueError('refusing to replace a symbolic link')
p.parent.mkdir(parents=True,exist_ok=True)
mode=stat.S_IMODE(p.stat().st_mode) if p.exists() else 0o600
t=None
try:
 with tempfile.NamedTemporaryFile(mode='w',encoding='utf-8',dir=p.parent,delete=False) as f:
  t=f.name; f.write(v['content']); f.flush(); os.fsync(f.fileno())
 os.chmod(t,mode); os.replace(t,p); t=None
 print('Wrote '+str(len(v['content'].encode()))+' bytes to '+str(p))
finally:
 if t: os.unlink(t)
"#;
    Ok(format!(
        "python3 -c {} {}",
        shell_quote(script),
        shell_quote(&data)
    ))
}

#[cfg(test)]
mod tests {
    use super::super::run::StepOutcome;
    use super::*;
    #[test]
    fn completion_requires_successful_verification_and_work_invalidates_it() {
        let mut state = WorkflowState::default();
        let mut result = RunResult {
            run_id: "r".into(),
            summary: "ready".into(),
            complexity: hive_common::Complexity::Simple,
            provider: hive_common::AiProvider::Local,
            outcomes: vec![StepOutcome {
                id: 0,
                command: "probe".into(),
                status: StepStatus::Executed,
                output: "ok".into(),
            }],
            sessions: vec![],
            awaiting_approval: vec![],
        };
        let run: PlannedRun = serde_json::from_value(serde_json::json!({
            "id":"r", "user_input":"test", "summary":"test", "complexity":"simple", "provider":"local", "routed_provider":"local",
            "targets":[{"kind":"local"}], "steps":[{"id":0,"description":"probe","command":"probe","target":{"kind":"local"},"risk":null}]
        })).unwrap();
        assert!(!state.can_finish(&result));
        state.observe(PlanPhase::Verify, &result, &run);
        assert!(state.can_finish(&result));
        state.observe(PlanPhase::Work, &result, &run);
        assert!(!state.can_finish(&result));
        result.outcomes[0].status = StepStatus::Failed;
        state.observe(PlanPhase::Verify, &result, &run);
        assert!(!state.can_finish(&result));
    }

    #[test]
    fn completion_requires_verification_on_every_destination() {
        let run: PlannedRun = serde_json::from_value(serde_json::json!({
            "id":"r", "user_input":"test", "summary":"test", "complexity":"simple", "provider":"local", "routed_provider":"local",
            "targets":[{"kind":"remote","worker":"air"},{"kind":"remote","worker":"arch"}],
            "steps":[
                {"id":0,"description":"probe air","command":"probe","target":{"kind":"remote","worker":"air"},"risk":null},
                {"id":1,"description":"probe arch","command":"probe","target":{"kind":"remote","worker":"arch"},"risk":null}
            ]
        })).unwrap();
        let mut state = WorkflowState::default();
        let mut result = RunResult {
            run_id: "r".into(),
            summary: "test".into(),
            complexity: run.complexity,
            provider: run.provider,
            outcomes: vec![StepOutcome {
                id: 0,
                command: "probe".into(),
                status: StepStatus::Executed,
                output: "ok".into(),
            }],
            sessions: vec![],
            awaiting_approval: vec![],
        };
        state.observe(PlanPhase::Verify, &result, &run);
        assert!(
            !state.can_finish(&result),
            "checking only Air cannot complete a two-host task"
        );
        result.outcomes[0].id = 1;
        state.observe(PlanPhase::Verify, &result, &run);
        assert!(state.can_finish(&result));
        state.observe(PlanPhase::Work, &result, &run);
        state.observe(PlanPhase::Verify, &result, &run);
        assert!(
            !state.can_finish(&result),
            "new work invalidates earlier verification"
        );
    }

    #[tokio::test]
    async fn file_contents_are_data_and_survive_quotes_backticks_and_unicode() {
        let root = std::env::temp_dir().join(format!("hive-file-{}", uuid::Uuid::new_v4()));
        let file = super::super::planner::FileWrite {
            path: root.join("a ' file.py").to_string_lossy().into_owned(),
            content: "print(\"héllo\\n\")\n# ' ; `touch pwned` $(touch pwned)\n".into(),
        };
        let out = tokio::process::Command::new("bash")
            .args(["-c", &file_command(&file).unwrap()])
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(std::fs::read_to_string(&file.path).unwrap(), file.content);
        assert!(!root.join("pwned").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
