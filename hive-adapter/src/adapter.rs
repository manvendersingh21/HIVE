//! The shuttle itself: files in one direction, envelopes in the other.
//!
//! This is the piece that makes constraint C1 work. Everything protocol-shaped happens
//! here, beside a worker that knows nothing about any of it, so that a stock CLI which
//! can read a prompt and write a JSON file is a conformant HACP participant with zero
//! vendor cooperation.
//!
//! The adapter takes a `Box<dyn Bus>` rather than an HTTP client so that every rule
//! below — dedupe, cursor advance, retry-on-transient, delete-only-after-accept — is
//! exercised in this module's tests without a network.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hacp::envelope::HeartbeatBody;
use hacp::report::{CompletionReport, ReportArtifact};
use hacp::{kinds, CapabilityManifest, Envelope};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::fileedge;
use crate::synth::{self, Observations};
use crate::transport::{Bus, IngestOutcome};

/// Where one worker's files live (§14). Every path is under `root`, which is the only
/// directory the worker may write to.
#[derive(Debug, Clone)]
pub struct AgentPaths {
    pub root: PathBuf,
    pub inbox: PathBuf,
    pub outbox: PathBuf,
    /// Outbox files the adapter refused, kept with the reason beside them rather than
    /// deleted: a worker's malformed message is evidence, and silently dropping it
    /// would leave a human wondering why nothing arrived.
    pub rejected: PathBuf,
    pub report: PathBuf,
    pub fallback_report: PathBuf,
}

impl AgentPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            inbox: root.join("INBOX"),
            outbox: root.join("OUTBOX"),
            rejected: root.join("OUTBOX.rejected"),
            report: root.join("REPORT.json"),
            fallback_report: root.join("REPORT.fallback.json"),
            root,
        }
    }
}

/// Static configuration for one adapter instance.
#[derive(Debug, Clone)]
pub struct Config {
    pub run_id: String,
    /// The role id the coordinator assigned. Logged, and the identity the run's token
    /// is bound to (§13.3).
    pub role: String,
    pub agent_urn: String,
    /// Where a message with no explicit `to` goes. The coordinator, always: it is the
    /// only actor every participant may address without knowing anything about it.
    pub coordinator_urn: String,
    pub paths: AgentPaths,
    /// Declared, not proven (§8). An adapter cannot introspect a stock tool.
    pub capabilities: Vec<String>,
    /// The tree `git diff --numstat` is run in when synthesizing a report.
    pub repo_dir: PathBuf,
    /// Artifact ids and paths the adapter checks for existence in a fallback report.
    /// Paths are resolved relative to `repo_dir`.
    pub watch_artifacts: Vec<(String, String)>,
    /// The worker's retained log, as §10 requires the report to name.
    pub log_path: Option<String>,
}

/// What one outbox drain did, for logging and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainSummary {
    pub sent: usize,
    pub duplicates: usize,
    pub refused: usize,
    /// Files left in place because the send could not be completed. They are retried on
    /// the next pass: a transient network failure must not lose a worker's message.
    pub retained: usize,
}

impl DrainSummary {
    /// Whether anything at all happened, so an idle loop stays quiet in the log.
    pub fn is_empty(&self) -> bool {
        self.sent == 0 && self.duplicates == 0 && self.refused == 0 && self.retained == 0
    }
}

pub struct Adapter {
    bus: Box<dyn Bus>,
    cfg: Config,
    /// The poll cursor: the highest sequence number this adapter has consumed.
    cursor: u64,
    /// Inbound `message_id`s already written. Delivery is at-least-once and the
    /// coordinator deduplicates, but §13.1 requires dedupe *at both ends*, and the
    /// adapter is the end that turns a message into a file a worker will act on.
    seen: HashSet<String>,
    /// Outbox path to the `message_id` first minted for it. A retry after a failed POST
    /// reuses the id, so a send that actually landed is recognized as a duplicate
    /// instead of being stored twice under two ids.
    minted: HashMap<PathBuf, String>,
    /// Fallback ordinal for INBOX filenames when the binding returns no sequence
    /// numbers. Preserves order; is not the bus's number.
    local_seq: u64,
    warned_about_missing_seq: bool,
}

