//! Mechanical coordination rules for delegated assignments.
//!
//! Protocol state is represented by the pinned HACP v2 types. This module only
//! adapts peer messages and measured attestation results to that state machine.

use crate::runtime::attest::{self, Corroboration};
use hacp::v2::{canon, Contract, ContractLimits, Relationship, Session, Task, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub fn default_max_rework() -> u8 {
    2
}

fn excerpt(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn limits(max_rework: u8) -> ContractLimits {
    ContractLimits {
        max_rounds: 3,
        max_amendments: 16,
        max_rework: u64::from(max_rework),
    }
}

/// Explicit checks fixed by the coordinator's assignment. Commands use argv,
/// with a workspace-relative working directory and a bounded runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AcceptanceCheck {
    FileExists { path: String },
    Command { argv: Vec<String>, cwd: String, timeout_seconds: u64 },
}

fn relative_path(path: &str, allow_root: bool) -> bool {
    (allow_root && path == ".") || (!path.is_empty()
        && !path.starts_with(['/', '~'])
        && !path.contains(['\n', '\r', '\0', '\\'])
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != ".."))
}

pub fn validate_checks(checks: &[AcceptanceCheck]) -> anyhow::Result<()> {
    anyhow::ensure!(checks.len() <= 32, "At most 32 acceptance checks");
    let mut seconds = 0;
    for check in checks {
        match check {
            AcceptanceCheck::FileExists { path } => {
                anyhow::ensure!(relative_path(path, false), "Acceptance file must be workspace-relative");
            }
            AcceptanceCheck::Command { argv, cwd, timeout_seconds } => {
                anyhow::ensure!(!argv.is_empty() && !argv[0].trim().is_empty()
                    && argv.len() <= 128 && argv.iter().all(|a| !a.contains('\0')),
                    "Acceptance command requires a valid argv");
                anyhow::ensure!(relative_path(cwd, true), "Acceptance cwd must be workspace-relative");
                anyhow::ensure!((1..=120).contains(timeout_seconds), "Acceptance timeout must be 1–120 seconds");
                seconds += timeout_seconds;
            }
        }
    }
    anyhow::ensure!(seconds <= 600, "Acceptance checks may take at most 600 seconds in total");
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckMeasurement {
    pub check: AcceptanceCheck,
    pub passed: bool,
    pub detail: String,
}

/// A runner response is support only when it covers the exact frozen check
/// list. Missing, substituted or failed checks all fail the attestation gate.
pub fn corroborate_checks(checks: &[AcceptanceCheck], measured: &[CheckMeasurement]) -> Corroboration {
    let mut c = Corroboration::default();
    if checks.is_empty() {
        c.contradicted.push("No mechanical acceptance checks were specified; worker claims cannot establish completion".into());
    }
    if checks.len() != measured.len() {
        c.contradicted.push("Runner returned an incomplete acceptance check list".into());
    }
    for (index, check) in checks.iter().enumerate() {
        match measured.get(index) {
            Some(result) if &result.check == check && result.passed => {
                let name = format!("check {} passed", index + 1);
                c.backed.push(name.clone());
                c.requirements_backed.push(name);
            }
            result => c.contradicted.push(format!("check {}: {}", index + 1,
                result.map(|r| excerpt(&r.detail, 512)).unwrap_or("missing measurement"))),
        }
    }
    c
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgreementRecord {
    pub contract: Contract,
    pub proposed_terms: Value,
    pub proposed_digest: String,
    pub proposer: String,
    pub rejected_changes: Vec<Value>,
}

impl AgreementRecord {
    pub fn propose(task_id: &str, proposer: &str, peer: &str, text: &str) -> anyhow::Result<Self> {
        let proposer = agent_urn(proposer);
        let peer = agent_urn(peer);
        let mut session = Session::open(
            &format!("agreement:{task_id}:{}", canon::digest_canonical(text)),
            &proposer,
            &peer,
        )?;
        session.accept(&peer)?;
        let terms = json!({"agreement": text});
        let digest = canon::digest_of(&terms)?;
        let mut contract = Contract::propose(
            &session,
            &format!("contract:{task_id}:{}", &digest[..16]),
            Task {
                task_id: task_id.into(),
                summary: text.into(),
                owner: proposer.clone(),
            },
            Relationship::Collaboration,
            None,
            None,
            None,
            None,
            vec![],
            limits(default_max_rework()),
        )?;
        contract.agree(&proposer, &terms)?;
        Ok(Self {
            contract,
            proposed_terms: terms,
            proposed_digest: digest,
            proposer,
            rejected_changes: vec![],
        })
    }

    /// Record the counterparty's reference. Only the exact canonical digest freezes.
    pub fn agree(&mut self, by: &str, digest: &str) -> anyhow::Result<String> {
        let by = agent_urn(by);
        if digest != self.proposed_digest {
            self.rejected_changes
                .push(json!({"by":by,"digest":digest,"reason":"one-sided change"}));
            anyhow::bail!("agreement digest does not match the bilateral proposal");
        }
        self.contract.agree(&by, &self.proposed_terms)?;
        Ok(self.contract.freeze(self.proposed_terms.clone())?)
    }

    /// Begin an amendment. It remains pending until both parties accept the same digest.
    pub fn propose_amendment(&mut self, by: &str, text: &str) -> anyhow::Result<String> {
        let by = agent_urn(by);
        let terms = json!({"agreement": text});
        self.contract.propose_amendment(&by)?;
        self.contract
            .decide_amendment(&by, true, Some(terms.clone()))?;
        self.proposed_terms = terms;
        self.proposed_digest = canon::digest_of(&self.proposed_terms)?;
        self.proposer = by;
        Ok(self.proposed_digest.clone())
    }

    pub fn agree_amendment(&mut self, by: &str, digest: &str) -> anyhow::Result<String> {
        let by = agent_urn(by);
        if digest != self.proposed_digest {
            self.rejected_changes
                .push(json!({"by":by,"digest":digest,"reason":"one-sided amendment"}));
            anyhow::bail!("amendment digest does not match the bilateral proposal");
        }
        self.contract
            .decide_amendment(&by, true, Some(self.proposed_terms.clone()))?
            .ok_or_else(|| anyhow::anyhow!("amendment still needs both parties"))
    }
}

fn agent_urn(id: &str) -> String {
    if id.starts_with("urn:hacp:agent:") {
        id.into()
    } else {
        format!("urn:hacp:agent:{id}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionVerdict {
    Accept,
    Rework,
    NoAgreement,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionRecord {
    pub verdict: CompletionVerdict,
    pub rework_rounds: u8,
    pub evidence: Vec<String>,
    pub followup: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionAssessment {
    pub turn_seq: i64,
    pub record: CompletionRecord,
    pub measurements: Vec<CheckMeasurement>,
}

impl Default for CompletionRecord {
    fn default() -> Self {
        Self {
            verdict: CompletionVerdict::Rework,
            rework_rounds: 0,
            evidence: vec![],
            followup: None,
        }
    }
}

impl CompletionRecord {
    /// Apply independently measured evidence through the existing attestation gate.
    pub fn assess(&mut self, evidence: impl Into<String>, corroboration: &Corroboration) {
        self.assess_with_limits(evidence, corroboration, &limits(default_max_rework()));
    }

    pub fn assess_with_limits(&mut self, evidence: impl Into<String>, corroboration: &Corroboration, limits: &ContractLimits) {
        if self.verdict == CompletionVerdict::NoAgreement {
            return;
        }
        let evidence = evidence.into();
        self.evidence.push(evidence.clone());
        match attest::gate(&Verdict::Accept, corroboration) {
            Ok(()) => {
                self.verdict = CompletionVerdict::Accept;
                self.followup = None;
            }
            Err(error) if u64::from(self.rework_rounds) < limits.max_rework => {
                self.rework_rounds += 1;
                self.verdict = CompletionVerdict::Rework;
                self.followup = Some(format!(
                    "Acceptance checks failed: {}. Evidence (excerpt): {}",
                    excerpt(&error.to_string(), 4000), excerpt(&evidence, 8000)
                ));
            }
            Err(_) => {
                self.verdict = CompletionVerdict::NoAgreement;
                self.followup = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_complete_measurement_list_can_pass() {
        let required = AcceptanceCheck::FileExists { path: "result.txt".into() };
        let measured = CheckMeasurement { check: required.clone(), passed: true, detail: "file exists".into() };
        let check = |checks: &[AcceptanceCheck], measured: &[CheckMeasurement]| attest::gate(&Verdict::Accept, &corroborate_checks(checks, measured)).is_ok();
        assert!(check(&[required.clone()], &[measured.clone()]));
        assert!(!check(&[], &[]));
        assert!(!check(&[required.clone()], &[]));
        assert!(!check(&[required.clone()], &[measured.clone(), measured.clone()]));
        assert!(!check(&[AcceptanceCheck::FileExists { path: "other.txt".into() }], &[measured.clone()]));
        assert!(!check(&[required], &[CheckMeasurement { passed: false, ..measured }]));
    }

    #[test]
    fn checks_reject_traversal_empty_commands_and_unbounded_time() {
        for path in ["../file", "/file", "link/../file", "~/file", "a\\b", ""] {
            assert!(validate_checks(&[AcceptanceCheck::FileExists { path: path.into() }]).is_err());
        }
        let command = |argv: Vec<String>, cwd: &str, timeout_seconds| AcceptanceCheck::Command { argv, cwd: cwd.into(), timeout_seconds };
        assert!(validate_checks(&[command(vec!["cargo".into(), "test".into()], ".", 120)]).is_ok());
        assert!(validate_checks(&[command(vec![], ".", 1)]).is_err());
        assert!(validate_checks(&[command(vec!["true".into()], "../outside", 1)]).is_err());
        assert!(validate_checks(&[command(vec!["true".into()], ".", 121)]).is_err());
        assert!(validate_checks(&vec![command(vec!["true".into()], ".", 120); 6]).is_err());
    }

    #[test]
    fn rework_uses_the_hacp_limit_and_no_agreement_is_terminal() {
        let failed = Corroboration { contradicted: vec!["missing".into()], ..Default::default() };
        for bound in [0, 1, 2] {
            let mut record = CompletionRecord::default();
            for _ in 0..=bound {
                record.assess_with_limits("measured", &failed, &limits(bound));
            }
            assert_eq!(record.verdict, CompletionVerdict::NoAgreement);
            assert_eq!(record.rework_rounds, bound);
            let terminal = record.clone();
            record.assess("claim repaired", &Corroboration { backed: vec!["file".into()], ..Default::default() });
            assert_eq!(record, terminal);
        }
    }

    #[test]
    fn feedback_is_bounded_but_full_unicode_evidence_is_retained() {
        let evidence = "évidence ".repeat(4000);
        let failed = Corroboration { contradicted: vec!["échec ".repeat(4000)], ..Default::default() };
        let mut record = CompletionRecord::default();
        record.assess(&evidence, &failed);
        assert_eq!(record.evidence, vec![evidence]);
        assert!(record.followup.unwrap().len() < 13000);
    }

    #[test]
    fn runner_control_measures_real_artifacts_before_attestation_accepts() {
        // Exercise the actual CLI in a throwaway journal. No native agent,
        // service, HOME override, SSH or live Hive state is involved.
        let script = r#"
import contextlib, importlib.util, io, json, pathlib, sys, tempfile, uuid
spec = importlib.util.spec_from_file_location('runner', sys.argv[1])
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
with tempfile.TemporaryDirectory(prefix='hive-acceptance-') as tmp:
    root = pathlib.Path(tmp)
    runner.BASE = root/'runs'
    ident = str(uuid.uuid4())
    journal = runner.Journal(runner.BASE/ident)
    journal.quiet = True
    journal.set('assignment', {'autonomy':'yolo'})
    journal.emit('acknowledgment', {'message_id':'initial'})
    journal.state('completed')
    request = {'turn_seq':journal.completed_turn(), 'workspace':str(root), 'checks':[
        {'kind':'command', 'argv':[sys.executable,'-c',"from pathlib import Path; Path('result.txt').write_text('ready'); print('checked')"], 'cwd':'.', 'timeout_seconds':2},
        {'kind':'file_exists','path':'result.txt'}]}
    journal.db.close()
    sys.argv = ['runner.py','acceptance','--run-id',ident]
    sys.stdin = io.StringIO(json.dumps(request))
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        runner.main()
    response = json.loads(out.getvalue())
    print(json.dumps({'checks':request['checks'], 'response':response}))
"#;
        let result = std::process::Command::new("python3")
            .args(["-c", script, concat!(env!("CARGO_MANIFEST_DIR"), "/src/delegation/runner/runner.py")])
            .output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        let checks: Vec<AcceptanceCheck> = serde_json::from_value(result["checks"].clone()).unwrap();
        let measured: Vec<CheckMeasurement> = serde_json::from_value(result["response"]["measurements"].clone()).unwrap();
        assert!(measured[0].detail.contains("exit=0\nchecked"));
        let mut record = CompletionRecord::default();
        record.assess(result.to_string(), &corroborate_checks(&checks, &measured));
        assert_eq!(record.verdict, CompletionVerdict::Accept);
    }

    #[test]
    fn agreement_freezes_and_amendment_needs_both_sides() {
        let mut record = AgreementRecord::propose("task", "a", "b", "API v1").unwrap();
        let first = record.agree("b", &record.proposed_digest.clone()).unwrap();
        assert_eq!(record.contract.state, hacp::v2::ContractState::Executing);
        let amendment = record.propose_amendment("a", "API v2").unwrap();
        assert_eq!(record.contract.state, hacp::v2::ContractState::Amending);
        assert!(record.agree_amendment("b", "different").is_err());
        assert_eq!(record.rejected_changes.len(), 1);
        let second = record.agree_amendment("b", &amendment).unwrap();
        assert_ne!(first, second);
        assert_eq!(record.contract.revisions.len(), 2);
    }

    #[test]
    fn third_failed_attestation_is_no_agreement() {
        let failed = Corroboration {
            contradicted: vec!["missing output".into()],
            ..Default::default()
        };
        let mut record = CompletionRecord::default();
        record.assess("failure one", &failed);
        assert_eq!(record.verdict, CompletionVerdict::Rework);
        assert!(record.followup.as_deref().unwrap().contains("failure one"));
        record.assess("failure two", &failed);
        assert_eq!(record.verdict, CompletionVerdict::Rework);
        record.assess("failure three", &failed);
        assert_eq!(record.verdict, CompletionVerdict::NoAgreement);
        assert_eq!(record.rework_rounds, 2);
    }
}
