//! The HIVE runtime — the layer that actually *runs* an HACP/2.0 collaboration.
//!
//! Until this module existed, HIVE had a finished protocol it did not use. The only
//! thing that had ever driven a live 2.0 session was `interop/live/hacp-live.py`, a
//! Python driver that is not HIVE; every line of HIVE's own binding
//! ([`crate::collab`], `hive-adapter`) is built on the frozen HACP 1.1 and cannot open
//! a 2.0 session at all. `docs/PROJECT-RECORD.md` §8 named the gap in one line: what
//! remains is "the HIVE runtime — supervisor loops, spawn/delegate lifecycles, org
//! bootstrapping on top of Core + the profile."
//!
//! This is the first slice of it: two real, heterogeneous stock CLIs, launched and
//! supervised by HIVE, opening a bilateral session, negotiating and freezing a
//! contract, executing it, submitting an artifact, and verifying it — with every
//! protocol decision taken by [`hacp::v2`] and every claim re-checked mechanically.
//!
//! ```text
//!   lifecycle.rs   the ordered run, driving hacp::v2's state machines
//!        │
//!        ├── cli.rs      which stock CLI, and how to start it non-interactively
//!        ├── brief.rs    what an agent is told, and "did it actually write the file?"
//!        ├── edge.rs     the file edge (§12.1): two outboxes, and the transcript
//!        ├── journal.rs  durable v2 inputs, receipts and invocation recovery
//!        ├── hosting.rs  per-role SSH/local placement and checked file transfer
//!        ├── attest.rs   §9.4 — the runtime's own measurements, never the agent's word
//!        └── report.rs   what happened, in a form that cannot flatter itself
//! ```
//!
//! **What this layer adds over the Python driver**, and the reason it belongs in HIVE
//! rather than in a script: every agent invocation is a supervised tmux session
//! ([`crate::collab::session::LocalSessionHost`]) with Tier-1 rules scanned on every output line
//! as it arrives, a timeout that suspends rather than kills, and SIGSTOP to the pane's
//! foreground process group. `subprocess.run` can do none of that.
//!
//! **What it does not do yet:** recursive spawning (the `hive-recursive-pairwise/1`
//! profile's org chart and capability grants), autonomous fleet placement and peer
//! discovery. SSH hosting uses explicit trusted aliases; reachability is not discovery.

pub mod attest;
pub mod brief;
pub mod cli;
pub mod edge;
pub mod lifecycle;
pub mod report;
pub mod journal;
pub mod hosting;
pub mod verification;

pub use cli::AgentCli;
pub use lifecycle::{authorize_continuation, resume_bilateral, resume_configured, run_bilateral, run_configured, RunConfig};
pub use report::{RunOutcome, RunReport, Stage};

/// A self-deleting scratch directory for tests.
///
/// The workspace has no temp-dir dependency and does not need one: `collab::workspace`
/// already settled on this shape, and a test fixture is not worth a crate.
#[cfg(test)]
pub(crate) struct Scratch {
    pub dir: std::path::PathBuf,
}

#[cfg(test)]
impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hive-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }
    pub fn join(&self, p: &str) -> std::path::PathBuf {
        self.dir.join(p)
    }
}

#[cfg(test)]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
