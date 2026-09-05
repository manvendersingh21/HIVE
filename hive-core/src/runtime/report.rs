//! What happened, in a form that cannot flatter itself.
//!
//! The outcome type is the load-bearing part. Every failure this runtime has to survive
//! was, in the measured runs, a failure that *looked* like success: an agent exited 0
//! having written nothing; a verifier accepted on a claim; a session was flagged and
//! kept running. So the enum makes each of those a distinct, named terminal, and none
//! of them is `Settled`.
//!
//! `NoAgreement` deserves its own note: it is a **success** of the protocol, not a
//! failure of the run. A worker that reads a contract and says "I cannot do this as
//! written" has done exactly what §7.4 exists to allow, and a runtime that treated that
//! as an error would be teaching agents to accept work they cannot do.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::attest::Corroboration;
use super::brief::AgentCall;

/// Where in the lifecycle a run stopped. Named because "the run failed" is not a
/// diagnosis, and §11.1 feeds exactly this kind of detail back into a rework request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Handshake,
    Authoring,
    Proposal,
    Review,
    Freeze,
    Execute,
    Submission,
    Verification,
    Settlement,
    Transcript,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        f.write_str(&s)
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum RunOutcome {
    /// The full lifecycle ran and both transcripts agree.
    Settled { verdict: String },
    /// A legitimate protocol terminal: the contract did not form. Exit 0.
    NoAgreement { reason: String },
    /// Something did not hold. The stage is always named.
    Failed { stage: Stage, reason: String },
    /// Tier-1 supervision suspended an agent. Nothing was killed; the process tree is
    /// intact and the command to take it over is in the report.
    Paused {
        session: String,
        reason: String,
        attach: String,
    },
}

impl RunOutcome {
    /// The process exit code this outcome deserves.
    ///
    /// `NoAgreement` is 0 on purpose. Everything else is non-zero, including `Paused`:
    /// a suspended agent is unfinished work waiting on a person, and a green exit would
    /// let it scroll past.
    pub fn exit_code(&self) -> i32 {
        match self {
            RunOutcome::Settled { .. } | RunOutcome::NoAgreement { .. } => 0,
            RunOutcome::Failed { .. } => 1,
            RunOutcome::Paused { .. } => 2,
        }
    }

    pub fn headline(&self) -> String {
        match self {
            RunOutcome::Settled { verdict } => format!("SETTLED — verdict {verdict}"),
            RunOutcome::NoAgreement { reason } => format!("NO AGREEMENT — {reason}"),
            RunOutcome::Failed { stage, reason } => format!("FAILED at {stage} — {reason}"),
            RunOutcome::Paused { session, reason, .. } => {
                format!("PAUSED — session {session} suspended: {reason}")
            }
        }
    }
}

/// A session this runtime started that had not ended when the run finished.
///
/// Recorded rather than cleaned up. Killing it would destroy the state a person is
/// being asked to look at — the same reason the session host suspends instead of
/// interrupting, learned three separate times in `docs/STATUS.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSession {
    pub name: String,
    pub attach: String,
}

/// The run record. This is the one place the mapping from a role to the tool behind it
/// legitimately appears (§3, and `docs/HACP-HIVE.md` §3): it never crosses the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub pair: String,
    pub supervisor_cli: String,
    pub worker_cli: String,
    pub run_dir: PathBuf,
    pub session_id: String,
    pub contract_id: String,
    pub task_id: String,
    pub task: String,
    #[serde(flatten)]
    pub outcome: RunOutcome,
    pub frames: usize,
    pub calls: Vec<AgentCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<hacp::v2::Artifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corroboration: Option<Corroboration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<PathBuf>,
    /// Empty in a clean shutdown. Anything here is named, never abandoned silently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub still_running: Vec<LiveSession>,
}

impl RunReport {
    pub async fn write(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let path = dir.join("run-report.json");
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(&path, format!("{json}\n")).await?;
        Ok(path)
    }

    /// A short human summary — what `hive collab run` prints when it is done.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{}\n  pair       {}\n  session    {}\n  contract   {}\n  frames     {}\n  agent runs {}",
            self.outcome.headline(),
            self.pair,
            self.session_id,
            self.contract_id,
            self.frames,
            self.calls.len(),
        );
        if let Some(c) = &self.corroboration {
            s.push_str(&format!(
                "\n  checks     {} corroborated, {} unmatched, {} contradicted",
                c.backed.len(),
                c.unmatched.len(),
                c.contradicted.len()
            ));
        }
        if let Some(t) = &self.transcript {
            s.push_str(&format!("\n  transcript {}", t.display()));
        }
        for live in &self.still_running {
            s.push_str(&format!(
                "\n  STILL RUNNING: {} — take it over with: {}",
                live.name, live.attach
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_agreement_is_a_success() {
        // §7.4: bounds reached without agreement is a valid terminal. A runtime that
        // exits non-zero here teaches agents to accept work they cannot do.
        assert_eq!(
            RunOutcome::NoAgreement { reason: "worker declined".into() }.exit_code(),
            0
        );
    }

    #[test]
    fn a_pause_never_exits_green() {
        let o = RunOutcome::Paused {
            session: "hive-x-work".into(),
            reason: "rm -rf /".into(),
            attach: "tmux attach -t hive-x-work".into(),
        };
        assert_eq!(o.exit_code(), 2);
    }

    #[test]
    fn a_failure_names_its_stage_in_the_headline() {
        let o = RunOutcome::Failed {
            stage: Stage::Execute,
            reason: "status.txt does not exist".into(),
        };
        assert!(o.headline().contains("FAILED at execute"), "{}", o.headline());
    }

    #[test]
    fn the_outcome_round_trips_through_its_tagged_form() {
        for o in [
            RunOutcome::Settled { verdict: "accept".into() },
            RunOutcome::NoAgreement { reason: "declined".into() },
            RunOutcome::Failed { stage: Stage::Freeze, reason: "digest mismatch".into() },
            RunOutcome::Paused {
                session: "s".into(),
                reason: "r".into(),
                attach: "a".into(),
            },
        ] {
            let v = serde_json::to_value(&o).unwrap();
            assert!(v.get("outcome").is_some(), "the tag is what a reader scans for: {v}");
            let back: RunOutcome = serde_json::from_value(v).unwrap();
            assert_eq!(back, o);
        }
    }

    #[test]
    fn a_still_running_session_appears_in_the_summary_with_its_attach_command() {
        let r = RunReport {
            pair: "a x b".into(),
            supervisor_cli: "a".into(),
            worker_cli: "b".into(),
            run_dir: PathBuf::from("/tmp/run"),
            session_id: "s-1".into(),
            contract_id: "c-1".into(),
            task_id: "t-1".into(),
            task: "do it".into(),
            outcome: RunOutcome::Paused {
                session: "hive-x-work".into(),
                reason: "tier 1".into(),
                attach: "tmux attach -t hive-x-work".into(),
            },
            frames: 4,
            calls: vec![],
            artifact: None,
            corroboration: None,
            transcript: None,
            still_running: vec![LiveSession {
                name: "hive-x-work".into(),
                attach: "tmux attach -t hive-x-work".into(),
            }],
        };
        assert!(r.summary().contains("STILL RUNNING"), "{}", r.summary());
        assert!(r.summary().contains("tmux attach -t hive-x-work"));
    }
}
