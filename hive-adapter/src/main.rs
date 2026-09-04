//! `hive-adapter` — a HACP adapter: one worker's files on one side, the bus on the
//! other (`spec/HACP.md` §2, §13.2, §15).
//!
//! It exists to make constraint C1 true in practice. A worker is a stock agentic CLI
//! that cannot be taught a protocol, so the protocol's edge is files: the worker reads
//! `BRIEF.md`, reads `INBOX/`, writes `OUTBOX/`, and ideally writes `REPORT.json`. This
//! process turns that into conformant protocol traffic, and it does so without knowing
//! anything about the worker beside it or about the master on the other end.
//!
//! That last point is architectural, not stylistic: this crate depends on `hacp` and a
//! transport, and on nothing from the implementation that hosts it. An adapter that
//! knew about a particular coordinator would not be an adapter.
//!
//! ```text
//! HIVE_COLLAB_TOKEN=... hive-adapter \
//!   --run-id run-8a41 --role a --agent-urn urn:hacp:agent:a-8a41 \
//!   --base-url http://127.0.0.1:8080 --agent-dir /runs/8a41/agents/a \
//!   --worker-pgid 41233 --deadline-secs 3600
//! ```

mod adapter;
mod fileedge;
mod signals;
mod synth;
mod transport;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use hacp::envelope::urn;
use tracing::{error, info, warn};

use crate::adapter::{Adapter, AgentPaths, Config};
use crate::signals::Pgid;
use crate::transport::HttpBus;

/// The environment variable carrying the run's per-role bearer token (§13.3).
///
/// Deliberately not a command-line argument. Arguments are world-readable in the
/// process table on every platform this runs on, so a token passed that way is a token
/// shared with every other user on the machine.
const TOKEN_ENV: &str = "HIVE_COLLAB_TOKEN";

#[derive(Parser, Debug)]
#[command(
    name = "hive-adapter",
    about = "A HACP adapter: shuttles one worker's files to and from the collaboration bus",
    long_about = None
)]
struct Args {
    /// The run this adapter belongs to.
    #[arg(long)]
    run_id: String,

    /// The coordinator-assigned role id (`a`, `b`, `api`, …).
    #[arg(long)]
    role: String,

    /// This adapter's agent URN, `urn:hacp:agent:<role>-<run-short>` (§3).
    #[arg(long)]
    agent_urn: String,

    /// Base URL of the coordinator, e.g. `http://127.0.0.1:8080`.
    #[arg(long)]
    base_url: String,

    /// The worker's directory: `agents/<role>/`, holding BRIEF.md, INBOX/, OUTBOX/.
    #[arg(long)]
    agent_dir: PathBuf,

    /// Seconds between polls.
    #[arg(long, default_value_t = 2)]
    poll_interval_secs: u64,

    /// Seconds between heartbeats (§12). The coordinator marks a worker unresponsive
    /// after three of these.
    #[arg(long, default_value_t = 30)]
    heartbeat_secs: u64,

    /// Wall-clock budget for the worker. On expiry its process group is SUSPENDED, not
    /// killed. Omit for no deadline.
    #[arg(long)]
    deadline_secs: Option<u64>,

    /// The worker's process group id, for deadline suspension. Validated: must be
    /// digits only and greater than 1.
    #[arg(long, value_parser = parse_pgid_arg)]
    worker_pgid: Option<Pgid>,

    /// File the worker's supervisor writes its exit code into. Its appearance is how
    /// the adapter learns the worker finished.
    #[arg(long)]
    exit_file: Option<PathBuf>,

    /// The coordinator's URN. `<name>` is a deployment-unique string, never a vendor or
    /// product identity for a *worker* (§3).
    #[arg(long, default_value = "urn:hacp:coordinator:hive")]
    coordinator_urn: String,

    /// A declared capability; repeatable. Declaration is provenance, not proof (§8),
    /// and admission never depends on it.
    #[arg(long = "capability")]
    capabilities: Vec<String>,

    /// Declare that the worker was briefed to write REPORT.json (`report-json`, §8).
    #[arg(long)]
    writes_own_report: bool,

    /// The tree to diff when synthesizing a report. Defaults to the agent directory.
    #[arg(long)]
    repo_dir: Option<PathBuf>,

    /// `id=path` of an artifact whose existence a synthesized report should record;
    /// repeatable. Paths are relative to the repo directory.
    #[arg(long = "artifact", value_parser = parse_artifact)]
    artifacts: Vec<(String, String)>,

    /// The worker's retained log, as §10 requires a report to name.
    #[arg(long, default_value = "agent.log")]
    log_path: String,
}

fn parse_pgid_arg(raw: &str) -> Result<Pgid, String> {
    signals::parse_pgid(raw)
}

