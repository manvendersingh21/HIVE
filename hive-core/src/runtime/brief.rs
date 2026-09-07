//! What an agent is told, and whether it actually did the thing.
//!
//! Two rules shape everything here.
//!
//! **Evidence, not signals (§9.4).** An agent's exit code is not proof of its work. The
//! live runs measured this three separate times: two different CLIs narrated successful
//! file creation that had not happened, one resolved "the current directory" to its own
//! scratch space, and every one of those calls exited 0
//! (`docs/findings/adapter-edge.md`, findings 8–10). So an invocation here is not "run
//! the CLI"; it is "run the CLI and then look on disk", with exactly one retry that
//! says plainly what was missing.
//!
//! **Vendor opacity (§3).** A brief is protocol content: it crosses to the other agent's
//! side of the run and is quoted into contracts. It must never name the tool behind a
//! role — a worker that learns its peer is a particular vendor will condition on that,
//! and the protocol's whole claim is that collaboration happens through interfaces. A
//! test at the bottom of this file greps every rendered brief for every name in
//! [`super::cli::KNOWN`].

use std::path::Path;

use serde_json::Value;

use crate::collab::{SessionHost, SessionOutcome, SessionSpec};

use super::cli::AgentCli;

/// Appended when an agent's first attempt left the required file absent.
///
/// It states the disconfirming observation rather than scolding: the agent is being
/// told a fact it could not see, which is the only thing that changes the second run.
const RETRY_NOTE: &str = "\n\nIMPORTANT: your previous reply did not actually create the \
required file — this was checked on disk after you finished. Create it now with your \
file-writing tool. Printing its contents as a reply does not create it.";

/// One invocation of one agent, as it appears in the run report.
///
/// `role` is "supervisor" or "worker", never the program name: this record is written
/// into the run report, and a report is something a person reads beside a transcript.
/// The CLI behind a role is recorded once, at the top of the report, deliberately.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AgentCall {
    pub role: String,
    pub stage: String,
    pub session: String,
    pub outcome: String,
    pub seconds: u64,
    /// Whether the required output passed the file/JSON-object gate after this call.
    pub produced: bool,
}

/// How an invocation ended, from the runtime's point of view.
#[derive(Debug)]
pub enum CallResult {
    /// The required output passed the file/JSON-object gate.
    Produced(Vec<AgentCall>),
    /// It does not, after the retry. The run cannot proceed on a claim.
    Missing(Vec<AgentCall>),
    /// Tier-1 supervision suspended the session. The process tree is intact and
    /// attachable; nothing is retried and nothing is killed.
    Paused {
        session: String,
        reason: String,
        calls: Vec<AgentCall>,
    },
}

/// Everything needed to run one agent once, in its own workspace.
#[derive(Clone, Copy)]
pub struct Invocation<'a> {
    pub host: &'a dyn SessionHost,
    pub cli: AgentCli,
    /// "supervisor" or "worker".
    pub role: &'static str,
    /// The agent's workspace. Never the run root: an agent must not be able to read
    /// its peer's side of the edge by walking up one directory.
    pub cwd: &'a Path,
    /// Stable role project, distinct from immutable per-attempt output directories.
    pub conversation_cwd: &'a Path,
    /// Where logs and rendered briefs are kept.
    pub logs: &'a Path,
    /// Prefix that makes tmux session names unique to this run.
    pub run_tag: &'a str,
    pub timeout_secs: u64,
    pub journal: Option<&'a super::journal::Journal>,
    pub model: Option<&'a str>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CallReceipt {
    call: AgentCall,
    outcome: SessionOutcome,
    retry_note: String,
    output_digest: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
}

