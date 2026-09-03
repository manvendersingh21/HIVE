//! Rendering a plan and asking about the steps the watchdog flagged.
//!
//! The web UI has had this gate since the agent chat shipped; the CLI ran
//! whatever the planner produced. That asymmetry was the dangerous way round —
//! `hive task` is the interface most likely to be scripted, and the local model
//! writing the commands is a 9B.
//!
//! The gate is the watchdog's own Tier-1 rules, so a command is judged by the
//! same regexes whether it was planned in a browser or a terminal.

use std::collections::HashSet;
use std::io::{self, Write};

use hive_core::agent::run::{PlannedRun, RunResult, StepStatus, StepTarget};

/// How the user answers gated steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatePolicy {
    /// Ask on stdin.
    Prompt,
    /// Approve everything without asking. For non-interactive use, where a
    /// prompt would hang forever; opting into this is the user's call.
    AssumeYes,
    /// Refuse everything flagged and report what was skipped.
    DenyAll,
}

pub fn describe_target(target: &StepTarget) -> String {
    match target {
        StepTarget::Local => "local".to_string(),
        StepTarget::Remote { worker } if worker.is_empty() => "remote (no worker)".to_string(),
        StepTarget::Remote { worker } => format!("remote:{worker}"),
    }
}

/// Print the routing decision and the plan's steps.
pub fn print_plan(run: &PlannedRun) {
    println!("Complexity: {} → {}", run.complexity, run.provider);
    if run.routed_provider != run.provider {
        println!(
            "  (routed to {} but it was unavailable, so the local model answered)",
            run.routed_provider
        );
    }
    println!("Plan: {}", run.summary);

    if run.steps.iter().all(|s| s.command.is_empty()) {
        println!("  (no commands)");
        return;
    }
    for step in &run.steps {
        if step.command.is_empty() {
            continue;
        }
        let flag = if step.needs_approval() { "  ⚠ needs approval" } else { "" };
        println!(
            "  [{}] {}  ({}){}",
            step.id,
            step.command,
            describe_target(&step.target),
            flag
        );
    }
}

/// Print what each step actually did.
pub fn print_outcomes(result: &RunResult) {
    for outcome in &result.outcomes {
        if outcome.status == StepStatus::AwaitingApproval {
            continue;
        }
        let mark = match outcome.status {
            StepStatus::Executed | StepStatus::Delegated => "ok",
            StepStatus::Failed => "FAILED",
            StepStatus::Denied => "skipped",
            StepStatus::Skipped => "--",
            StepStatus::AwaitingApproval => unreachable!("filtered above"),
        };
        if outcome.command.is_empty() {
            continue;
        }
        println!("\n[{mark}] $ {}", outcome.command);
        for line in outcome.output.lines().take(40) {
            println!("    {line}");
        }
    }

    for session in &result.sessions {
        println!(
            "\nDelegated to '{}' as tmux session '{}' — supervised by the master.",
            session.worker_name, session.session_name
        );
    }
}

/// Ask about every gated step, returning the approved and denied ids.
///
/// A step whose answer cannot be read (no tty, EOF) is treated as denied:
/// defaulting to "run it" would make a piped command silently execute exactly
/// the commands the watchdog objected to.
pub fn decide(run: &PlannedRun, result: &RunResult, policy: GatePolicy) -> (Vec<usize>, Vec<usize>) {
    let gated: HashSet<usize> = result.awaiting_approval.iter().copied().collect();
    let mut approved = Vec::new();
    let mut denied = Vec::new();

    for step in &run.steps {
        if !gated.contains(&step.id) {
            continue;
        }
        let reason = step
            .risk
            .as_ref()
            .map(|r| r.reason.clone())
            .unwrap_or_else(|| "flagged by the safety watchdog".to_string());

        println!("\n⚠  Step {} was flagged", step.id);
        println!("   $ {}", step.command);
        println!("   {reason}");

        match policy {
            GatePolicy::AssumeYes => {
                println!("   → approved (--yes)");
                approved.push(step.id);
            }
            GatePolicy::DenyAll => {
                println!("   → skipped (--deny-flagged)");
                denied.push(step.id);
            }
            GatePolicy::Prompt => {
                print!("   Run it? [y/N] ");
                let _ = io::stdout().flush();
                let mut answer = String::new();
                match io::stdin().read_line(&mut answer) {
                    Ok(0) | Err(_) => {
                        println!("   → skipped (no answer available)");
                        denied.push(step.id);
                    }
                    Ok(_) => {
                        if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                            approved.push(step.id);
                        } else {
                            println!("   → skipped");
                            denied.push(step.id);
                        }
                    }
                }
            }
        }
    }

    (approved, denied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_common::{AiProvider, Complexity, SafetyAnalysis, SafetyCategory, Severity};
    use hive_core::agent::run::{PlannedStep, StepOutcome};

    fn risky() -> SafetyAnalysis {
        SafetyAnalysis {
            is_safe: false,
            severity: Severity::High,
            category: Some(SafetyCategory::DestructiveCommand),
            reason: "matched rm-rf-generic".into(),
            suggested_action: "review".into(),
        }
    }

    fn run_with_gate() -> (PlannedRun, RunResult) {
        let run = PlannedRun {
            id: "r1".into(),
            user_input: "clean up".into(),
            summary: "remove a directory".into(),
            complexity: Complexity::Simple,
            routed_provider: AiProvider::Local,
            provider: AiProvider::Local,
            steps: vec![
                PlannedStep {
                    id: 0,
                    description: "safe".into(),
                    command: "ls".into(),
                    target: StepTarget::Local,
                    risk: None,
                },
                PlannedStep {
                    id: 1,
                    description: "risky".into(),
                    command: "rm -rf /tmp/x".into(),
                    target: StepTarget::Local,
                    risk: Some(risky()),
                },
            ],
        };
        let result = RunResult {
            run_id: "r1".into(),
            summary: "remove a directory".into(),
            complexity: Complexity::Simple,
            provider: AiProvider::Local,
            outcomes: vec![StepOutcome {
                id: 1,
                command: "rm -rf /tmp/x".into(),
                status: StepStatus::AwaitingApproval,
                output: "matched rm-rf-generic".into(),
            }],
            sessions: vec![],
            awaiting_approval: vec![1],
        };
        (run, result)
    }

    #[test]
    fn assume_yes_approves_only_the_gated_steps() {
        let (run, result) = run_with_gate();
        let (approved, denied) = decide(&run, &result, GatePolicy::AssumeYes);
        assert_eq!(approved, vec![1], "ungated steps are not re-approved");
        assert!(denied.is_empty());
    }

    #[test]
    fn deny_all_refuses_every_flagged_step() {
        let (run, result) = run_with_gate();
        let (approved, denied) = decide(&run, &result, GatePolicy::DenyAll);
        assert!(approved.is_empty());
        assert_eq!(denied, vec![1]);
    }

    #[test]
    fn nothing_gated_means_nothing_to_decide() {
        let (run, mut result) = run_with_gate();
        result.awaiting_approval.clear();
        let (approved, denied) = decide(&run, &result, GatePolicy::AssumeYes);
        assert!(approved.is_empty() && denied.is_empty());
    }

    #[test]
    fn targets_render_readably_including_the_no_worker_case() {
        assert_eq!(describe_target(&StepTarget::Local), "local");
        assert_eq!(
            describe_target(&StepTarget::Remote { worker: "lawfinder".into() }),
            "remote:lawfinder"
        );
        assert_eq!(
            describe_target(&StepTarget::Remote { worker: String::new() }),
            "remote (no worker)"
        );
    }
}
