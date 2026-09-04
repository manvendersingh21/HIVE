//! The on-disk run workspace (`spec/HACP.md` §14) and the file edge a stock CLI actually
//! sees (§13.2).
//!
//! This is constraint C1 made concrete: the entire protocol surface a worker touches is
//! `BRIEF.md` to read, `INBOX/` to read, `OUTBOX/` to write, and `REPORT.json` to write.
//! A tool that can read a prompt and write a JSON file is conformant, with no cooperation
//! from its vendor. Everything else in this file exists to keep that surface honest.
//!
//! Two habits run through it:
//!
//! * **Assume the worker can only `ls`.** Inbox names are zero-padded so lexical order is
//!   message order, and a message is renamed into place rather than written in place, so a
//!   directory listing never shows a half-written file.
//! * **Shape, never meaning.** [`FileRunStore::drain_outbox`] checks that a file is a JSON
//!   object with a `kind` and a `body` and stops there. It does not repair a malformed
//!   file, and it does not drop one — a body the store does not understand is exactly what
//!   §5's forward compatibility is for.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hacp::report::CompletionReport;
use hacp::Envelope;
use tokio::fs;

use super::{AgentPaths, Outbound, Result, RunStore};

/// The run workspace rooted at one directory.
#[derive(Debug, Clone)]
pub struct FileRunStore {
    root: PathBuf,
}

impl FileRunStore {
    /// A store over `<run_root>/`. Nothing is created until [`RunStore::init_run`].
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn agent_root(&self, role: &str) -> Result<PathBuf> {
        Ok(self.root.join("agents").join(safe_segment(role, "role")?))
    }

    fn paths_for(&self, role: &str) -> Result<AgentPaths> {
        let root = self.agent_root(role)?;
        Ok(AgentPaths {
            role: role.to_string(),
            brief: root.join("BRIEF.md"),
            inbox: root.join("INBOX"),
            outbox: root.join("OUTBOX"),
            // §14's tree does not name this directory, but it does say a worker writes
            // ONLY inside `agents/<role>/`, so the worker's checkout has to live there.
            // Contract paths stay relative to `repo/`; the worker realizes them in here
            // and the coordinator merges into `integration/`.
            workspace: root.join("workspace"),
            log: root.join("agent.log"),
            root,
        })
    }
}

#[async_trait]
impl RunStore for FileRunStore {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn init_run(&self, run_id: &str, goal: &str) -> Result<()> {
        for dir in ["", "repo", "integration", "agents"] {
            fs::create_dir_all(self.root.join(dir)).await?;
        }
        // §13.3 issues one token per run per role, delivered out of band. None of them
        // appears here, and none should ever be added: `run.json` is world-readable to
        // anything that can see the run directory, including every worker.
        let run = serde_json::json!({
            "run_id": run_id,
            "goal": goal,
            "state": "formation",
            "roles": [],
            "created_at": chrono::Utc::now().to_rfc3339(),
        });
        write_json(&self.root.join("run.json"), &run).await
    }

    async fn init_agent(&self, role: &str, brief: &str) -> Result<AgentPaths> {
        let paths = self.paths_for(role)?;
        for dir in [
            &paths.root,
            &paths.inbox,
            &paths.outbox,
            &paths.workspace,
            &paths.root.join("artifacts"),
        ] {
            fs::create_dir_all(dir).await?;
        }
        fs::write(&paths.brief, brief).await?;
        // Created empty so supervision can attach a tail before the session starts;
        // otherwise the watchdog races the worker's first line of output.
        if !fs::try_exists(&paths.log).await? {
            fs::write(&paths.log, b"").await?;
        }
        Ok(paths)
    }

    async fn agent_paths(&self, role: &str) -> Result<AgentPaths> {
        let paths = self.paths_for(role)?;
        if !fs::try_exists(&paths.root).await? {
            anyhow::bail!("no agent {role:?} in this run workspace");
        }
        Ok(paths)
    }

    async fn write_inbox(&self, role: &str, seq: u64, envelope: &Envelope) -> Result<()> {
        let paths = self.agent_paths(role).await?;
        // Six digits, so lexical order is message order for a worker whose only tool is
        // `ls`. A run that exceeds a million messages sorts wrong rather than colliding,
        // which is the failure worth having of the two.
        let name = format!("{seq:06}-{}.json", file_safe(envelope.kind.as_str()));
        let final_path = paths.inbox.join(&name);
        // Dot-prefixed so `ls` does not show it, then renamed: a worker polling its inbox
        // must never read a file that is still being written.
        let staging = paths.inbox.join(format!(".{name}.partial"));
        fs::write(&staging, serde_json::to_vec_pretty(envelope)?).await?;
        fs::rename(&staging, &final_path).await?;
        Ok(())
    }

