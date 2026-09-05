//! `hive collab` — run and review a two-agent HACP/2.0 collaboration.
//!
//! This is the surface over [`hive_core::runtime`]. It stays thin on purpose: every
//! decision that matters — what an agent is asked, what counts as evidence, when a run
//! may settle — belongs to the runtime and to the protocol, not to an argument parser.
//! What the CLI owns is where a run lives, what a person sees when it ends, and the
//! exit code, which is the only part of the result a script will ever read.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use hive_core::collab::session::LocalSessionHost;
use hive_core::runtime::{run_bilateral, RunConfig, RunReport};
use hive_core::watchdog::Watchdog;

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
        /// Where the run lives. Defaults to ~/.hive/collab/<timestamp>-<pair>.
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Wall-clock limit per agent invocation. Exceeding it suspends, never kills.
        #[arg(long, default_value_t = 600)]
        timeout_secs: u64,
    },
    /// Print the report and transcript of a finished run.
    Show { run_dir: PathBuf },
    /// List runs under ~/.hive/collab.
    List,
}

pub async fn run(action: CollabAction) -> anyhow::Result<i32> {
    match action {
        CollabAction::Run {
            supervisor,
            worker,
            task,
            run_dir,
            timeout_secs,
        } => {
            let run_dir = run_dir.unwrap_or_else(|| default_run_dir(&supervisor, &worker));
            println!("Run directory: {}\n", run_dir.display());

            // The built-in Tier-1 rule set, scanned on every line an agent emits. This
            // is the whole reason a collaboration belongs in HIVE rather than in a
            // script: `subprocess.run` cannot notice `rm -rf /` scrolling past, and
            // cannot suspend the process group that produced it.
            let host = LocalSessionHost::with_watchdog(Watchdog::new());
            let report = run_bilateral(
                &host,
                &RunConfig {
                    supervisor,
                    worker,
                    task,
                    run_dir,
                    timeout_secs,
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

fn default_run_dir(supervisor: &str, worker: &str) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    collab_root().join(format!("{stamp}-{supervisor}x{worker}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_run_dir_is_unique_per_pair_and_sorts_by_time() {
        let d = default_run_dir("claude", "codex");
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with("-claudexcodex"), "{name}");
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