fn parse_artifact(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((id, path)) if !id.is_empty() && !path.is_empty() => {
            Ok((id.to_string(), path.to_string()))
        }
        _ => Err(format!("expected id=path, got {raw:?}")),
    }
}

/// Why the shuttle loop ended.
enum Stop {
    /// The worker exited; the code, when the supervisor recorded one.
    WorkerExited(Option<i32>),
    /// A human interrupted the adapter. The worker's state is left as it is.
    Interrupted,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let token = std::env::var(TOKEN_ENV).with_context(|| {
        format!("{TOKEN_ENV} is not set; the run token is passed by environment, never by argument")
    })?;
    if token.trim().is_empty() {
        bail!("{TOKEN_ENV} is empty");
    }

    // Fail before saying hello rather than after: an adapter whose `from` is malformed
    // has its every message rejected by the coordinator's role binding (§13.3), and a
    // startup error is far easier to read than a run of rejections.
    let Some((urn_role, _run_short)) = urn::parse_agent(&args.agent_urn) else {
        bail!(
            "--agent-urn {:?} is not urn:hacp:agent:<role>-<run-short> (§3)",
            args.agent_urn
        );
    };
    if urn_role != args.role {
        bail!(
            "--agent-urn names role {urn_role:?} but --role is {:?}; the token is bound to a role \
             and the mismatch would be rejected on every message",
            args.role
        );
    }
    if !urn::is_coordinator(&args.coordinator_urn) {
        bail!("--coordinator-urn {:?} is not a coordinator URN", args.coordinator_urn);
    }

    let repo_dir = args.repo_dir.clone().unwrap_or_else(|| args.agent_dir.clone());
    let mut capabilities = args.capabilities.clone();
    if capabilities.is_empty() {
        // What any adapter can honestly say about a stock CLI: it has a workspace and a
        // shell. Anything more specific is the deployment's claim to make.
        capabilities = vec!["file-write".to_string(), "shell".to_string()];
    }
    if args.writes_own_report && !capabilities.iter().any(|c| c == "report-json") {
        capabilities.push("report-json".to_string());
    }

    let cfg = Config {
        run_id: args.run_id.clone(),
        role: args.role.clone(),
        agent_urn: args.agent_urn.clone(),
        coordinator_urn: args.coordinator_urn.clone(),
        paths: AgentPaths::new(&args.agent_dir),
        capabilities,
        repo_dir,
        watch_artifacts: args.artifacts.clone(),
        log_path: Some(args.log_path.clone()),
    };

    let bus = HttpBus::new(&args.base_url, &args.run_id, &args.role, token)?;
    let mut adapter = Adapter::new(Box::new(bus), cfg);
    adapter.prepare_workspace().await?;

    // §15: hello, with a manifest, before anything else.
    match adapter.send_hello().await {
        Ok(outcome) if outcome.is_settled() => info!("hello accepted"),
        Ok(outcome) => warn!(?outcome, "the coordinator did not accept hello; continuing anyway"),
        // Not fatal on purpose: the coordinator may simply not be up yet, and a worker
        // that has already started should not lose its adapter over that.
        Err(e) => warn!(error = %e, "hello could not be sent; continuing and retrying by heartbeat"),
    }

    let started = Instant::now();
    let stop = shuttle(&mut adapter, &args, started).await;

    // One last pass: a worker's final messages are usually written moments before it
    // exits, and they are the ones worth losing least.
    match adapter.drain_outbox().await {
        Ok(summary) if !summary.is_empty() => info!(?summary, "final outbox drain"),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "final outbox drain failed"),
    }

    let exit_code = match stop {
        Stop::WorkerExited(code) => code,
        Stop::Interrupted => {
            info!("interrupted; reporting what can be observed");
            None
        }
    };
    let observations = adapter.observe(exit_code, started.elapsed().as_secs()).await;
    match adapter.submit_report(observations).await {
        Ok(true) => info!("the worker's own report was submitted"),
        Ok(false) => info!(
            path = %adapter.paths().fallback_report.display(),
            "an adapter-synthesized report was submitted"
        ),
        Err(e) => error!(error = %e, "the report could not be submitted"),
    }
    Ok(())
}

