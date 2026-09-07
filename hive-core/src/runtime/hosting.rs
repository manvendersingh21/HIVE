//! Per-role stock-CLI hosting over authenticated SSH. HACP stays in the runtime;
//! the remote helper only manages processes and checks file transfers.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::collab::{SessionHandle, SessionHost, SessionOutcome, SessionSpec};
use crate::watchdog::Watchdog;

const HELPER: &str = include_str!("remote_io.py");
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 2048;

/// Placement is private run configuration, never a field on the HACP wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// An explicitly trusted SSH config alias. None means this device.
    pub host: Option<String>,
    /// Exact CLI model identifier; no fallback or substitution.
    pub model: Option<String>,
}

pub async fn make_host(placement: &Placement, root: &Path) -> anyhow::Result<Box<dyn SessionHost>> {
    match placement.host.as_deref() {
        None => Ok(Box::new(crate::collab::session::LocalSessionHost::new())),
        Some(target) => Ok(Box::new(SshSessionHost::connect(target, root).await?)),
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn relative(root: &Path, path: &Path) -> anyhow::Result<String> {
    let rel = path.strip_prefix(root).map_err(|_| anyhow::anyhow!("path is outside this run"))?;
    anyhow::ensure!(!rel.as_os_str().is_empty() && rel.components().all(|p| matches!(p, Component::Normal(_))),
        "invalid relative workspace path");
    let value = rel.to_str().ok_or_else(|| anyhow::anyhow!("workspace path is not UTF-8"))?;
    anyhow::ensure!(!value.contains('\\'), "backslash in workspace path");
    Ok(value.into())
}

pub(crate) fn workspace_output(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let path = root.join(name);
    let rel = relative(root, &path)?;
    let mut current = root.to_path_buf();
    for component in Path::new(&rel).components() {
        current.push(component);
        anyhow::ensure!(!current.is_symlink(), "output follows a symlink outside the declared file interface");
    }
    Ok(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransferFile {
    path: String,
    bytes: Vec<u8>,
    digest: String,
    executable: bool,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn snapshot(root: &Path) -> anyhow::Result<Vec<TransferFile>> {
    fn visit(root: &Path, dir: &Path, files: &mut Vec<TransferFile>, size: &mut usize) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            anyhow::ensure!(!kind.is_symlink(), "symlink in workspace transfer");
            if kind.is_dir() {
                if entry.file_name() != ".git" && entry.file_name() != "__pycache__" {
                    visit(root, &path, files, size)?;
                }
            } else {
                anyhow::ensure!(kind.is_file(), "only regular workspace files can be transferred");
                anyhow::ensure!(entry.metadata()?.len() <= MAX_BYTES as u64, "transfer file too large");
                let bytes = std::fs::read(&path)?;
                *size += bytes.len();
                anyhow::ensure!(*size <= MAX_BYTES && files.len() < MAX_FILES, "workspace exceeds transfer limits");
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    entry.metadata()?.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                files.push(TransferFile { path: relative(root, &path)?, digest: digest(&bytes), bytes, executable });
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, root, &mut files, &mut 0)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn install(root: &Path, files: &[TransferFile]) -> anyhow::Result<()> {
    anyhow::ensure!(files.len() <= MAX_FILES, "too many transferred files");
    let mut total = 0;
    let mut seen = BTreeSet::new();
    for file in files {
        let path = root.join(&file.path);
        relative(root, &path)?;
        anyhow::ensure!(seen.insert(&file.path), "duplicate transfer path");
        total += file.bytes.len();
        anyhow::ensure!(total <= MAX_BYTES && digest(&file.bytes) == file.digest, "transfer digest/size mismatch");
        let mut ancestor = Some(path.as_path());
        while let Some(p) = ancestor {
            anyhow::ensure!(!p.is_symlink(), "symlink in destination path");
            if p == root { break; }
            ancestor = p.parent();
        }
    }
    for file in files {
        let path = root.join(&file.path);
        std::fs::create_dir_all(path.parent().expect("workspace parent"))?;
        let tmp = path.with_file_name(format!(".hive-transfer-{}", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if file.executable { 0o700 } else { 0o600 });
        }
        let mut stream = options.open(&tmp)?;
        std::io::Write::write_all(&mut stream, &file.bytes)?;
        stream.sync_all()?;
        std::fs::rename(tmp, path)?;
    }
    Ok(())
}

#[derive(Clone)]
struct Hosted {
    spec: SessionSpec,
    started: i64,
    approved_bytes: u64,
}

/// The detached tmux process survives a lost connection/coordinator. All recovery
/// operations observe its original name and log; none can launch a replacement.
pub struct SshSessionHost {
    ssh_program: PathBuf,
    target: String,
    local_root: PathBuf,
    remote_root: PathBuf,
    sessions: Mutex<HashMap<String, Hosted>>,
    watchdog: Watchdog,
}

impl SshSessionHost {
    pub async fn connect(target: &str, local_root: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(!target.is_empty() && !target.starts_with('-') &&
            target.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c)), "invalid SSH alias");
        tokio::fs::create_dir_all(local_root).await?;
        let local_root = std::fs::canonicalize(local_root)?;
        let tag = digest(local_root.to_string_lossy().as_bytes());
        let remote_root = PathBuf::from(format!("/tmp/hive-collab-{}", &tag[..24]));
        let host = Self { ssh_program:"ssh".into(), target: target.into(), local_root, remote_root,
            sessions: Mutex::new(HashMap::new()), watchdog: Watchdog::new() };
        host.rpc(json!({"mode":"preflight"})).await?;
        Ok(host)
    }

    async fn rpc(&self, mut request: Value) -> anyhow::Result<Value> {
        request["root"] = json!(self.remote_root);
        let payload = serde_json::to_vec(&request)?;
        // Only the helper code is argv. Workspace contents travel on this explicitly
        // owned stdin pipe; SSH never inherits the user's piped chat/task input.
        let script = format!("exec python3 -c {}", quote(HELPER));
        let remote = format!("exec \"${{SHELL:-/bin/sh}}\" -lc {}", quote(&script));
        let mut child = Command::new(&self.ssh_program)
            .args(["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
                "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2", "--", &self.target, &remote])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("SSH input pipe missing"))?;
        let send = async move { stdin.write_all(&payload).await?; stdin.shutdown().await };
        let operation = async {
            let (sent, result) = tokio::join!(send, child.wait_with_output());
            sent?;
            let output = result?;
            anyhow::ensure!(output.status.success(), "SSH host operation failed (not a task outcome): {}",
                String::from_utf8_lossy(&output.stderr).trim());
            Ok::<Value, anyhow::Error>(serde_json::from_slice(&output.stdout)?)
        };
        tokio::time::timeout(Duration::from_secs(30), operation).await
            .map_err(|_| anyhow::anyhow!("SSH observation timed out; invocation state is unknown, not finished"))?
    }

    fn mapped(&self, path: &Path) -> anyhow::Result<PathBuf> {
        Ok(self.remote_root.join(relative(&self.local_root, path)?))
    }

    fn request(&self, mode: &str, spec: &SessionSpec) -> anyhow::Result<Value> {
        Ok(json!({"mode":mode,"name":spec.name,"log":relative(&self.local_root,&spec.log)?,
            "workspace":relative(&self.local_root,&spec.cwd)?}))
    }

    async fn push(&self, spec: &SessionSpec) -> anyhow::Result<()> {
        let files = snapshot(&spec.cwd)?;
        let mut request = self.request("put", spec)?;
        request["files"] = serde_json::to_value(&files)?;
        let ack = self.rpc(request).await?;
        let expected: Vec<Value> = files.iter().map(|f| json!({"path":f.path,"digest":f.digest})).collect();
        anyhow::ensure!(ack["received"] == json!(expected), "remote file receipt differs from transfer manifest");
        Ok(())
    }

    async fn pull(&self, spec: &SessionSpec) -> anyhow::Result<()> {
        let response = self.rpc(self.request("get", spec)?).await?;
        let files: Vec<TransferFile> = serde_json::from_value(response["files"].clone())?;
        install(&spec.cwd, &files)
    }

    async fn pause_spec(&self, spec: &SessionSpec) -> anyhow::Result<bool> {
        let result = self.rpc(self.request("pause", spec)?).await?;
        anyhow::ensure!(result["stopped"] == true || result["ended"] == true, "remote pause not confirmed");
        Ok(result["stopped"] == true)
    }

    async fn record(&self, handle: &SessionHandle) -> anyhow::Result<Hosted> {
        self.sessions.lock().await.get(&handle.name).cloned().ok_or_else(|| anyhow::anyhow!("recover the remote invocation before observing it"))
    }
}

#[async_trait]
impl SessionHost for SshSessionHost {
    async fn launch(&self, spec: &SessionSpec) -> anyhow::Result<SessionHandle> {
        let state = self.rpc(self.request("poll", spec)?).await?;
        anyhow::ensure!(state["live"] == false && state["log_exists"] == false,
            "remote invocation already has host evidence; recover without overwriting its workspace");
        self.push(spec).await?;
        let mut mapped = spec.clone();
        mapped.cwd = self.mapped(&spec.cwd)?;
        mapped.log = self.mapped(&spec.log)?;
        let from = self.local_root.to_string_lossy();
        let to = self.remote_root.to_string_lossy();
        mapped.prompt = mapped.prompt.replace(from.as_ref(), to.as_ref());
        mapped.args = mapped.args.iter().map(|a| a.replace(from.as_ref(), to.as_ref())).collect();
        let mut request = self.request("launch", spec)?;
        request["command"] = json!(crate::collab::session::build_command(&mapped)?);
        self.sessions.lock().await.insert(spec.name.clone(), Hosted { spec:spec.clone(), started:chrono::Utc::now().timestamp(), approved_bytes:0 });
        self.rpc(request).await?;
        Ok(SessionHandle { name:spec.name.clone(), log:spec.log.clone() })
    }

    async fn recover(&self, spec: &SessionSpec, started: i64) -> anyhow::Result<SessionHandle> {
        let state = self.rpc(self.request("poll", spec)?).await?;
        anyhow::ensure!(state["live"] == true || state["log_exists"] == true,
            "uncertain remote launch has neither process nor log; refusing to relaunch");
        // Never push the stale local workspace over a still-running remote agent.
        self.sessions.lock().await.insert(spec.name.clone(), Hosted { spec:spec.clone(), started, approved_bytes:0 });
        Ok(SessionHandle { name:spec.name.clone(), log:spec.log.clone() })
    }

    async fn wait(&self, handle: &SessionHandle) -> anyhow::Result<SessionOutcome> {
        let record = self.record(handle).await?;
        if let Some(parent) = handle.log.parent() { tokio::fs::create_dir_all(parent).await?; }
        let mut log = tokio::fs::File::create(&handle.log).await?;
        let mut offset = 0;
        let mut pending = Vec::new();
        let mut code = None;
        let mut scanned = 0;
        loop {
            let mut request = self.request("poll", &record.spec)?;
            request["offset"] = json!(offset);
            let reply = self.rpc(request).await?;
            let bytes: Vec<u8> = serde_json::from_value(reply["bytes"].clone())?;
            offset += bytes.len();
            log.write_all(&bytes).await?;
            log.flush().await?;
            pending.extend(bytes);
            let ended = reply["live"] == false && reply["drained"] == true;
            while let Some(end) = pending.iter().position(|b| *b == b'\n').or_else(|| {
                if ended && !pending.is_empty() { Some(pending.len()-1) } else { None }
            }) {
                let line: Vec<u8> = pending.drain(..=end).collect();
                scanned += line.len() as u64;
                let text = String::from_utf8_lossy(&line);
                if let Some(number) = text.trim().strip_prefix("__HIVE_DONE__") {
                    if let Ok(number) = number.parse::<i32>() { code = Some(number); }
                }
                if let Some(analysis) = (scanned > record.approved_bytes).then(|| self.watchdog.scan_line(&text)).flatten() {
                    if !ended {
                        if self.pause_spec(&record.spec).await? {
                            return Ok(SessionOutcome::Paused { reason:analysis.reason });
                        }
                    }
                }
            }
            if ended {
                let code = code.ok_or_else(|| anyhow::anyhow!("remote invocation ended without completion evidence; do not relaunch"))?;
                self.pull(&record.spec).await?;
                return Ok(SessionOutcome::Exited { code });
            }
            if record.spec.timeout_secs > 0 && chrono::Utc::now().timestamp().saturating_sub(record.started) >= record.spec.timeout_secs as i64 {
                if self.pause_spec(&record.spec).await? {
                    return Ok(SessionOutcome::TimedOut);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn recover_at(&self, spec: &SessionSpec, started: i64, approved_bytes: u64) -> anyhow::Result<SessionHandle> {
        let handle = self.recover(spec, started).await?;
        anyhow::ensure!(self.log_size(&handle).await? >= approved_bytes, "approved remote log was truncated");
        self.sessions.lock().await.get_mut(&spec.name).expect("recovered handle").approved_bytes = approved_bytes;
        Ok(handle)
    }

    async fn log_size(&self, handle: &SessionHandle) -> anyhow::Result<u64> {
        let record = self.record(handle).await?;
        self.rpc(self.request("poll", &record.spec)?).await?["size"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("remote log size missing"))
    }

    async fn pause(&self, handle: &SessionHandle, _reason: &str) -> anyhow::Result<()> {
        self.pause_spec(&self.record(handle).await?.spec).await.map(|_| ())
    }

    async fn resume(&self, handle: &SessionHandle) -> anyhow::Result<()> {
        self.rpc(self.request("resume", &self.record(handle).await?.spec)?).await?;
        Ok(())
    }
}

pub(crate) async fn recover_invocation(host: &dyn SessionHost, journal: Option<&super::journal::Journal>,
    record: &super::journal::InvocationRecord) -> anyhow::Result<SessionHandle> {
    let name = &record.spec.name;
    let offset = journal.map(|j| j.get(&format!("approved-log:{name}"))).transpose()?.flatten()
        .and_then(|v| v.as_u64()).unwrap_or(0);
    let handle = host.recover_at(&record.spec, record.started_unix, offset).await?;
    if let Some(journal) = journal {
        if journal.get(&format!("continue-pending:{name}"))? == Some(json!(true)) {
            host.continue_approved(&handle).await?;
            journal.remember(&format!("continue-pending:{name}"), &json!(false))?;
        }
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_host(scratch: &super::super::Scratch) -> SshSessionHost {
        use std::os::unix::fs::PermissionsExt;
        let ssh = scratch.join("ssh-stub");
        std::fs::write(&ssh, format!("#!/bin/sh\nif test -f {}; then echo simulated-disconnect >&2; exit 255; fi\nfor hive_arg do hive_last=$hive_arg; done\nexec /bin/sh -c \"$hive_last\"\n", quote(&scratch.join("disconnected").to_string_lossy()))).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let local_root = scratch.join("local");
        std::fs::create_dir_all(local_root.join("wrk")).unwrap();
        SshSessionHost { ssh_program:ssh, target:"test-peer".into(),
            local_root:std::fs::canonicalize(local_root).unwrap(),
            remote_root:scratch.join("hive-collab-peer"), sessions:Mutex::new(HashMap::new()), watchdog:Watchdog::new() }
    }

    #[tokio::test]
    #[ignore = "uses real local tmux with a loopback SSH shim and injected disconnect"]
    async fn local_remote_host_disconnect_does_not_repeat_or_overwrite_live_work() {
        let scratch = super::super::Scratch::new("host-disconnect");
        let host = loopback_host(&scratch);
        let spec = SessionSpec { name:format!("hive-{}-disconnect", uuid::Uuid::new_v4().simple()),
            program:"sh".into(), args:vec!["-c".into()], prompt:"echo started-once; echo remote > result.txt; sleep 3; exit 3".into(),
            cwd:host.local_root.join("wrk"), log:host.local_root.join("logs/work.log"), timeout_secs:30 };
        let started = chrono::Utc::now().timestamp();
        let handle = host.launch(&spec).await.unwrap();
        std::fs::write(scratch.join("disconnected"), "yes").unwrap();
        assert!(host.wait(&handle).await.unwrap_err().to_string().contains("simulated-disconnect"));
        drop(host);
        std::fs::remove_file(scratch.join("disconnected")).unwrap();
        let host = loopback_host(&scratch);
        std::fs::write(spec.cwd.join("result.txt"), "stale local mirror").unwrap();
        assert!(host.launch(&spec).await.is_err(), "an existing invocation cannot be launched again");
        let handle = host.recover(&spec, started).await.unwrap();
        assert_eq!(host.wait(&handle).await.unwrap(), SessionOutcome::Exited { code:3 });
        assert_eq!(std::fs::read_to_string(spec.cwd.join("result.txt")).unwrap(), "remote\n");
        assert_eq!(std::fs::read_to_string(&spec.log).unwrap().matches("started-once").count(), 1);
    }

    #[tokio::test]
    #[ignore = "uses real local tmux to verify remote-host SIGSTOP and SIGCONT"]
    async fn local_remote_host_pauses_on_timeout_and_watchdog_without_killing() {
        for watchdog in [false, true] {
            let scratch = super::super::Scratch::new("host-pause");
            let host = loopback_host(&scratch);
            let text = if watchdog { "echo 'rm -rf /'; sleep 3; echo survived" } else { "echo waiting; sleep 3; echo survived" };
            let spec = SessionSpec { name:format!("hive-{}-pause", uuid::Uuid::new_v4().simple()),
                program:"sh".into(), args:vec!["-c".into()], prompt:text.into(),
                cwd:host.local_root.join("wrk"), log:host.local_root.join("logs/work.log"), timeout_secs:if watchdog { 30 } else { 1 } };
            let handle = host.launch(&spec).await.unwrap();
            let result = host.wait(&handle).await.unwrap();
            if watchdog { assert!(matches!(result, SessionOutcome::Paused { .. })); }
            else { assert_eq!(result, SessionOutcome::TimedOut); }
            let continued = host.rpc(host.request("resume", &spec).unwrap()).await.unwrap();
            assert!(continued["resumed"].as_u64().unwrap() > 0, "no stopped process survived to resume");
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let state = host.rpc(host.request("poll", &spec).unwrap()).await.unwrap();
                    if state["live"] == false {
                        let bytes:Vec<u8> = serde_json::from_value(state["bytes"].clone()).unwrap();
                        assert!(String::from_utf8_lossy(&bytes).contains("survived"));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }).await.unwrap();
        }
    }

    #[test]
    #[ignore = "uses real local tmux and the remote helper through a loopback SSH shim"]
    fn local_remote_host_approved_cursor_recovers_without_waiving_new_output() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let scratch = super::super::Scratch::new("remote-cursor");
            let host = loopback_host(&scratch);
            let spec = SessionSpec { name:format!("hive-{}-cursor", uuid::Uuid::new_v4().simple()),
                program:"sh".into(), args:vec!["-c".into()],
                prompt:"echo started-once; echo 'rm -rf /'; sleep 2; echo 'rm -rf /'; sleep 2; echo survived".into(),
                cwd:host.local_root.join("wrk"), log:host.local_root.join("logs/work.log"), timeout_secs:30 };
            let handle = host.launch(&spec).await.unwrap();
            assert!(matches!(host.wait(&handle).await.unwrap(), SessionOutcome::Paused { .. }));
            let first_cursor = host.log_size(&handle).await.unwrap();
            drop(host);
            let host = loopback_host(&scratch);
            let handle = host.recover_at(&spec, chrono::Utc::now().timestamp(), first_cursor).await.unwrap();
            host.continue_approved(&handle).await.unwrap();
            assert!(matches!(host.wait(&handle).await.unwrap(), SessionOutcome::Paused { .. }),
                "only already reviewed bytes may be exempted");
            let second_cursor = host.log_size(&handle).await.unwrap();
            assert!(second_cursor > first_cursor);
            drop(host);
            let host = loopback_host(&scratch);
            let handle = host.recover_at(&spec, chrono::Utc::now().timestamp(), second_cursor).await.unwrap();
            host.continue_approved(&handle).await.unwrap();
            assert_eq!(host.wait(&handle).await.unwrap(), SessionOutcome::Exited { code:0 });
            host.continue_approved(&handle).await.unwrap(); // idempotent after completion
            let log = std::fs::read_to_string(&spec.log).unwrap();
            assert_eq!(log.matches("started-once").count(), 1);
            assert_eq!(log.matches("rm -rf /").count(), 2, "full log is retained across cursor recovery");
            assert!(log.contains("survived"));
        });
    }

    #[test]
    fn transfer_checks_all_digests_and_paths_before_writing_any_file() {
        let scratch = super::super::Scratch::new("transfer");
        let file = TransferFile { path:"src/a.py".into(), bytes:b"print(1)\n".to_vec(),
            digest:digest(b"print(1)\n"), executable:true };
        for path in ["../escape", "/absolute", "a/../../escape", "a\\b"] {
            let mut bad = file.clone(); bad.path = path.into();
            assert!(install(&scratch.dir, &[file.clone(), bad]).is_err());
            assert!(!scratch.join("src/a.py").exists());
        }
        let mut bad = file.clone(); bad.digest = "f".repeat(64); bad.path = "second".into();
        assert!(install(&scratch.dir, &[file.clone(), bad]).is_err());
        assert!(!scratch.join("src/a.py").exists());
        install(&scratch.dir, &[file.clone()]).unwrap();
        let read = snapshot(&scratch.dir).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].digest, file.digest);
        assert!(read[0].executable);
    }

    #[tokio::test]
    async fn independent_remote_transfer_helper_agrees_with_rust_digests() {
        let scratch = super::super::Scratch::new("remote-helper");
        let root = scratch.join("hive-collab-transfer");
        let mut child = Command::new("python3").args(["-c", HELPER]).stdin(Stdio::piped())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let request = json!({"root":root,"mode":"put","workspace":"wrk","files":[{
            "path":"nested/binary","bytes":[0,255,65],"digest":digest(&[0,255,65]),"executable":false}]});
        child.stdin.take().unwrap().write_all(&serde_json::to_vec(&request).unwrap()).await.unwrap();
        let out = child.wait_with_output().await.unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read(root.join("wrk/nested/binary")).unwrap(), vec![0,255,65]);
        let reply:Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(reply["received"][0]["digest"], digest(&[0,255,65]));
    }

    #[tokio::test]
    #[ignore = "needs HIVE_TEST_SSH_HOST; terminates only an owned SSH observation channel while a remote task runs"]
    async fn live_ssh_channel_disconnect_preserves_running_task() {
        use std::os::unix::fs::PermissionsExt;
        let target = std::env::var("HIVE_TEST_SSH_HOST").expect("set HIVE_TEST_SSH_HOST");
        let scratch = super::super::Scratch::new("ssh-channel-disconnect");
        let root = std::fs::canonicalize(&scratch.dir).unwrap();
        std::fs::create_dir_all(root.join("wrk")).unwrap();
        let spec = SessionSpec { name:format!("hive-{}-channel-test", uuid::Uuid::new_v4().simple()),
            program:"sh".into(), args:vec!["-c".into()],
            prompt:"echo started-once; sleep 5; echo recovered > result.txt; exit 7".into(),
            cwd:root.join("wrk"), log:root.join("logs/task.log"), timeout_secs:30 };
        let mut first = SshSessionHost::connect(&target, &root).await.unwrap();
        let started = chrono::Utc::now().timestamp();
        let handle = first.launch(&spec).await.unwrap();
        let wrapper = root.join("observation-ssh");
        let pidfile = root.join("observation.pid");
        let stderr = root.join("observation.stderr");
        // A real SSH channel reports establishment before delaying its observation.
        // The task remains detached in a different remote tmux process group.
        let source = format!(r#"#!/bin/bash
args=("$@")
last=$((${{#args[@]}}-1))
args[$last]="printf 'HIVE_TEST_CHANNEL_CONNECTED\\n' >&2; sleep 10; ${{args[$last]}}"
printf '%s\n' "$$" > {}
exec /usr/bin/ssh "${{args[@]}}" 2> {}
"#, quote(&pidfile.to_string_lossy()), quote(&stderr.to_string_lossy()));
        std::fs::write(&wrapper, source).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        first.ssh_program = wrapper;
        let observation = tokio::spawn(async move { first.wait(&handle).await });
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if std::fs::read_to_string(&stderr).unwrap_or_default().contains("HIVE_TEST_CHANNEL_CONNECTED") { break; }
                assert!(!observation.is_finished(), "observation ended before channel establishment");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        let pid:libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(pid > 1);
        // Exact PID of our wrapper's exec'd SSH client. No task or process group
        // is signalled; network settings and unrelated connections are untouched.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        assert!(observation.await.unwrap().unwrap_err().to_string().contains("SSH host operation failed"));
        let second = SshSessionHost::connect(&target, &root).await.unwrap();
        let handle = second.recover(&spec, started).await.unwrap();
        assert_eq!(second.wait(&handle).await.unwrap(), SessionOutcome::Exited { code:7 });
        assert_eq!(std::fs::read_to_string(spec.cwd.join("result.txt")).unwrap(), "recovered\n");
        assert_eq!(std::fs::read_to_string(&spec.log).unwrap().matches("started-once").count(), 1);
    }

    #[tokio::test]
    #[ignore = "needs HIVE_TEST_SSH_HOST; sends a realistic large contract-sized prompt on both hosts"]
    async fn live_ssh_and_local_accept_large_contract_prompts() {
        let target = std::env::var("HIVE_TEST_SSH_HOST").expect("set HIVE_TEST_SSH_HOST");
        let scratch = super::super::Scratch::new("large-contract-live");
        let root = std::fs::canonicalize(&scratch.dir).unwrap();
        std::fs::create_dir_all(root.join("wrk")).unwrap();
        let prompt = "contract-text-".repeat(4500);
        for (role, host) in [("local", Box::new(crate::collab::session::LocalSessionHost::new()) as Box<dyn SessionHost>),
            ("remote", Box::new(SshSessionHost::connect(&target, &root).await.unwrap()) as Box<dyn SessionHost>)] {
            let spec = SessionSpec { name:format!("hive-{}-large-{role}", uuid::Uuid::new_v4().simple()),
                program:"python3".into(), args:vec!["-c".into(), format!("import sys; print(len(sys.argv[1])); assert len(sys.argv[1]) == {}", prompt.len())],
                prompt:prompt.clone(), cwd:root.join("wrk"), log:root.join(format!("logs/{role}.log")), timeout_secs:30 };
            let handle = host.launch(&spec).await.unwrap();
            assert_eq!(host.wait(&handle).await.unwrap(), SessionOutcome::Exited { code:0 });
            assert!(std::fs::read_to_string(&spec.log).unwrap().contains(&prompt.len().to_string()));
        }
    }

    #[tokio::test]
    #[ignore = "needs HIVE_TEST_SSH_HOST; real remote pause/continuation with one injected observation failure"]
    async fn live_ssh_approved_continuation_survives_lost_observation() {
        let target = std::env::var("HIVE_TEST_SSH_HOST").expect("set HIVE_TEST_SSH_HOST");
        let scratch = super::super::Scratch::new("ssh-continuation-live");
        let root = std::fs::canonicalize(&scratch.dir).unwrap();
        std::fs::create_dir_all(root.join("wrk")).unwrap();
        let spec = SessionSpec { name:format!("hive-{}-continuation-test", uuid::Uuid::new_v4().simple()),
            program:"sh".into(), args:vec!["-c".into()],
            // These are printed test strings, never commands to execute.
            prompt:"echo started-once; echo 'rm -rf /'; sleep 3; echo 'rm -rf /'; sleep 3; echo survived".into(),
            cwd:root.join("wrk"), log:root.join("logs/test.log"), timeout_secs:30 };
        let journal = super::super::journal::Journal::open(&root.join("runtime.db")).unwrap();
        journal.begin_invocation(&spec).unwrap();
        let mut first = SshSessionHost::connect(&target, &root).await.unwrap();
        let handle = first.launch(&spec).await.unwrap();
        let paused = first.wait(&handle).await.unwrap();
        assert!(matches!(paused, SessionOutcome::Paused { .. }));
        journal.finish_invocation(&spec.name, &json!({"role":"worker","outcome":paused})).unwrap();
        let first_cursor = first.log_size(&handle).await.unwrap();
        journal.authorize_continue(&spec.name, "reviewed echo-only fixture output", first_cursor).unwrap();
        first.continue_approved(&handle).await.unwrap();
        // Model loss of observation after SIGCONT but before its durable receipt.
        // Only this host's next transport operation fails; no network settings change.
        first.ssh_program = "/usr/bin/false".into();
        assert!(first.wait(&handle).await.unwrap_err().to_string().contains("SSH host operation failed"));
        drop(first);
        assert_eq!(journal.get(&format!("continue-pending:{}", spec.name)).unwrap(), Some(json!(true)));
        let second = SshSessionHost::connect(&target, &root).await.unwrap();
        let handle = recover_invocation(&second, Some(&journal), &journal.invocation(&spec.name).unwrap()).await.unwrap();
        assert_eq!(journal.get(&format!("continue-pending:{}", spec.name)).unwrap(), Some(json!(false)));
        let paused = second.wait(&handle).await.unwrap();
        assert!(matches!(paused, SessionOutcome::Paused { .. }), "new violation must remain supervised");
        let second_cursor = second.log_size(&handle).await.unwrap();
        assert!(second_cursor > first_cursor);
        journal.finish_invocation(&spec.name, &json!({"role":"worker","outcome":paused})).unwrap();
        journal.authorize_continue(&spec.name, "reviewed second echo-only fixture event", second_cursor).unwrap();
        drop(second);
        let third = SshSessionHost::connect(&target, &root).await.unwrap();
        let handle = recover_invocation(&third, Some(&journal), &journal.invocation(&spec.name).unwrap()).await.unwrap();
        assert_eq!(third.wait(&handle).await.unwrap(), SessionOutcome::Exited { code:0 });
        third.continue_approved(&handle).await.unwrap();
        let log = std::fs::read_to_string(&spec.log).unwrap();
        assert_eq!(log.matches("started-once").count(), 1);
        assert_eq!(log.matches("rm -rf /").count(), 2);
        assert!(log.contains("survived"));
        assert!(third.launch(&spec).await.is_err(), "completion cannot authorize relaunch");
    }

    #[tokio::test]
    #[ignore = "needs HIVE_TEST_SSH_HOST pointing to a trusted SSH peer with Python and tmux"]
    async fn live_ssh_invocation_recovers_and_transfers_without_relaunch() {
        let target = std::env::var("HIVE_TEST_SSH_HOST").expect("set HIVE_TEST_SSH_HOST");
        let scratch = super::super::Scratch::new("ssh-host-live");
        let root = std::fs::canonicalize(&scratch.dir).unwrap();
        std::fs::create_dir_all(root.join("wrk")).unwrap();
        std::fs::write(root.join("wrk/input.txt"), "input\n").unwrap();
        let spec = SessionSpec {
            name:format!("hive-{}-host-test", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            program:"sh".into(), args:vec!["-c".into()],
            prompt:"test -f input.txt || exit 9; printf 'started-once\\n'; printf 'ready\\n' > status.txt; sleep 2; exit 7".into(),
            cwd:root.join("wrk"), log:root.join("logs/test.log"), timeout_secs:30,
        };
        let first = SshSessionHost::connect(&target, &root).await.unwrap();
        let started = chrono::Utc::now().timestamp();
        first.launch(&spec).await.unwrap();
        drop(first);
        let second = SshSessionHost::connect(&target, &root).await.unwrap();
        let handle = second.recover(&spec, started).await.unwrap();
        assert_eq!(second.wait(&handle).await.unwrap(), SessionOutcome::Exited { code:7 });
        assert_eq!(std::fs::read_to_string(root.join("wrk/status.txt")).unwrap(), "ready\n");
        assert_eq!(std::fs::read_to_string(&spec.log).unwrap().matches("started-once").count(), 1);
        assert!(second.launch(&spec).await.is_err(), "completed remote log must prevent relaunch");
    }
}