impl Adapter {
    pub fn new(bus: Box<dyn Bus>, cfg: Config) -> Self {
        Self {
            bus,
            cfg,
            cursor: 0,
            seen: HashSet::new(),
            minted: HashMap::new(),
            local_seq: 0,
            warned_about_missing_seq: false,
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    pub fn paths(&self) -> &AgentPaths {
        &self.cfg.paths
    }

    /// Create the directories the worker reads and writes, so it never has to.
    pub async fn prepare_workspace(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.cfg.paths.inbox).await?;
        tokio::fs::create_dir_all(&self.cfg.paths.outbox).await?;
        info!(
            role = %self.cfg.role,
            agent = %self.cfg.agent_urn,
            root = %self.cfg.paths.root.display(),
            "file edge ready"
        );
        Ok(())
    }

    /// The manifest, built from declared capabilities only.
    ///
    /// It carries no vendor, product, or model identity, and there is no code path that
    /// could add one: the adapter is never told what tool runs beside it. That mapping
    /// lives only in the coordinator's private run record (§3), and since the
    /// coordinator relays manifests to the arbiter, a manifest is exactly how such a
    /// leak would travel.
    pub fn manifest(&self) -> CapabilityManifest {
        CapabilityManifest {
            agent: self.cfg.agent_urn.clone(),
            capabilities: self.cfg.capabilities.clone(),
            declared_by: "adapter-default".to_string(),
            extra: serde_json::Map::new(),
        }
    }

    /// Send `hello` with the capability manifest. §15 requires this before anything
    /// else, and §8 requires admission not to depend on what it contains.
    pub async fn send_hello(&mut self) -> Result<IngestOutcome> {
        let body = serde_json::to_value(self.manifest()).context("serializing the manifest")?;
        self.send(kinds::HELLO, body, None).await
    }

    /// Send a heartbeat (§12). Silence from a live worker is a signal to check, not a
    /// verdict, and this is what makes the difference visible to the coordinator.
    pub async fn send_heartbeat(&mut self, state: &str, note: Option<String>) -> Result<IngestOutcome> {
        let body = serde_json::to_value(HeartbeatBody { state: state.to_string(), note })
            .context("serializing the heartbeat")?;
        self.send(kinds::HEARTBEAT, body, None).await
    }

    /// Build and ingest one adapter-authored envelope.
    async fn send(&mut self, kind: &str, body: Value, to: Option<&str>) -> Result<IngestOutcome> {
        let envelope = Envelope::new(
            &self.cfg.run_id,
            &self.cfg.agent_urn,
            to.unwrap_or(&self.cfg.coordinator_urn),
            kind,
            body,
        );
        envelope
            .validate()
            .map_err(|e| anyhow::anyhow!("the adapter built an invalid envelope: {e}"))?;
        let outcome = self.bus.ingest(&envelope).await?;
        // (from, to, kind) only. §13.3 keeps bodies out of ordinary logs, and an
        // adapter's log is no safer a place for them than a coordinator's.
        debug!(kind, to = %envelope.to, ?outcome, "sent");
        Ok(outcome)
    }

    /// One poll: fetch, deduplicate, write INBOX files, advance the cursor.
    ///
    /// Returns how many new files were written.
    pub async fn poll_once(&mut self) -> Result<usize> {
        let page = self.bus.poll(self.cursor).await?;
        if let Some(state) = &page.state {
            // Logged, never branched on: what the run's state means is the
            // coordinator's and the arbiter's business, not the shuttle's.
            debug!(run_state = %state, "poll");
        }
        let mut written = 0usize;
        let mut highest = self.cursor;

        for delivered in &page.messages {
            let envelope = &delivered.envelope;
            if let Some(seq) = delivered.seq {
                highest = highest.max(seq);
            }
            if !self.seen.insert(envelope.message_id.clone()) {
                debug!(message_id = %envelope.message_id, "duplicate delivery ignored");
                continue;
            }
            let seq = match delivered.seq {
                Some(seq) => {
                    self.local_seq = self.local_seq.max(seq);
                    seq
                }
                None => {
                    if !self.warned_about_missing_seq {
                        self.warned_about_missing_seq = true;
                        warn!(
                            "the coordinator returned messages without sequence numbers; \
                             INBOX files will be numbered locally and the poll cursor can \
                             only advance from the page cursor"
                        );
                    }
                    self.local_seq += 1;
                    self.local_seq
                }
            };
            let path = fileedge::write_inbox(&self.cfg.paths.inbox, seq, envelope)
                .await
                .with_context(|| format!("writing INBOX message {seq}"))?;
            info!(
                seq,
                from = %envelope.from,
                to = %envelope.to,
                kind = %envelope.kind,
                path = %path.display(),
                "delivered to the worker"
            );
            written += 1;
        }

        // The page cursor wins when the binding supplies one: it accounts for messages
        // filtered out before delivery (peer traffic this agent is not party to), which
        // the highest delivered seq does not.
        self.cursor = page.cursor.unwrap_or(highest).max(highest);
        Ok(written)
    }

    /// Send everything the worker has left in OUTBOX.
    pub async fn drain_outbox(&mut self) -> Result<DrainSummary> {
        let mut summary = DrainSummary::default();
        for path in fileedge::list_outbox(&self.cfg.paths.outbox).await? {
            let raw = match tokio::fs::read_to_string(&path).await {
                Ok(raw) => raw,
                // A file that vanished between listing and reading is not an error: the
                // worker may still be writing it, and the next pass will find it.
                Err(e) => {
                    debug!(path = %path.display(), error = %e, "outbox file unreadable this pass");
                    summary.retained += 1;
                    continue;
                }
            };

            let item = match fileedge::parse_outbox(&raw) {
                Ok(item) => item,
                Err(e) => {
                    self.refuse(&path, &e.to_string()).await?;
                    summary.refused += 1;
                    continue;
                }
            };

            let message_id = self
                .minted
                .entry(path.clone())
                .or_insert_with(|| format!("m-{}", uuid::Uuid::new_v4()))
                .clone();

            let envelope = match fileedge::envelope_for(
                &item,
                &self.cfg.run_id,
                &self.cfg.agent_urn,
                &self.cfg.coordinator_urn,
                message_id,
            ) {
                Ok(envelope) => envelope,
                Err(e) => {
                    self.refuse(&path, &e.to_string()).await?;
                    summary.refused += 1;
                    continue;
                }
            };
            // The spec's own shape check, run before the network sees it: `in_reply_to`
            // missing on a kind that requires it is the common case, and telling the
            // worker locally is more use than a coordinator rejection it cannot read.
            if let Err(e) = envelope.validate() {
                self.refuse(&path, &e.to_string()).await?;
                summary.refused += 1;
                continue;
            }

            match self.bus.ingest(&envelope).await {
                Ok(IngestOutcome::Accepted { seq }) => {
                    info!(kind = %envelope.kind, to = %envelope.to, seq, "accepted");
                    self.settle(&path).await;
                    summary.sent += 1;
                }
                Ok(IngestOutcome::Duplicate { seq }) => {
                    // An earlier attempt landed after all; the file has done its job.
                    info!(kind = %envelope.kind, seq, "already known to the coordinator");
                    self.settle(&path).await;
                    summary.duplicates += 1;
                }
                Ok(IngestOutcome::Rejected { code, detail }) => {
                    self.refuse(&path, &format!("the coordinator refused it: {code}: {detail}"))
                        .await?;
                    summary.refused += 1;
                }
                Err(e) => {
                    // Transport failure. Keep the file: §13.2's contract with the
                    // worker is that a written message is sent eventually, and the
                    // reused message_id makes the retry safe.
                    warn!(path = %path.display(), error = %e, "send failed; will retry");
                    summary.retained += 1;
                }
            }
        }
        Ok(summary)
    }

    /// Remove a settled outbox file. Only reached once the bus has taken the message.
    async fn settle(&mut self, path: &Path) {
        self.minted.remove(path);
        if let Err(e) = tokio::fs::remove_file(path).await {
            // If removal fails the file is resent next pass under the same id, which
            // the coordinator deduplicates. Noisy, not lossy.
            warn!(path = %path.display(), error = %e, "could not remove a sent outbox file");
        }
    }

    /// Move a refused file aside with the reason beside it.
    async fn refuse(&mut self, path: &Path, reason: &str) -> Result<()> {
        warn!(path = %path.display(), reason, "outbox file refused");
        self.minted.remove(path);
        tokio::fs::create_dir_all(&self.cfg.paths.rejected).await?;
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let moved = self.cfg.paths.rejected.join(&name);
        if let Err(e) = tokio::fs::rename(path, &moved).await {
            warn!(path = %path.display(), error = %e, "could not move a refused outbox file aside");
        }
        let note = self.cfg.paths.rejected.join(format!("{name}.why.txt"));
        if let Err(e) = tokio::fs::write(&note, format!("{reason}\n")).await {
            warn!(path = %note.display(), error = %e, "could not record why a file was refused");
        }
        Ok(())
    }

    /// End of work: submit the worker's report, or synthesize one (§10).
    ///
    /// Returns whether the report was the worker's own.
    pub async fn submit_report(&mut self, obs: Observations) -> Result<bool> {
        if let Ok(raw) = tokio::fs::read_to_string(&self.cfg.paths.report).await {
            match serde_json::from_str::<Value>(&raw) {
                Ok(value) if value.is_object() => {
                    // Relayed byte for byte. The adapter does not fill in fields the
                    // worker forgot, does not normalize `source`, and does not judge
                    // the outcome — all of that is content, and content-blindness (§2)
                    // is what keeps every claim in a run attributable to a named agent.
                    // The cost is real: a report missing a required field is refused by
                    // the coordinator rather than quietly repaired here.
                    if value.get("source").and_then(Value::as_str) != Some("agent") {
                        warn!(
                            "REPORT.json does not declare source \"agent\"; relaying it \
                             unchanged, which the coordinator may refuse"
                        );
                    }
                    self.send(kinds::REPORT_SUBMITTED, value, None).await?;
                    info!("submitted the worker's own report");
                    return Ok(true);
                }
                Ok(_) => warn!("REPORT.json is not a JSON object; synthesizing a report instead"),
                Err(e) => {
                    warn!(error = %e, "REPORT.json is not valid JSON; synthesizing a report instead")
                }
            }
        }

        let report = synth::synthesize(&self.cfg.agent_urn, &obs);
        self.write_fallback(&report).await?;
        let body = serde_json::to_value(&report).context("serializing the synthesized report")?;
        self.send(kinds::REPORT_SUBMITTED, body, None).await?;
        info!(outcome = %report.outcome, "submitted an adapter-synthesized report");
        Ok(false)
    }

    async fn write_fallback(&self, report: &CompletionReport) -> Result<()> {
        let json = serde_json::to_vec_pretty(report)?;
        tokio::fs::write(&self.cfg.paths.fallback_report, json)
            .await
            .with_context(|| format!("writing {}", self.cfg.paths.fallback_report.display()))?;
        Ok(())
    }

    /// Gather what can be observed from outside the work (§10).
    pub async fn observe(&self, exit_code: Option<i32>, duration_secs: u64) -> Observations {
        let mut artifacts = Vec::new();
        for (artifact_id, rel) in &self.cfg.watch_artifacts {
            let path = self.cfg.repo_dir.join(rel);
            artifacts.push(ReportArtifact {
                artifact_id: artifact_id.clone(),
                path: rel.clone(),
                // No digest: deciding what to hash for an artifact that may be a
                // directory is a decision the contract makes, and the adapter does not
                // read the contract.
                sha256: None,
                exists: tokio::fs::metadata(&path).await.is_ok(),
            });
        }
        Observations {
            exit_code,
            diffstat: self.git_numstat().await,
            artifacts,
            log_path: self.cfg.log_path.clone(),
            duration_secs,
        }
    }

    /// `git diff --numstat HEAD` in the worker's tree. `None` when it is not a git tree
    /// or git is unavailable — an absent diff is reported as absent, never as zero.
    async fn git_numstat(&self) -> Option<hacp::report::DiffStat> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.cfg.repo_dir)
            .args(["diff", "--numstat", "HEAD"])
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            debug!("git diff --numstat did not succeed; no diffstat will be reported");
            return None;
        }
        Some(synth::parse_numstat(&String::from_utf8_lossy(&output.stdout)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{BoxFuture, Delivered, PollPage};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// A bus that answers from a script and records what it was given. Cloneable so a
    /// test keeps a handle after the adapter has boxed one.
    #[derive(Clone, Default)]
    struct FakeBus {
        inner: Arc<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        pages: Mutex<Vec<PollPage>>,
        ingested: Mutex<Vec<Envelope>>,
        /// Outcomes for successive `ingest` calls; exhausted means "accept".
        outcomes: Mutex<Vec<std::result::Result<IngestOutcome, String>>>,
    }

    impl FakeBus {
        fn with_pages(pages: Vec<PollPage>) -> Self {
            let bus = Self::default();
            *bus.inner.pages.lock().unwrap() = pages;
            bus
        }

        fn with_outcomes(outcomes: Vec<std::result::Result<IngestOutcome, String>>) -> Self {
            let bus = Self::default();
            *bus.inner.outcomes.lock().unwrap() = outcomes;
            bus
        }

        fn ingested(&self) -> Vec<Envelope> {
            self.inner.ingested.lock().unwrap().clone()
        }
    }

    impl Bus for FakeBus {
        fn ingest<'a>(&'a self, envelope: &'a Envelope) -> BoxFuture<'a, Result<IngestOutcome>> {
            Box::pin(async move {
                self.inner.ingested.lock().unwrap().push(envelope.clone());
                let mut outcomes = self.inner.outcomes.lock().unwrap();
                if outcomes.is_empty() {
                    return Ok(IngestOutcome::Accepted { seq: None });
                }
                match outcomes.remove(0) {
                    Ok(outcome) => Ok(outcome),
                    Err(message) => Err(anyhow::anyhow!(message)),
                }
            })
        }

        fn poll(&self, _since: u64) -> BoxFuture<'_, Result<PollPage>> {
            Box::pin(async move {
                let mut pages = self.inner.pages.lock().unwrap();
                if pages.is_empty() {
                    Ok(PollPage::default())
                } else {
                    Ok(pages.remove(0))
                }
            })
        }
    }

    /// A workspace that removes itself, so a failing assertion does not leave litter
    /// and does not have to be unwound by hand in every test.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "hive-adapter-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(dir.join("OUTBOX")).unwrap();
            std::fs::create_dir_all(dir.join("INBOX")).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn config(root: &Path) -> Config {
        Config {
            run_id: "run-8a41".into(),
            role: "a".into(),
            agent_urn: "urn:hacp:agent:a-8a41".into(),
            coordinator_urn: "urn:hacp:coordinator:hive".into(),
            paths: AgentPaths::new(root),
            capabilities: vec!["file-write".into(), "shell".into()],
            repo_dir: root.to_path_buf(),
            watch_artifacts: Vec::new(),
            log_path: Some("agents/a/agent.log".into()),
        }
    }

    fn delivered(seq: u64, message_id: &str, kind: &str) -> Delivered {
        Delivered {
            seq: Some(seq),
            envelope: Envelope {
                protocol: hacp::PROTOCOL.into(),
                message_id: message_id.into(),
                run_id: "run-8a41".into(),
                from: "urn:hacp:coordinator:hive".into(),
                to: "urn:hacp:agent:a-8a41".into(),
                kind: hacp::MessageKind::new(kind),
                in_reply_to: None,
                timestamp: chrono::Utc::now(),
                body: json!({}),
            },
        }
    }

    #[tokio::test]
    async fn hello_is_a_manifest_with_no_vendor_identity() {
        let ws = Workspace::new("hello");
        let bus = FakeBus::default();
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        adapter.send_hello().await.unwrap();

        let sent = bus.ingested();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind.as_str(), "hello");
        assert_eq!(sent[0].from, "urn:hacp:agent:a-8a41");
        assert_eq!(sent[0].to, "urn:hacp:coordinator:hive");
        assert_eq!(sent[0].body["agent"], json!("urn:hacp:agent:a-8a41"));
        assert_eq!(sent[0].body["capabilities"], json!(["file-write", "shell"]));
        // §3 and §8: a manifest reaches the arbiter, so it is exactly the wrong place
        // for anything that identifies the tool behind the URN.
        for forbidden in ["vendor", "product", "model", "tool", "version"] {
            assert!(sent[0].body.get(forbidden).is_none(), "manifest leaked {forbidden}");
        }
    }

    #[tokio::test]
    async fn heartbeat_carries_only_a_state_label() {
        let ws = Workspace::new("heartbeat");
        let bus = FakeBus::default();
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        adapter.send_heartbeat("working", None).await.unwrap();

        let sent = bus.ingested();
        assert_eq!(sent[0].kind.as_str(), "heartbeat");
        assert_eq!(sent[0].body, json!({"state": "working"}));
    }

    #[tokio::test]
    async fn writes_inbox_files_dedupes_and_advances_the_cursor() {
        let ws = Workspace::new("inbox");
        let bus = FakeBus::with_pages(vec![
            PollPage {
                cursor: Some(2),
                state: Some("working".into()),
                messages: vec![
                    delivered(1, "m-1", "run.started"),
                    delivered(2, "m-2", "contract.frozen"),
                ],
            },
            // At-least-once: the coordinator redelivers m-2 with a new message.
            PollPage {
                cursor: Some(3),
                state: Some("working".into()),
                messages: vec![delivered(2, "m-2", "contract.frozen"), delivered(3, "m-3", "question")],
            },
        ]);
        let mut adapter = Adapter::new(Box::new(bus), config(ws.path()));

        assert_eq!(adapter.poll_once().await.unwrap(), 2);
        assert_eq!(adapter.cursor(), 2);
        assert_eq!(adapter.poll_once().await.unwrap(), 1, "a redelivery must not be rewritten");
        assert_eq!(adapter.cursor(), 3);

        let mut names: Vec<String> = std::fs::read_dir(ws.path().join("INBOX"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "000001-run.started.json".to_string(),
                "000002-contract.frozen.json".to_string(),
                "000003-question.json".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn sends_a_good_outbox_file_and_removes_it_only_then() {
        let ws = Workspace::new("outbox-ok");
        std::fs::write(
            ws.path().join("OUTBOX/001-work.started.json"),
            r#"{"kind": "work.started", "body": {}}"#,
        )
        .unwrap();
        let bus = FakeBus::default();
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        let summary = adapter.drain_outbox().await.unwrap();

        assert_eq!(summary, DrainSummary { sent: 1, duplicates: 0, refused: 0, retained: 0 });
        assert!(!ws.path().join("OUTBOX/001-work.started.json").exists());
        let sent = bus.ingested();
        assert_eq!(sent[0].run_id, "run-8a41");
        assert_eq!(sent[0].from, "urn:hacp:agent:a-8a41");
        assert!(sent[0].message_id.starts_with("m-"));
    }

    #[tokio::test]
    async fn keeps_the_file_and_reuses_the_message_id_when_a_send_fails() {
        let ws = Workspace::new("outbox-retry");
        let path = ws.path().join("OUTBOX/001-question.json");
        std::fs::write(&path, r#"{"kind": "question", "body": {"about": "x", "text": "y"}}"#).unwrap();

        let bus = FakeBus::with_outcomes(vec![
            Err("connection refused".to_string()),
            Ok(IngestOutcome::Accepted { seq: Some(4) }),
        ]);
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        let first = adapter.drain_outbox().await.unwrap();
        assert_eq!(first.retained, 1, "a transient failure must not lose the message");
        assert!(path.exists(), "the file must survive a failed send");

        let second = adapter.drain_outbox().await.unwrap();
        assert_eq!(second.sent, 1);
        assert!(!path.exists());

        let sent = bus.ingested();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0].message_id, sent[1].message_id,
            "a retry must reuse the id so the coordinator can deduplicate it"
        );
    }

    #[tokio::test]
    async fn moves_a_malformed_outbox_file_aside_with_the_reason() {
        let ws = Workspace::new("outbox-bad");
        std::fs::write(ws.path().join("OUTBOX/001-broken.json"), "{ not json").unwrap();
        std::fs::write(ws.path().join("OUTBOX/002-nokind.json"), r#"{"body": {}}"#).unwrap();
        // `peer.answer` requires in_reply_to (§5); the worker omitted it.
        std::fs::write(
            ws.path().join("OUTBOX/003-noreply.json"),
            r#"{"kind": "peer.answer", "to": "urn:hacp:agent:b-8a41", "body": {"text": "yes"}}"#,
        )
        .unwrap();
        let bus = FakeBus::default();
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        let summary = adapter.drain_outbox().await.unwrap();

        assert_eq!(summary.refused, 3);
        assert_eq!(summary.sent, 0);
        assert!(bus.ingested().is_empty(), "nothing malformed may reach the bus");
        assert!(ws.path().join("OUTBOX.rejected/001-broken.json").exists());
        let why =
            std::fs::read_to_string(ws.path().join("OUTBOX.rejected/002-nokind.json.why.txt")).unwrap();
        assert!(why.contains("kind"), "the reason must name the field; got: {why}");
        let why3 =
            std::fs::read_to_string(ws.path().join("OUTBOX.rejected/003-noreply.json.why.txt")).unwrap();
        assert!(why3.contains("in_reply_to"), "got: {why3}");
    }

    #[tokio::test]
    async fn relays_a_worker_report_unchanged() {
        let ws = Workspace::new("report-own");
        let written = json!({
            "report_id": "r-1",
            "agent": "urn:hacp:agent:a-8a41",
            "outcome": "success",
            "summary": "did the thing",
            "contract_status": "satisfied",
            "source": "agent",
            "a_field_this_build_does_not_know": 42
        });
        std::fs::write(ws.path().join("REPORT.json"), serde_json::to_vec(&written).unwrap()).unwrap();
        let bus = FakeBus::default();
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        let own = adapter.submit_report(Observations::default()).await.unwrap();

        assert!(own, "a present REPORT.json is the worker's own report");
        let sent = bus.ingested();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind.as_str(), "report.submitted");
        assert_eq!(sent[0].body, written, "the body must not be interpreted or altered");
        assert!(!ws.path().join("REPORT.fallback.json").exists());
    }

    #[tokio::test]
    async fn synthesizes_a_report_when_the_worker_wrote_none() {
        let ws = Workspace::new("report-fallback");
        let bus = FakeBus::default();
        let mut adapter = Adapter::new(Box::new(bus.clone()), config(ws.path()));

        let obs = Observations { exit_code: Some(0), duration_secs: 12, ..Default::default() };
        let own = adapter.submit_report(obs).await.unwrap();
        assert!(!own);

        let raw = std::fs::read_to_string(ws.path().join("REPORT.fallback.json")).unwrap();
        let written: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(written["source"], json!("adapter-synthesized"));
        // Asserted through the type rather than against the literal `"not-reported"`
        // the spec prints: `hacp`'s `ContractStatus` derives snake_case and so writes
        // `not_reported` on the wire, disagreeing with §10 and with its own `Display`.
        // That is a bug in the frozen crate, not something for this adapter to paper
        // over by rewriting the field — see the escalation note in the parcel report.
        let parsed: CompletionReport = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.contract_status, hacp::report::ContractStatus::NotReported);

        let sent = bus.ingested();
        assert_eq!(sent[0].kind.as_str(), "report.submitted");
        assert_eq!(sent[0].body["source"], json!("adapter-synthesized"));
    }
}