impl Invocation<'_> {
    /// Run the agent on `brief`, then require `relpath` to exist inside its workspace.
    ///
    /// Up to two attempts. The brief is also written beside the log, because a run
    /// report that shows what an agent produced without showing what it was asked is
    /// unreviewable.
    pub async fn require_file(
        &self,
        stage: &str,
        brief: &str,
        relpath: &str,
    ) -> anyhow::Result<CallResult> {
        self.require_output(stage, brief, &[relpath.to_string()], false).await
    }

    pub async fn require_files(&self, stage: &str, brief: &str, paths: &[String]) -> anyhow::Result<CallResult> {
        anyhow::ensure!(!paths.is_empty(), "an invocation must declare its required outputs");
        self.require_output(stage, brief, paths, false).await
    }

    /// Control records must be JSON objects, not merely existing files. Retry
    /// malformed output once after the invocation exits; never repair model data
    /// silently. The rejected bytes remain in the audit log directory.
    pub async fn require_json(
        &self, stage: &str, brief: &str, relpath: &str,
    ) -> anyhow::Result<CallResult> {
        self.require_output(stage, brief, &[relpath.to_string()], true).await
    }

    async fn require_output(
        &self, stage: &str, brief: &str, relpaths: &[String], json_object: bool,
    ) -> anyhow::Result<CallResult> {
        let targets: Vec<_> = relpaths.iter().map(|p| super::hosting::workspace_output(self.cwd, p)).collect::<anyhow::Result<_>>()?;
        let target = &targets[0];
        let mut calls = Vec::new();
        let mut retry_note = RETRY_NOTE.to_string();

        for attempt in 1..=2u32 {
            let text = if attempt == 1 {
                brief.to_string()
            } else {
                format!("{brief}{retry_note}")
            };
            let suffix = if attempt == 1 { String::new() } else { format!("-r{attempt}") };
            let name = format!("hive-{}-{}{}", self.run_tag, stage, suffix);

            tokio::fs::create_dir_all(self.logs).await?;
            tokio::fs::write(self.logs.join(format!("{stage}{suffix}-brief.txt")), &text).await?;

            let conversation_key = format!("conversation:{}", self.role);
            let conversation = match self.journal {
                Some(journal) => journal.initialize(&format!("conversation-input:{name}"),
                    &journal.get(&conversation_key)?.unwrap_or(Value::Null))?,
                None => Value::Null,
            };

            let cwd = match self.journal.map(|j| j.invocation_if_exists(&name)).transpose()?.flatten() {
                // Existing launch identity includes its original directory. Never
                // silently migrate an already running or completed invocation.
                Some(record) => record.spec.cwd,
                None => self.cli.working_directory(self.cwd, self.conversation_cwd, conversation.as_str()).to_path_buf(),
            };
            let spec = SessionSpec {
                name: name.clone(),
                program: self.cli.name.to_string(),
                args: self.cli.call_args(&cwd, self.model, conversation.as_str())?,
                prompt: text,
                cwd,
                log: self.logs.join(format!("{stage}{suffix}.log")),
                timeout_secs: self.timeout_secs,
            };

            let started = std::time::Instant::now();
            let intent = self.journal.map(|j| j.begin_invocation(&spec)).transpose()?;
            if let Some((_, record)) = &intent {
                if let Some(value) = &record.result {
                    let receipt: CallReceipt = serde_json::from_value(value.clone())?;
                    if let (Some(journal), Some(id)) = (self.journal, &receipt.conversation_id) {
                        journal.remember(&conversation_key, &serde_json::json!(id))?;
                    }
                    retry_note = receipt.retry_note;
                    let produced = receipt.call.produced;
                    calls.push(receipt.call);
                    if matches!(receipt.outcome, SessionOutcome::Paused { .. } | SessionOutcome::TimedOut) {
                        return Ok(CallResult::Paused { session: name,
                            reason: "recorded suspension requires explicit operator resume".into(), calls });
                    }
                    if produced {
                        for p in relpaths { super::hosting::workspace_output(self.cwd, p)?; }
                        let digest = outputs_digest(&targets).await?;
                        anyhow::ensure!(receipt.output_digest.as_deref() == Some(digest.as_str()),
                            "completed invocation output changed since its receipt: {}", target.display());
                        return Ok(CallResult::Produced(calls));
                    }
                    continue;
                }
            }
            let handle = match &intent {
                Some((false, record)) => super::hosting::recover_invocation(self.host, self.journal, record).await?,
                _ => self.host.launch(&spec).await?,
            };
            let outcome = self.host.wait(&handle).await?;
            let seconds = started.elapsed().as_secs();

            // Checked before the outcome is interpreted, because the outcome is the
            // agent's signal and this is the evidence.
            let mut produced = targets.iter().all(|p| p.is_file());
            if produced && json_object && matches!(outcome, SessionOutcome::Exited { .. }) {
                let bytes = tokio::fs::read(&target).await?;
                if !serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v.is_object()) {
                    produced = false;
                    tokio::fs::write(self.logs.join(format!("{stage}{suffix}-invalid-output.txt")), bytes).await?;
                    retry_note = "\n\nThe required file exists but is not one valid JSON object. \
                        Rewrite it using the requested schema, with no Markdown fences, \
                        trailing text, or duplicated object. Its previous bytes were retained for audit.".into();
                }
            }

            calls.push(AgentCall {
                role: self.role.to_string(),
                stage: stage.to_string(),
                session: name.clone(),
                outcome: describe(&outcome),
                seconds,
                produced,
            });

            if let Some(journal) = self.journal {
                for p in relpaths { super::hosting::workspace_output(self.cwd, p)?; }
                let log = tokio::fs::read_to_string(&spec.log).await?;
                let conversation_id = self.cli.conversation_from_log(&log)?;
                if let (Some(expected), Some(actual)) = (conversation.as_str(), conversation_id.as_deref()) {
                    anyhow::ensure!(expected == actual, "CLI switched conversation despite an explicit continuation handle");
                }
                let receipt = CallReceipt {
                    call: calls.last().expect("just recorded call").clone(),
                    outcome: outcome.clone(), retry_note: retry_note.clone(),
                    output_digest: if produced { Some(outputs_digest(&targets).await?) } else { None },
                    conversation_id: conversation_id.clone(),
                };
                journal.finish_invocation(&name, &serde_json::to_value(receipt)?)?;
                if let Some(id) = conversation_id { journal.remember(&conversation_key, &serde_json::json!(id))?; }
            }

            if matches!(outcome, SessionOutcome::Paused { .. } | SessionOutcome::TimedOut) {
                let reason = match outcome {
                    SessionOutcome::Paused { reason } => reason,
                    _ => "invocation timed out; inspect the existing session before resuming or retrying".into(),
                };
                // A suspended session is a human's to look at. Retrying would start a
                // second agent beside a frozen one, which is how you end up with two
                // processes editing one workspace.
                return Ok(CallResult::Paused {
                    session: name,
                    reason,
                    calls,
                });
            }
            if produced {
                return Ok(CallResult::Produced(calls));
            }
        }

        Ok(CallResult::Missing(calls))
    }
}

