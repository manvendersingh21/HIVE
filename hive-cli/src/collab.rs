//! `hive collab` — run and review a two-agent HACP/2.0 collaboration.
//!
//! This is the surface over [`hive_core::runtime`]. It stays thin on purpose: every
//! decision that matters — what an agent is asked, what counts as evidence, when a run
//! may settle — belongs to the runtime and to the protocol, not to an argument parser.
//! What the CLI owns is where a run lives, what a person sees when it ends, and the
//! exit code, which is the only part of the result a script will ever read.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use hive_core::runtime::{authorize_continuation, resume_configured, run_configured, RunConfig, RunReport};
use hive_core::runtime::hosting::Placement;

#[derive(Subcommand)]
pub enum CollabAction {
    /// Run one collaboration between two agent CLIs, start to finish.
    Run {
        /// The CLI that authors the terms and verifies the result.
        #[arg(long)]
        supervisor: String,
        /// The CLI that reviews the contract and does the work.
        #[arg(long)]
        worker: String,
        /// The objective. The agents are given this and nothing else.
        #[arg(long)]
        task: String,
        /// Where the run lives. Defaults to ~/.hive/collab/<timestamp>-<neutral-id>.
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Wall-clock limit per agent invocation. Exceeding it suspends, never kills.
        #[arg(long, default_value_t = 600)]
        timeout_secs: u64,
        /// SSH config alias for the supervisor; omitted means this device.
        #[arg(long)]
        supervisor_host: Option<String>,
        /// SSH config alias for the worker; omitted means this device.
        #[arg(long)]
        worker_host: Option<String>,
        #[arg(long)]
        supervisor_model: Option<String>,
        #[arg(long)]
        worker_model: Option<String>,
        /// Maximum repair attempts after the initial submission (0–5).
        #[arg(long, default_value_t = 2)]
        max_rework: u32,
    },
    /// Print the report and transcript of a finished run.
    Show { run_dir: PathBuf },
    /// Inspect durable protocol state, delivery receipts and invocation results.
    Inspect { run_dir: PathBuf },
    /// Recover a run; completed calls are replayed, uncertain calls are reattached.
    Resume {
        run_dir: PathBuf,
        /// Continue exactly this recorded suspended invocation, preserving its identity.
        #[arg(long, requires = "reason")]
        continue_session: Option<String>,
        /// Audit reason for accepting existing log output and renewing its time budget.
        #[arg(long, requires = "continue_session")]
        reason: Option<String>,
    },
    /// List runs under ~/.hive/collab.
    List,
}

pub async fn run(action: CollabAction) -> anyhow::Result<i32> {
    match action {
        CollabAction::Inspect { run_dir } => {
            anyhow::ensure!(run_dir.join("runtime.db").is_file(), "no runtime journal in this run");
            let journal = hive_core::runtime::journal::Journal::open(&run_dir.join("runtime.db"))?;
            println!("{}", serde_json::to_string_pretty(&journal.inspect()?)?);
            Ok(0)
        }
        CollabAction::Resume { run_dir, continue_session, reason } => {
            if let Some(name) = continue_session {
                authorize_continuation(&run_dir, &name, reason.as_deref().unwrap_or_default()).await?;
            }
            let report = resume_configured(&run_dir).await?;
            println!("{}", report.summary());
            Ok(report.outcome.exit_code())
        }
        CollabAction::Run {
            supervisor,
            worker,
            task,
            run_dir,
            timeout_secs,
            supervisor_host,
            worker_host,
            supervisor_model,
            worker_model,
            max_rework,
        } => {
            let run_dir = run_dir.unwrap_or_else(|| default_run_dir(&supervisor, &worker));
            println!("Run directory: {}\n", run_dir.display());

            // The built-in Tier-1 rule set, scanned on every line an agent emits. This
            // is the whole reason a collaboration belongs in HIVE rather than in a
            // script: `subprocess.run` cannot notice `rm -rf /` scrolling past, and
            // cannot suspend the process group that produced it.
            let report = run_configured(
                &RunConfig {
                    supervisor,
                    worker,
                    task,
                    run_dir,
                    timeout_secs,
                    supervisor_placement: Placement { host:supervisor_host, model:supervisor_model },
                    worker_placement: Placement { host:worker_host, model:worker_model },
                    max_rework,
                },
            )
            .await?;
            println!("{}", report.summary());
            Ok(report.outcome.exit_code())
        }
        CollabAction::Show { run_dir } => {
            let report = read_report(&run_dir)?;
            println!("{}\n", report.summary());
            if let Some(t) = &report.transcript {
                match std::fs::read_to_string(t) {
                    Ok(lines) => {
                        println!("Transcript ({}):", t.display());
                        for line in lines.lines() {
                            println!("  {line}");
                        }
                    }
                    Err(e) => println!("Transcript {} is unreadable: {e}", t.display()),
                }
            }
            Ok(report.outcome.exit_code())
        }
        CollabAction::List => {
            let root = collab_root();
            let mut rows: Vec<(String, String)> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let dir = entry.path();
                    if !dir.is_dir() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    // A run directory with no report is a run that did not finish —
                    // said plainly, rather than hidden by filtering it out.
                    let state = match read_report(&dir) {
                        Ok(r) => r.outcome.headline(),
                        Err(_) => "no report — the run did not finish".to_string(),
                    };
                    rows.push((name, state));
                }
            }
            if rows.is_empty() {
                println!("No collaborations under {}.", root.display());
                return Ok(0);
            }
            rows.sort();
            for (name, state) in rows {
                println!("{name:<40} {state}");
            }
            Ok(0)
        }
    }
}

fn read_report(dir: &Path) -> anyhow::Result<RunReport> {
    let path = dir.join("run-report.json");
    let bytes = std::fs::read(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn collab_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".hive/collab")
}

fn default_run_dir(_supervisor: &str, _worker: &str) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    // Physical workspace paths appear in briefs, so their names must be neutral too.
    collab_root().join(format!("{stamp}-{}", uuid::Uuid::new_v4().simple()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_run_dir_is_unique_neutral_and_sorts_by_time() {
        let d = default_run_dir("claude", "codex");
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains("claude") && !name.contains("codex"), "{name}");
        assert_ne!(d, default_run_dir("claude", "codex"));
        // The timestamp leads so `hive collab list` is chronological without parsing.
        assert!(name.starts_with("20"), "{name}");
        assert!(d.starts_with(collab_root()));
    }

    #[test]
    fn reading_a_report_that_is_not_there_names_the_path() {
        let e = read_report(Path::new("/tmp/definitely-not-a-hive-run"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("run-report.json"), "{e}");
    }
}