    async fn drain_outbox(&self, role: &str) -> Result<Vec<Outbound>> {
        let paths = self.agent_paths(role).await?;
        let mut files = Vec::new();
        let mut entries = fs::read_dir(&paths.outbox).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Dot-files and anything not ending `.json` are a worker's own scratch, or a
            // write still in progress. Claiming them would be interpreting the directory
            // rather than reading the mailbox §13.2 defines.
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            if entry.file_type().await?.is_file() {
                files.push(path);
            }
        }
        // Deterministic order so a worker that names its files can control emission order.
        files.sort();

        // Every file is parsed before any file is removed. `Result<Vec<Outbound>>` has no
        // way to say "these drained, that one is broken", so a partial drain would delete
        // messages it never returned. The cost is that one malformed file blocks the
        // mailbox until an operator looks at it — visible and recoverable, unlike silent
        // loss, but it is a real limitation and not a nicety.
        let mut drained = Vec::with_capacity(files.len());
        for path in &files {
            drained.push(parse_outbound(path).await?);
        }
        for path in &files {
            fs::remove_file(path).await?;
        }
        Ok(drained)
    }

    async fn read_report(&self, role: &str) -> Result<Option<CompletionReport>> {
        let paths = self.agent_paths(role).await?;
        let path = paths.root.join("REPORT.json");
        let Ok(bytes) = fs::read(&path).await else {
            // Absent is a legitimate answer (§10): the adapter synthesizes a fallback and
            // marks it `adapter-synthesized` rather than pretending the worker reported.
            return Ok(None);
        };
        let report = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("{}: not a CompletionReport: {e}", path.display()))?;
        Ok(Some(report))
    }

    async fn record(&self, name: &str, value: &serde_json::Value) -> Result<()> {
        let name = safe_segment(name, "record name")?;
        write_json(&self.root.join(name), value).await
    }

    async fn append_amendment(&self, entry: &serde_json::Value) -> Result<()> {
        // Compact, so one amendment is one line; `serde_json` escapes any newline inside a
        // string, which is what keeps the file readable with `wc -l` and `tail`.
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let path = self.root.join("AMENDMENTS.jsonl");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        use tokio::io::AsyncWriteExt;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Shape check for one OUTBOX file (§13.2): a JSON object with a non-empty string `kind`
/// and a `body`. The body is carried through as-is — reading it here would make the store
/// a participant in the conversation instead of a mailbox.
async fn parse_outbound(path: &Path) -> Result<Outbound> {
    let bytes = fs::read(path).await?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("{}: not valid JSON: {e}", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{}: expected a JSON object", path.display()))?;
    let kind = object
        .get("kind")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{}: missing a non-empty \"kind\"", path.display()))?;
    // Not defaulted to `{}`: inventing a body for a worker is repair, and §2 forbids it.
    let body = object
        .get("body")
        .ok_or_else(|| anyhow::anyhow!("{}: missing \"body\"", path.display()))?;
    Ok(Outbound {
        source: path.to_path_buf(),
        kind: kind.to_string(),
        body: body.clone(),
    })
}

/// Reject anything that is not a single path component.
///
/// §14 says a worker writes only inside `agents/<role>/`. A role id of `..` would make
/// that untrue, and the check belongs here because this is the only place a role becomes
/// a path.
fn safe_segment<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    let bad = value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0');
    if bad {
        anyhow::bail!("{what} {value:?} is not a single path component");
    }
    Ok(value)
}

