//! The fallback report (`spec/HACP.md` §10).
//!
//! A stock CLI cannot be taught to write a `CompletionReport`, and C1 says that is the
//! normal case rather than a failure. When the worker leaves no `REPORT.json`, the
//! adapter writes one from what it can observe **without understanding the work**: the
//! exit code, `git diff --numstat`, and whether the paths it was told to watch exist.
//!
//! What it must never do is guess. `CompletionReport::fallback` starts at
//! `Blocked`/`NotReported` and this module keeps `contract_status` there, because an
//! adapter has no way to know whether a contract was satisfied — it never read the
//! contract, and reading it would make it a participant that reasons about content
//! (§2). The report is marked `adapter-synthesized` so no consumer can mistake this for
//! the worker's own claim.

use hacp::report::{
    CompletionReport, ContractStatus, DiffStat, Outcome, ReportArtifact, ReportEvidence,
    ReportSource,
};

/// Everything the adapter can see from outside the work.
#[derive(Debug, Clone, Default)]
pub struct Observations {
    /// The worker process's exit status, if the adapter observed the exit.
    pub exit_code: Option<i32>,
    /// `git diff --numstat` over the worker's workspace, if it is a git tree.
    pub diffstat: Option<DiffStat>,
    /// Artifacts the adapter was told to look for, and whether their paths exist.
    pub artifacts: Vec<ReportArtifact>,
    /// Where the worker's tee'd output was retained. §10 requires this to point at a
    /// log a human can actually read.
    pub log_path: Option<String>,
    /// How long the adapter ran. This is the adapter's lifetime, which is the closest
    /// honest proxy it has for the worker's; it is not measured inside the worker.
    pub duration_secs: u64,
}

/// Build the fallback report. Pure, so the mapping from observation to outcome is
/// testable without a process or a repository.
pub fn synthesize(agent_urn: &str, obs: &Observations) -> CompletionReport {
    let mut report = CompletionReport::fallback(agent_urn);

    // The exit code is the one fact about success the adapter genuinely holds, and it
    // is a weak one: a worker can exit 0 having done nothing. So a clean exit maps to
    // `Partial` — "it ended, and no one has said what it produced" — and never to
    // `Success`, which is a claim only the worker or the arbiter may make. No observed
    // exit at all leaves the `Blocked` that `fallback` starts with.
    report.outcome = match obs.exit_code {
        Some(0) => Outcome::Partial,
        Some(_) => Outcome::Failure,
        None => Outcome::Blocked,
    };

    // Deliberately left as `fallback` set it. The adapter never read the contract.
    report.contract_status = ContractStatus::NotReported;
    report.summary = summary_line(obs);
    report.diffstat = obs.diffstat.clone();
    report.artifacts = obs.artifacts.clone();
    report.duration_secs = obs.duration_secs;
    if let Some(log_path) = &obs.log_path {
        report.evidence = Some(ReportEvidence { log_path: log_path.clone(), session: None });
    }
    report.follow_ups = vec![
        "No REPORT.json was written; this report is mechanical and states nothing about \
         whether the work is correct."
            .to_string(),
    ];
    debug_assert_eq!(report.source, ReportSource::AdapterSynthesized);
    report
}

/// A summary made only of observations, in the order a human would want them.
fn summary_line(obs: &Observations) -> String {
    let exit = match obs.exit_code {
        Some(code) => format!("worker exited with code {code}"),
        None => "worker exit was not observed".to_string(),
    };
    let diff = match &obs.diffstat {
        Some(d) => format!(
            "{} file(s) changed, +{}/-{}",
            d.files_changed, d.insertions, d.deletions
        ),
        None => "no diff was available".to_string(),
    };
    let artifacts = if obs.artifacts.is_empty() {
        "no artifact paths were checked".to_string()
    } else {
        let present = obs.artifacts.iter().filter(|a| a.exists).count();
        format!("{present}/{} expected artifact path(s) exist", obs.artifacts.len())
    };
    format!("Adapter-synthesized because the worker wrote no REPORT.json: {exit}; {diff}; {artifacts}.")
}