async fn outputs_digest(paths: &[std::path::PathBuf]) -> anyhow::Result<String> {
    let mut digests = Vec::new();
    for path in paths { digests.push(super::attest::Facts::measure(path).await?.digest); }
    if digests.len() == 1 { Ok(digests.remove(0)) }
    else { Ok(hacp::v2::canon::digest_of(&serde_json::json!(digests))?) }
}

fn describe(outcome: &SessionOutcome) -> String {
    match outcome {
        SessionOutcome::Exited { code } => format!("exit {code}"),
        SessionOutcome::TimedOut => "timed out".to_string(),
        SessionOutcome::Paused { reason } => format!("paused: {reason}"),
    }
}

// ---------------------------------------------------------------------------
// The four briefs, in lifecycle order
// ---------------------------------------------------------------------------

/// Step 2 — the supervising agent authors the terms it wants to delegate.
///
/// The shape is dictated and the acceptance criteria are constrained to things a
/// machine can re-check, because [`super::attest`] has to corroborate them later. A
/// criterion like "the report is well written" cannot be mechanically backed, and an
/// `accept` that rests only on such criteria is refused at §9.4 — so the constraint is
/// stated here rather than discovered as a failure four steps on.
pub fn author(task: &str, terms_path: &Path) -> String {
    format!(
        "You are the supervisor in a two-agent delegation protocol. The task to delegate is:

  {task}

Write delegation-terms.json at this absolute path:

  {}

with EXACTLY this shape:

{{
  \"outputs\": [{{\"name\": \"<output file name>\", \"media_type\": \"text/plain\", \"one_line\": true}}],
  \"acceptance\": [\"<check 1>\", \"<check 2>\", \"<check 3>\"],
  \"budget\": {{\"max_minutes\": 5}}
}}

Rules: use only these mechanically supported acceptance forms (replace WORD with the
case-sensitive required alphanumeric/underscore word): \"the file exists\",
\"the file is not empty\", \"the file is exactly one line\", \"the file contains the word WORD\",
\"the file ends with the word WORD\", \"the sha256 matches the submitted manifest\",
\"size matches the submitted manifest\". Unsupported prose will block acceptance.
Do not weaken the task to fit these checks; if they cannot express its requirements,
report that limitation rather than claiming full verification. Declare every required output
file, with a relative workspace path (nested directories are allowed, no parent traversal).
Use one_line=true only when that output must be exactly one line. Global acceptance
criteria apply to each output. Create the file with your file-writing tool; printing the JSON as a reply is not
writing it. Write the JSON file and nothing else.

For coding or any executable task, also include a verification object in the terms:
\"verification\": {{\"files\":[{{\"path\":\"tests/test_acceptance.py\",\"content\":\"<complete independent test source>\"}}],
\"commands\":[{{\"program\":\"python3\",\"args\":[\"-m\",\"unittest\",\"discover\",\"-s\",\"tests\"],\"timeout_secs\":60}}]}}
Use the language and test runner appropriate to the task, not necessarily this example.
Author meaningful tests covering the original objective now, before seeing the worker's implementation.
The fixture paths must not overlap worker output paths. The tests and command definitions
will be frozen into the contract and executed independently on both devices. Include
\"the frozen acceptance suite passes\" as an acceptance criterion. File-only checks
do not verify code semantics; do not substitute a status message for a coding objective.",
        terms_path.display()
    )
}