/// Make a message kind safe to use as a filename.
///
/// Kinds are open-ended strings (§5), so a future or hostile one may contain a separator.
/// Substituting is right here and wrong in [`parse_outbound`]: the filename is this
/// store's own bookkeeping, while the kind on the bus is the sender's word.
fn file_safe(kind: &str) -> String {
    let cleaned: String = kind
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

async fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hacp::envelope::urn;
    use serde_json::json;

    struct Fixture {
        store: FileRunStore,
        // Held for the lifetime of the test; dropping it removes the tree.
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!("hive-runstore-{}", uuid::Uuid::new_v4()));
        let store = FileRunStore::new(&dir);
        store.init_run("run-8a41", "ship the thing").await.unwrap();
        Fixture { store, dir }
    }

    async fn write_outbox(store: &FileRunStore, role: &str, name: &str, contents: &str) {
        let paths = store.agent_paths(role).await.unwrap();
        fs::write(paths.outbox.join(name), contents).await.unwrap();
    }

    #[tokio::test]
    async fn lays_out_the_workspace_and_writes_no_token() {
        let f = fixture().await;
        for dir in ["repo", "integration", "agents"] {
            assert!(f.store.root().join(dir).is_dir(), "{dir} missing");
        }
        let run: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.store.root().join("run.json")).unwrap())
                .unwrap();
        assert_eq!(run["goal"], "ship the thing");
        // §13.3's tokens are delivered out of band; nothing token-shaped may be on disk.
        let text = run.to_string().to_lowercase();
        for word in ["token", "secret", "credential"] {
            assert!(!text.contains(word), "run.json mentions {word}: {text}");
        }
    }

    #[tokio::test]
    async fn an_agent_gets_a_mailbox_a_brief_and_a_log() {
        let f = fixture().await;
        let paths = f.store.init_agent("api", "# You are the api\n").await.unwrap();
        assert!(paths.inbox.is_dir());
        assert!(paths.outbox.is_dir());
        assert!(paths.workspace.is_dir());
        assert!(paths.root.join("artifacts").is_dir());
        assert!(paths.log.is_file(), "log must exist before the tail attaches");
        assert_eq!(
            std::fs::read_to_string(&paths.brief).unwrap(),
            "# You are the api\n"
        );
        assert!(paths.root.starts_with(f.store.root().join("agents")));
    }

    #[tokio::test]
    async fn a_role_cannot_escape_the_agents_directory() {
        let f = fixture().await;
        for role in ["..", "../../etc", "", "a/b"] {
            assert!(
                f.store.init_agent(role, "brief").await.is_err(),
                "role {role:?} was accepted"
            );
        }
    }

    #[tokio::test]
    async fn inbox_filenames_sort_into_message_order() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        let paths = f.store.agent_paths("api").await.unwrap();

        for seq in [2u64, 10, 1, 100] {
            let env = Envelope::new(
                "run-8a41",
                urn::coordinator("hive"),
                urn::agent("api", "8a41"),
                "role.offer",
                json!({"role_id": "api"}),
            );
            f.store.write_inbox("api", seq, &env).await.unwrap();
        }

        let mut names: Vec<String> = std::fs::read_dir(&paths.inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "000001-role.offer.json",
                "000002-role.offer.json",
                "000010-role.offer.json",
                "000100-role.offer.json",
            ]
        );

        // The whole envelope lands, not just the body: the worker is told who is asking.
        let first =
            std::fs::read_to_string(paths.inbox.join("000001-role.offer.json")).unwrap();
        let back: Envelope = serde_json::from_str(&first).unwrap();
        assert_eq!(back.kind.as_str(), "role.offer");
        assert_eq!(back.body, json!({"role_id": "api"}));
    }

    #[tokio::test]
    async fn an_exotic_kind_still_produces_one_usable_filename() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        let env = Envelope::new(
            "run-8a41",
            urn::coordinator("hive"),
            urn::agent("api", "8a41"),
            "../../escape attempt",
            json!({}),
        );
        f.store.write_inbox("api", 1, &env).await.unwrap();

        let paths = f.store.agent_paths("api").await.unwrap();
        let names: Vec<String> = std::fs::read_dir(&paths.inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["000001-.._.._escape_attempt.json"]);
    }

    #[tokio::test]
    async fn drain_is_exactly_once() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        write_outbox(
            &f.store,
            "api",
            "01-work.json",
            r#"{"kind":"work.started","body":{}}"#,
        )
        .await;
        write_outbox(
            &f.store,
            "api",
            "02-peer.json",
            r#"{"kind":"peer.question","body":{"about":"api","text":"which encoding?"}}"#,
        )
        .await;

        let first = f.store.drain_outbox("api").await.unwrap();
        assert_eq!(
            first.iter().map(|o| o.kind.as_str()).collect::<Vec<_>>(),
            ["work.started", "peer.question"]
        );
        assert_eq!(first[1].body["text"], "which encoding?");
        assert!(first[1].source.ends_with("02-peer.json"));

        // Emitted once: a second drain finds nothing, because the files are gone.
        assert!(f.store.drain_outbox("api").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unregistered_kind_drains_like_any_other() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        write_outbox(
            &f.store,
            "api",
            "01.json",
            r#"{"kind":"contract.telepathy.requested","body":{"whatever":[1,2]}}"#,
        )
        .await;

        let drained = f.store.drain_outbox("api").await.unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, "contract.telepathy.requested");
        assert_eq!(drained[0].body, json!({"whatever": [1, 2]}));
    }

    #[tokio::test]
    async fn a_malformed_file_is_reported_by_name_and_nothing_is_lost() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        write_outbox(
            &f.store,
            "api",
            "01-good.json",
            r#"{"kind":"work.started","body":{}}"#,
        )
        .await;
        write_outbox(&f.store, "api", "02-truncated.json", r#"{"kind":"work.st"#).await;

        let err = f.store.drain_outbox("api").await.unwrap_err().to_string();
        assert!(err.contains("02-truncated.json"), "{err}");
        assert!(!err.contains("01-good.json"), "{err}");

        // Neither file was removed and neither was repaired: nothing may be dropped on
        // the error path, because the caller received no messages at all.
        let paths = f.store.agent_paths("api").await.unwrap();
        assert!(paths.outbox.join("01-good.json").is_file());
        assert!(paths.outbox.join("02-truncated.json").is_file());

        // Fixing the broken file unblocks the mailbox; the good message is still there.
        std::fs::remove_file(paths.outbox.join("02-truncated.json")).unwrap();
        assert_eq!(f.store.drain_outbox("api").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_body_is_never_invented_for_a_worker() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        write_outbox(&f.store, "api", "01.json", r#"{"kind":"work.started"}"#).await;
        let err = f.store.drain_outbox("api").await.unwrap_err().to_string();
        assert!(err.contains("missing \"body\""), "{err}");

        write_outbox(&f.store, "api", "01.json", r#"{"body":{}}"#).await;
        let err = f.store.drain_outbox("api").await.unwrap_err().to_string();
        assert!(err.contains("kind"), "{err}");

        write_outbox(&f.store, "api", "01.json", r#"["not","an","object"]"#).await;
        let err = f.store.drain_outbox("api").await.unwrap_err().to_string();
        assert!(err.contains("expected a JSON object"), "{err}");
    }

    #[tokio::test]
    async fn worker_scratch_in_the_outbox_is_left_alone() {
        let f = fixture().await;
        f.store.init_agent("api", "brief").await.unwrap();
        write_outbox(&f.store, "api", "notes.txt", "thinking out loud").await;
        write_outbox(&f.store, "api", ".partial.json", "{").await;
        write_outbox(
            &f.store,
            "api",
            "01.json",
            r#"{"kind":"work.started","body":{}}"#,
        )
        .await;

        assert_eq!(f.store.drain_outbox("api").await.unwrap().len(), 1);
        let paths = f.store.agent_paths("api").await.unwrap();
        assert!(paths.outbox.join("notes.txt").is_file());
        assert!(paths.outbox.join(".partial.json").is_file());
        assert!(!paths.outbox.join("01.json").exists());
    }

    #[tokio::test]
    async fn a_missing_report_is_none_and_a_broken_one_is_an_error() {
        let f = fixture().await;
        let paths = f.store.init_agent("api", "brief").await.unwrap();
        assert!(f.store.read_report("api").await.unwrap().is_none());

        let report = CompletionReport::fallback(&urn::agent("api", "8a41"));
        fs::write(
            paths.root.join("REPORT.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .await
        .unwrap();
        let back = f.store.read_report("api").await.unwrap().expect("a report");
        assert_eq!(back.report_id, report.report_id);

        fs::write(paths.root.join("REPORT.json"), b"{ nope")
            .await
            .unwrap();
        let err = f.store.read_report("api").await.unwrap_err().to_string();
        assert!(err.contains("REPORT.json"), "{err}");
    }

    #[tokio::test]
    async fn the_audit_trail_is_written_where_section_14_says() {
        let f = fixture().await;
        f.store
            .record("DECOMPOSITION.json", &json!({"agent_count": 2}))
            .await
            .unwrap();
        assert!(f.store.root().join("DECOMPOSITION.json").is_file());
        assert!(f.store.record("../escape.json", &json!({})).await.is_err());

        for i in 0..3 {
            f.store
                .append_amendment(&json!({"n": i, "decision": "accepted", "note": "a\nb"}))
                .await
                .unwrap();
        }
        let jsonl =
            std::fs::read_to_string(f.store.root().join("AMENDMENTS.jsonl")).unwrap();
        // One amendment, one line — an embedded newline is escaped, not emitted.
        assert_eq!(jsonl.lines().count(), 3);
        let last: serde_json::Value = serde_json::from_str(jsonl.lines().last().unwrap()).unwrap();
        assert_eq!(last["n"], 2);
        assert_eq!(last["note"], "a\nb");
    }

    #[tokio::test]
    async fn an_unknown_role_is_an_error_not_a_freshly_invented_directory() {
        let f = fixture().await;
        assert!(f.store.agent_paths("ghost").await.is_err());
        assert!(f.store.drain_outbox("ghost").await.is_err());
        assert!(!f.store.root().join("agents/ghost").exists());
    }
}