/// The shuttle loop: poll in, drain out, heartbeat, watch the deadline and the worker.
async fn shuttle(adapter: &mut Adapter, args: &Args, started: Instant) -> Stop {
    let poll_interval = Duration::from_secs(args.poll_interval_secs.max(1));
    let heartbeat_interval = Duration::from_secs(args.heartbeat_secs.max(1));
    let mut last_heartbeat = Instant::now();
    let mut suspended = false;

    loop {
        // A poll or a send that fails is a network event, not a reason to abandon the
        // worker: the loop keeps going and retries. Nothing is dropped, because outbox
        // files survive a failed send and the cursor does not advance past a failed
        // poll.
        if let Err(e) = adapter.poll_once().await {
            warn!(error = %e, cursor = adapter.cursor(), "poll failed; will retry");
        }
        match adapter.drain_outbox().await {
            Ok(summary) if !summary.is_empty() => info!(?summary, "outbox"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "outbox drain failed; will retry"),
        }

        if last_heartbeat.elapsed() >= heartbeat_interval {
            last_heartbeat = Instant::now();
            let state = if suspended { "suspended" } else { "working" };
            if let Err(e) = adapter.send_heartbeat(state, None).await {
                warn!(error = %e, "heartbeat failed");
            }
        }

        if let Some(deadline) = args.deadline_secs {
            if !suspended && started.elapsed().as_secs() >= deadline {
                suspended = true;
                enforce_deadline(adapter, args, deadline).await;
            }
        }

        if let Some(code) = worker_exited(args).await {
            info!(?code, "the worker exited");
            return Stop::WorkerExited(code);
        }

        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = tokio::signal::ctrl_c() => return Stop::Interrupted,
        }
    }
}

/// Deadline enforcement: suspend the worker's process group, and say so on the bus.
///
/// SIGSTOP, never SIGKILL (§13.3, §15). A run that hit its deadline is a run someone is
/// about to be asked to judge, and killing the worker destroys the very state that
/// judgement needs. The adapter keeps running afterwards so the stopped run stays
/// observable, and continuing it is a human's decision (`kill -CONT -<pgid>`).
async fn enforce_deadline(adapter: &mut Adapter, args: &Args, deadline: u64) {
    let note = match args.worker_pgid {
        Some(pgid) => match signals::suspend(pgid) {
            Ok(()) => {
                warn!(%pgid, deadline, "deadline reached; worker process group SUSPENDED, not killed");
                format!("deadline of {deadline}s reached; process group {pgid} suspended with SIGSTOP")
            }
            Err(e) => {
                error!(%pgid, error = %e, "could not suspend the worker's process group");
                format!("deadline of {deadline}s reached; suspending process group {pgid} failed: {e}")
            }
        },
        None => {
            warn!(deadline, "deadline reached but no --worker-pgid was given; nothing to suspend");
            format!("deadline of {deadline}s reached; no process group was given to suspend")
        }
    };
    // The coordinator learns a deadline passed the same way it learns anything else.
    if let Err(e) = adapter.send_heartbeat("suspended", Some(note)).await {
        warn!(error = %e, "could not report the deadline suspension");
    }
}

/// Has the worker finished?
///
/// Two independent signals, because neither alone is reliable. The exit file is written
/// by whatever supervises the worker and carries the exit *code*; the process group
/// tells the adapter the worker is gone but not why. A worker that vanished without an
/// exit file yields `Some(None)` — finished, code unknown — which the fallback report
/// reflects honestly rather than assuming a clean exit.
async fn worker_exited(args: &Args) -> Option<Option<i32>> {
    if let Some(path) = &args.exit_file {
        if let Ok(raw) = tokio::fs::read_to_string(path).await {
            return Some(raw.trim().parse::<i32>().ok());
        }
    }
    if let Some(pgid) = args.worker_pgid {
        if !signals::group_alive(pgid) {
            return Some(None);
        }
    }
    None
}

/// Logs go to stderr so they cannot be confused with anything the worker writes, and
/// carry `(from, to, kind)` rather than bodies: §13.3 keeps message content out of
/// ordinary logs, and an adapter's log is no safer a place for it than a coordinator's.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_arguments_must_be_id_equals_path() {
        assert_eq!(parse_artifact("job-store=src/store"), Ok(("job-store".into(), "src/store".into())));
        assert!(parse_artifact("no-equals-sign").is_err());
        assert!(parse_artifact("=src/store").is_err());
        assert!(parse_artifact("job-store=").is_err());
    }

    #[test]
    fn the_pgid_argument_is_guarded_at_the_boundary() {
        // The same guard the CLI uses, so a dangerous value never becomes a `Pgid`.
        assert!(parse_pgid_arg("0").is_err());
        assert!(parse_pgid_arg("-1").is_err());
        assert!(parse_pgid_arg("").is_err());
        assert!(parse_pgid_arg("4321").is_ok());
    }

    #[test]
    fn the_token_is_not_a_command_line_argument() {
        // §13.3's token must not be readable from the process table. The check is
        // mechanical so a later "just add a --token flag for convenience" fails here.
        use clap::CommandFactory;
        for arg in Args::command().get_arguments() {
            let name = arg.get_id().as_str().to_ascii_lowercase();
            assert!(
                !name.contains("token") && !name.contains("secret") && !name.contains("password"),
                "credential-shaped argument {name:?}: pass it in {TOKEN_ENV} instead"
            );
        }
    }

    #[test]
    fn the_cli_is_internally_consistent() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}
