//! Frozen executable acceptance suites. Fixtures are authored before execution,
//! hashed with the contract and copied into fresh verification workspaces. Worker
//! output cannot replace the verifier's fixtures or turn a failed command green.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::collab::{SessionHost, SessionOutcome, SessionSpec};
use super::journal::Journal;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCommand {
    pub program: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationPlan {
    pub files: Vec<TestFile>,
    pub commands: Vec<TestCommand>,
}

impl VerificationPlan {
    pub fn from_terms(terms: &Value, outputs: &[String]) -> anyhow::Result<Option<Self>> {
        let Some(raw) = terms.get("verification") else { return Ok(None); };
        let plan:Self = serde_json::from_value(raw.clone())?;
        anyhow::ensure!(!plan.files.is_empty() && plan.files.len() <= 128, "verification needs 1–128 independent test files");
        anyhow::ensure!(!plan.commands.is_empty() && plan.commands.len() <= 8, "verification needs 1–8 executable commands");
        let mut paths: Vec<String> = outputs.iter().map(|p| p.to_lowercase()).collect();
        for file in &plan.files {
            anyhow::ensure!(!file.path.contains(['\\','\0']) && file.path.split('/').all(|p| !p.is_empty() && p != "." && p != ".."),
                "unsafe verifier fixture path");
            anyhow::ensure!(!file.content.trim().is_empty(), "empty verifier fixture");
            let name = file.path.to_lowercase();
            anyhow::ensure!(!paths.iter().any(|p| p == &name || p.starts_with(&format!("{name}/")) || name.starts_with(&format!("{p}/"))),
                "verifier fixtures collide with worker outputs or another fixture");
            anyhow::ensure!(!name.split('/').any(|p| p == ".git" || p == "__pycache__"), "excluded fixture path");
            paths.push(name);
        }
        let watchdog = crate::watchdog::Watchdog::new();
        for cmd in &plan.commands {
            anyhow::ensure!(!cmd.program.is_empty() && !cmd.program.contains(['\0','\n','\r']) &&
                cmd.args.iter().all(|a| !a.contains('\0')) && (1..=300).contains(&cmd.timeout_secs), "invalid or unbounded acceptance command");
            anyhow::ensure!(!super::cli::KNOWN.contains(&cmd.program.as_str()), "acceptance must execute tests, not ask an agent for an opinion");
            anyhow::ensure!(watchdog.scan_line(&format!("{} {}", cmd.program, cmd.args.join(" "))).is_none(),
                "acceptance command refused by Tier-1 policy");
        }
        Ok(Some(plan))
    }

    pub async fn materialize(&self, root: &Path) -> anyhow::Result<()> {
        for file in &self.files {
            let path = super::hosting::workspace_output(root, &file.path)?;
            tokio::fs::create_dir_all(path.parent().expect("fixture parent")).await?;
            tokio::fs::write(path, &file.content).await?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub role: String,
    pub stage: String,
    pub session: String,
    pub outcome: SessionOutcome,
    pub log: PathBuf,
    pub log_digest: String,
}

/// Exit status is necessary but not sufficient for runners which return success
/// without exercising any tests. Recognize direct Python unittest invocations;
/// arbitrary scripts retain their exit-status contract, not a fabricated test count.
/// This inspects the already captured log and never executes a command on replay.
pub fn acceptance_failure(command: &TestCommand, outcome: &SessionOutcome, log: &str) -> Option<String> {
    if !matches!(outcome, SessionOutcome::Exited { code: 0 }) {
        return Some(format!("acceptance command did not exit successfully: {outcome:?}"));
    }
    let program = Path::new(&command.program).file_name().and_then(|p| p.to_str()).unwrap_or_default();
    let python = program.strip_prefix("python").is_some_and(|version|
        version.chars().all(|c| c.is_ascii_digit() || c == '.'));
    let unittest = command.args.iter().position(|a| a == "-m")
        .is_some_and(|i| command.args.get(i + 1).is_some_and(|a| a == "unittest") &&
            !command.args[..i].iter().any(|a| a == "-c" || a == "--"));
    if !python || !unittest {
        return None;
    }
    let summary = regex::Regex::new(r"^Ran ([0-9]+) tests? in [0-9.]+s$").expect("constant summary regex");
    let result = regex::Regex::new(r"^OK(?: \((?:skipped=[0-9]+|expected failures=[0-9]+)(?:, (?:skipped=[0-9]+|expected failures=[0-9]+))*\))?$")
        .expect("constant result regex");
    let skipped = regex::Regex::new(r"(?:\(|, )skipped=([0-9]+)").expect("constant skip regex");
    let mut pending = None;
    let mut completed = false;
    for line in log.lines().map(str::trim) {
        if let Some(count) = summary.captures(line) {
            if pending.is_some() {
                return Some("unittest summary is missing its successful result".into());
            }
            let Ok(count) = count[1].parse::<u64>() else {
                return Some("unittest reported an invalid test count".into());
            };
            if count == 0 {
                return Some("unittest discovered zero tests; acceptance was not exercised".into());
            }
            pending = Some(count);
        } else if result.is_match(line) {
            let Some(count) = pending.take() else {
                return Some("unittest success has no matching test-count summary".into());
            };
            let skipped_count = match skipped.captures(line) {
                Some(c) => match c[1].parse::<u64>() {
                    Ok(count) => count,
                    Err(_) => return Some("unittest reported an invalid skipped count".into()),
                },
                None => 0,
            };
            if skipped_count >= count {
                return Some("unittest skipped every discovered test; acceptance was not exercised".into());
            }
            completed = true;
        }
    }
    if !completed || pending.is_some() {
        return Some("unittest did not report a complete successful nonempty suite".into());
    }
    None
}

/// Executable tests use the same durable intent and host recovery boundary as
/// model calls. A command already observed complete is never rerun on replay.
#[allow(clippy::too_many_arguments)]
pub async fn execute(host: &dyn SessionHost, journal: &Journal, role: &str, tag: &str,
    stage: &str, workspace: &Path, logs: &Path, command: &TestCommand) -> anyhow::Result<Execution> {
    tokio::fs::create_dir_all(workspace).await?;
    tokio::fs::create_dir_all(logs).await?;
    let name = format!("hive-{tag}-{stage}");
    let spec = SessionSpec { name:name.clone(), program:command.program.clone(), args:command.args.clone(),
        prompt:String::new(), cwd:workspace.into(), log:logs.join(format!("{stage}.log")), timeout_secs:command.timeout_secs };
    let (new, record) = journal.begin_invocation(&spec)?;
    if let Some(result) = record.result {
        let saved:Execution = serde_json::from_value(result)?;
        anyhow::ensure!(super::attest::Facts::measure(&saved.log).await?.digest == saved.log_digest,
            "acceptance execution log changed since its receipt");
        return Ok(saved);
    }
    let handle = if new { host.launch(&spec).await? } else { super::hosting::recover_invocation(host, Some(journal), &record).await? };
    let outcome = host.wait(&handle).await?;
    let execution = Execution { role:role.into(), stage:stage.into(), session:name, outcome,
        log_digest:super::attest::Facts::measure(&spec.log).await?.digest, log:spec.log };
    journal.finish_invocation(&execution.session, &serde_json::to_value(&execution)?)?;
    Ok(execution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan() -> Value {
        json!({"verification":{"files":[{"path":"tests/test_api.py","content":"assert 1 == 1\n"}],
            "commands":[{"program":"python3","args":["-m","unittest","discover","-s","tests"],"timeout_secs":30}]}})
    }

    #[test]
    fn suite_requires_disjoint_fixtures_and_bounded_mechanical_commands() {
        assert!(VerificationPlan::from_terms(&plan(), &["api.py".into()]).unwrap().is_some());
        assert!(VerificationPlan::from_terms(&plan(), &["tests/test_api.py".into()]).is_err());
        for key in ["files", "commands"] {
            let mut value = plan(); value["verification"][key] = json!([]);
            assert!(VerificationPlan::from_terms(&value, &[]).is_err());
        }
        let mut value = plan(); value["verification"]["commands"][0]["timeout_secs"] = json!(0);
        assert!(VerificationPlan::from_terms(&value, &[]).is_err());
        let mut value = plan(); value["verification"]["files"][0]["path"] = json!("../escape");
        assert!(VerificationPlan::from_terms(&value, &[]).is_err());
    }

    #[test]
    fn unittest_success_requires_exercised_tests_and_a_complete_summary() {
        let command = TestCommand { program:"/usr/bin/python3.9".into(),
            args:vec!["-I".into(), "-m".into(), "unittest".into()], timeout_secs:30 };
        let success = SessionOutcome::Exited { code:0 };
        for log in ["Ran 0 tests in 0.000s\n\nOK\n", "Ran 2 tests in 0.000s\nOK (skipped=2)\n",
            "", "OK\n", "Ran 3 tests in 0.001s\n", "Ran 1 test in 0.001s\nFAILED (failures=1)\n",
            "Ran 1 test in 0.001s\nOK\nRan 0 tests in 0.000s\nOK\n"] {
            assert!(acceptance_failure(&command, &success, log).is_some(), "{log}");
        }
        for log in ["Ran 1 test in 0.000s\n\nOK\n__HIVE_DONE__0\n",
            "Ran 3 tests in 0.001s\n\nOK (skipped=1, expected failures=1)\n"] {
            assert_eq!(acceptance_failure(&command, &success, log), None, "{log}");
            assert!(acceptance_failure(&command, &SessionOutcome::Exited { code:1 }, log).is_some());
        }
        let script = TestCommand { args:vec!["check.py".into()], ..command };
        assert_eq!(acceptance_failure(&script, &success, ""), None,
            "custom commands have exit-status semantics, not an inferred test count");
    }
}