/// Step 4 — the performing agent decides whether it can do the work as written.
///
/// It is asked for a decision, not for the work: a worker that starts executing during
/// review has agreed to nothing, and the contract it later accepts is a formality over
/// a decision already made.
pub fn review(proposal_body: &Value, accept_path: &Path) -> String {
    format!(
        "You are the worker in a two-agent delegation protocol. A delegation contract has
been proposed to you:

{}

Decide ONLY whether you can complete it exactly as written. Do not start the work.

If you can, write accept.json at {} containing:
  {{\"accepted\": true}}

If any part is unclear or impossible, write the same file containing:
  {{\"accepted\": false, \"reasons\": [\"...\"]}}

To negotiate instead of declining, use {{\"accepted\":false,\"counter_terms\":<complete proposed terms>,\"reasons\":[\"why\"]}}.
For clarification use {{\"accepted\":false,\"question\":\"specific question\"}}.
Do not start work during negotiation. You will receive any revised terms for explicit acceptance.

Declining is a legitimate answer and is not a failure. Write the file and nothing else.",
        pretty(proposal_body),
        accept_path.display()
    )
}

/// Step 7 — EXECUTE. Not a message: a contract state entered implicitly on freeze
/// (§7.5), which is why `v2::envelope::kinds` has no execute kind for this to mirror.
pub fn work(frozen_body: &Value, terms: &Value, output_name: &str, output_path: &Path, cwd: &Path) -> String {
    format!(
        "The frozen delegation contract below is your work order. Complete its outputs
exactly. Work inside this directory: {}

Write {output_name} at this absolute path:
  {}

Write that one file and nothing else.

Frozen revision:
{}

Contract terms:
{}",
        cwd.display(),
        output_path.display(),
        pretty(frozen_body),
        pretty(terms),
    )
}

/// Step 9 — the supervising agent verifies, and is told not to trust what it was sent.
///
/// The claimed digest and size are shown *and* the agent is told to recompute them. That
/// is deliberate: a verifier that is only shown the claim can do nothing but restate it,
/// and the whole §9.3 record would then be a copy of the submission with a verdict
/// stapled on.
pub fn verify(artifact_path: &Path, claimed_digest: &str, claimed_size: u64, contents: &str, acceptance: &[String], verdict_path: &Path) -> String {
    let criteria = acceptance
        .iter()
        .enumerate()
        .map(|(i, c)| format!("  {}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are the verifier in a two-agent delegation protocol. A submission arrived.

Artifact: {}
It was CLAIMED to be sha256 {claimed_digest}, {claimed_size} bytes. Treat that as a
claim, not a fact.

The file's contents as read from disk:
---
{contents}
---

Acceptance criteria:
{criteria}

Perform EVERY check for real: read the file yourself, count its lines, recompute its
digest (`shasum -a 256 {}`). Do not restate the claims above.

Then write verdict.json at {} containing EXACTLY:

{{\"verdict\": \"accept\" | \"reject\" | \"rework\",
 \"checks\": [{{\"name\": \"...\", \"passed\": true, \"detail\": \"what you actually did\"}}],
 \"reasons\": [\"...\"]}}

Name each check after what it measures — digest, line count, size, required words — so
the check can be independently corroborated. Write the JSON file and nothing else.",
        artifact_path.display(),
        artifact_path.display(),
        verdict_path.display(),
    )
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn all_briefs() -> Vec<String> {
        let p = PathBuf::from("/tmp/run/wrk/out.txt");
        vec![
            author("write a status file", &PathBuf::from("/tmp/run/sup/delegation-terms.json")),
            review(&json!({"contract_id": "c-1", "terms": {"outputs": []}}), &p),
            work(&json!({"revision": 1}), &json!({"outputs": []}), "out.txt", &p, &PathBuf::from("/tmp/run/wrk")),
            verify(&p, &"a".repeat(64), 12, "hello\n", &["is one line".into()], &p),
        ]
    }

    #[test]
    fn no_brief_can_tell_an_agent_who_its_peer_is() {
        // §3: the mapping from a role to a vendor lives in the run record only. A peer
        // that learns the tool behind the other role will condition on it, and the
        // protocol's claim is that collaboration happens through interfaces.
        for brief in all_briefs() {
            let lower = brief.to_lowercase();
            for vendor in super::super::cli::KNOWN {
                assert!(
                    !lower.contains(vendor),
                    "a brief names the vendor {vendor:?}:\n{brief}"
                );
            }
        }
    }

    #[test]
    fn every_brief_names_an_absolute_path_to_write() {
        // Finding 9: an agent resolved "the current directory" to its own scratch space
        // and wrote a real file nobody could find. Absolute paths are the fix.
        for brief in all_briefs() {
            assert!(
                brief.contains("/tmp/run/"),
                "a brief gives no absolute target path:\n{brief}"
            );
        }
    }

    #[test]
    fn the_verify_brief_marks_the_claims_as_claims() {
        let b = verify(
            &PathBuf::from("/tmp/run/wrk/out.txt"),
            &"b".repeat(64),
            7,
            "ok\n",
            &["non-empty".into()],
            &PathBuf::from("/tmp/run/sup/verdict.json"),
        );
        assert!(b.contains("claim, not a fact"), "{b}");
        assert!(b.contains("shasum -a 256"), "the verifier is given a way to recheck: {b}");
    }

    #[test]
    fn the_review_brief_says_declining_is_allowed() {
        // A worker that believes declining is failure will accept work it cannot do,
        // and the run then fails four steps later with a much worse diagnosis.
        let b = review(&json!({}), &PathBuf::from("/tmp/run/wrk/accept.json"));
        assert!(b.contains("legitimate answer"), "{b}");
    }

    #[test]
    fn the_retry_note_states_the_observation_rather_than_repeating_the_order() {
        assert!(RETRY_NOTE.contains("checked on disk"));
    }
}