/// Parse `git diff --numstat` output.
///
/// Binary files are reported as `-\t-\t<path>`; they count as a changed file and
/// contribute no line counts, which is what git itself reports.
pub fn parse_numstat(output: &str) -> DiffStat {
    let mut stat = DiffStat::default();
    for line in output.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(added), Some(deleted), Some(_path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        stat.files_changed += 1;
        stat.insertions += added.parse::<usize>().unwrap_or(0);
        stat.deletions += deleted.parse::<usize>().unwrap_or(0);
    }
    stat
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observations() -> Observations {
        Observations {
            exit_code: Some(0),
            diffstat: Some(DiffStat { files_changed: 3, insertions: 120, deletions: 4 }),
            artifacts: vec![
                ReportArtifact {
                    artifact_id: "job-store".into(),
                    path: "src/store".into(),
                    sha256: None,
                    exists: true,
                },
                ReportArtifact {
                    artifact_id: "cli".into(),
                    path: "src/cli".into(),
                    sha256: None,
                    exists: false,
                },
            ],
            log_path: Some("agents/a/agent.log".into()),
            duration_secs: 240,
        }
    }

    #[test]
    fn is_marked_adapter_synthesized() {
        // §15: an adapter MUST mark a report it produced on the worker's behalf. This
        // is the one field a consumer uses to decide how much the report is worth.
        let report = synthesize("urn:hacp:agent:a-3f0c", &observations());
        assert_eq!(report.source, ReportSource::AdapterSynthesized);
        assert!(!report.is_self_reported());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["source"], serde_json::json!("adapter-synthesized"));
    }

    #[test]
    fn never_claims_the_contract_was_satisfied() {
        // The adapter never read the contract, so it has nothing to say about it.
        let report = synthesize("urn:hacp:agent:a-3f0c", &observations());
        assert_eq!(report.contract_status, ContractStatus::NotReported);
    }

    #[test]
    fn a_clean_exit_is_partial_never_success() {
        let report = synthesize("urn:hacp:agent:a-3f0c", &observations());
        assert_eq!(report.outcome, Outcome::Partial);
    }

    #[test]
    fn a_nonzero_exit_is_a_failure_and_no_exit_stays_blocked() {
        let mut obs = observations();
        obs.exit_code = Some(2);
        assert_eq!(synthesize("urn:hacp:agent:a-1", &obs).outcome, Outcome::Failure);
        obs.exit_code = None;
        assert_eq!(synthesize("urn:hacp:agent:a-1", &obs).outcome, Outcome::Blocked);
    }

    #[test]
    fn carries_the_observations_it_actually_made() {
        let report = synthesize("urn:hacp:agent:a-3f0c", &observations());
        assert_eq!(report.agent, "urn:hacp:agent:a-3f0c");
        assert_eq!(report.diffstat.as_ref().map(|d| d.insertions), Some(120));
        assert_eq!(report.artifacts.len(), 2);
        assert_eq!(report.evidence.as_ref().map(|e| e.log_path.as_str()), Some("agents/a/agent.log"));
        assert_eq!(report.duration_secs, 240);
        assert!(report.summary.contains("exited with code 0"), "summary was: {}", report.summary);
        assert!(report.summary.contains("1/2 expected artifact path(s) exist"));
    }

    #[test]
    fn parses_numstat_including_binary_files() {
        let stat = parse_numstat("12\t3\tsrc/a.rs\n0\t7\tsrc/b.rs\n-\t-\tassets/logo.png\n");
        assert_eq!(stat.files_changed, 3);
        assert_eq!(stat.insertions, 12);
        assert_eq!(stat.deletions, 10);
        assert_eq!(parse_numstat("").files_changed, 0);
    }
}
