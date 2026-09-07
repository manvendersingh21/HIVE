//! Abrupt coordinator death, not just a returned error or a replaced host object.
//! Model outputs are deterministic, host evidence survives in a separate file,
//! and the acceptance commands execute real Python. No provider calls are made.

#![cfg(unix)]

use super::*;
use async_trait::async_trait;
use crate::collab::{SessionHandle, SessionOutcome, SessionSpec};
use crate::runtime::{journal::Journal, Scratch};

struct DurableTestHost;

fn evidence(spec: &SessionSpec) -> PathBuf { spec.log.with_extension("host-result.json") }

fn scripted_output(spec: &SessionSpec) -> anyhow::Result<(&'static str, String)> {
    Ok(if spec.name.ends_with("01-author") {
        ("delegation-terms.json", json!({"outputs":[{"name":"api.py","media_type":"text/x-python"}],
            "acceptance":["the file exists", "the frozen acceptance suite passes"],
            "verification":{"files":[{"path":"tests/test_api.py","content":
                "import unittest\nfrom api import value\nclass Acceptance(unittest.TestCase):\n    def test_value(self):\n        self.assertEqual(value(), 42)\n"}],
                "commands":[{"program":"python3","args":["-m","unittest","discover","-s","tests"],"timeout_secs":30}]}}).to_string())
    } else if spec.name.ends_with("02-review") {
        ("accept.json", "{\"accepted\":true}".into())
    } else if spec.name.ends_with("03-work") {
        ("api.py", "def value():\n    return 41\n".into())
    } else if spec.name.ends_with("03-work-a2") {
        ("api.py", "def value():\n    return 42\n".into())
    } else if spec.name.ends_with("04-verify") || spec.name.ends_with("04-verify-a2") {
        ("verdict.json", json!({"verdict":"accept", "checks":[{"name":"sha256 recomputed",
            "passed":true,"detail":"measured"}], "reasons":[]}).to_string())
    } else { anyhow::bail!("unexpected scripted invocation {}", spec.name); })
}

#[async_trait]
impl SessionHost for DurableTestHost {
    async fn launch(&self, spec: &SessionSpec) -> anyhow::Result<SessionHandle> {
        // An exclusive marker makes even a completed duplicate launch observable.
        std::fs::OpenOptions::new().write(true).create_new(true)
            .open(spec.log.with_extension("launched"))?;
        let outcome = if spec.program == "python3" {
            let output = tokio::process::Command::new(&spec.program).args(&spec.args)
                .current_dir(&spec.cwd).stdin(std::process::Stdio::null()).output().await?;
            let mut log = output.stdout; log.extend(output.stderr);
            std::fs::write(&spec.log, log)?;
            SessionOutcome::Exited { code:output.status.code().unwrap_or(-1) }
        } else {
            let (path, content) = scripted_output(spec)?;
            std::fs::write(spec.cwd.join(path), content)?;
            std::fs::write(&spec.log, "")?;
            SessionOutcome::Exited { code:0 }
        };
        std::fs::write(evidence(spec), serde_json::to_vec(&outcome)?)?;
        Ok(SessionHandle { name:spec.name.clone(), log:spec.log.clone() })
    }

    async fn recover(&self, spec: &SessionSpec, _: i64) -> anyhow::Result<SessionHandle> {
        anyhow::ensure!(evidence(spec).is_file(), "uncertain launch has no host evidence");
        Ok(SessionHandle { name:spec.name.clone(), log:spec.log.clone() })
    }

    async fn wait(&self, handle: &SessionHandle) -> anyhow::Result<SessionOutcome> {
        Ok(serde_json::from_slice(&std::fs::read(handle.log.with_extension("host-result.json"))?)?)
    }
    async fn pause(&self, _: &SessionHandle, _: &str) -> anyhow::Result<()> { anyhow::bail!("not used") }
    async fn resume(&self, _: &SessionHandle) -> anyhow::Result<()> { anyhow::bail!("not used") }
}

/// Invoked in a fresh test executable by the controller below. Fault configuration
/// is never exposed by a production binary or read by production runtime code.
#[tokio::test]
#[ignore = "helper process for the coordinator SIGKILL matrix"]
async fn coordinator_child() {
    let Ok(root) = std::env::var("HIVE_TEST_CRASH_ROOT") else { return; };
    let root = PathBuf::from(root);
    let boundary:usize = std::env::var("HIVE_TEST_CRASH_BOUNDARY").unwrap().parse().unwrap();
    let journal = Journal::open(&root.join("runtime.db")).unwrap();
    let resume = journal.get("run").unwrap().is_some();
    let cfg = RunConfig { supervisor:"opencode".into(), worker:"agy".into(),
        task:"implement value() returning 42, independently tested".into(), run_dir:root.clone(),
        timeout_secs:30, supervisor_placement:Default::default(), worker_placement:Default::default(), max_rework:1 };
    if boundary > 0 { journal.crash_after(boundary); }
    let result = if let Ok(stage) = std::env::var("HIVE_TEST_LIVE_CRASH_STAGE") {
        let host = LiveTestHost { inner:crate::collab::session::LocalSessionHost::new(), root:root.clone(), stage };
        run_persistent(&host, &host, &cfg, resume, Some(journal)).await
    } else {
        run_persistent(&DurableTestHost, &DurableTestHost, &cfg, resume, Some(journal)).await
    };
    let observation = match result {
        Ok(report) => json!({"report":report}),
        Err(error) => json!({"error":error.to_string()}),
    };
    std::fs::write(root.join("child-result.json"), serde_json::to_vec(&observation).unwrap()).unwrap();
}

/// Only the fixture adapter is scripted: the process, tmux session, completion
/// marker, log supervision and recovery are the production LocalSessionHost.
struct LiveTestHost {
    inner: crate::collab::session::LocalSessionHost,
    root: PathBuf,
    stage: String,
}

#[async_trait]
impl SessionHost for LiveTestHost {
    async fn launch(&self, spec: &SessionSpec) -> anyhow::Result<SessionHandle> {
        let gate = spec.name.ends_with(&self.stage);
        let output = if spec.program == "python3" { None } else { Some(scripted_output(spec)?) };
        let config = json!({"output":output,"command":std::iter::once(&spec.program).chain(&spec.args).collect::<Vec<_>>(),
            "marker":spec.log.with_extension("launched"), "gate":gate, "release":self.root.join("release-task"),
            "started":self.root.join("task-started.json"), "session":spec.name});
        let script = r#"import json, os, pathlib, subprocess, sys, time
c = json.loads(sys.argv[1])
with open(c['marker'], 'x') as f:
    f.write(str(os.getpid()))
print('started-once', flush=True)
if c['gate']:
    p = pathlib.Path(c['started'])
    tmp = p.with_suffix('.tmp')
    tmp.write_text(json.dumps({'pid':os.getpid(), 'session':c['session']}))
    tmp.replace(p)
    deadline = time.monotonic() + 25
    while not pathlib.Path(c['release']).exists():
        if time.monotonic() >= deadline:
            sys.exit(89)
        time.sleep(0.05)
if c['output'] is not None:
    pathlib.Path(c['output'][0]).write_text(c['output'][1])
else:
    sys.exit(subprocess.run(c['command'], stdin=subprocess.DEVNULL).returncode)
"#;
        let mapped = SessionSpec { program:"python3".into(), args:vec!["-c".into(), script.into(), config.to_string()],
            prompt:String::new(), ..spec.clone() };
        self.inner.launch(&mapped).await
    }
    async fn recover(&self, spec: &SessionSpec, started: i64) -> anyhow::Result<SessionHandle> {
        let handle = self.inner.recover(spec, started).await?;
        if spec.name.ends_with(&self.stage) {
            std::fs::write(self.root.join("recovered-task"), &spec.name)?;
        }
        Ok(handle)
    }
    async fn wait(&self, handle: &SessionHandle) -> anyhow::Result<SessionOutcome> { self.inner.wait(handle).await }
    async fn pause(&self, handle: &SessionHandle, reason: &str) -> anyhow::Result<()> { self.inner.pause(handle, reason).await }
    async fn resume(&self, handle: &SessionHandle) -> anyhow::Result<()> { self.inner.resume(handle).await }
}

fn live_child(root: &Path, stage: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::lifecycle::crash_tests::coordinator_child", "--ignored", "--nocapture"])
        .env("HIVE_TEST_CRASH_ROOT", root).env("HIVE_TEST_CRASH_BOUNDARY", "0")
        .env("HIVE_TEST_LIVE_CRASH_STAGE", stage)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .spawn().unwrap()
}

fn await_marker(child: &mut std::process::Child, marker: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !marker.exists() {
        assert!(child.try_wait().unwrap().is_none(), "coordinator exited before {}", marker.display());
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {}", marker.display());
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn assert_task_live(task: &Value) {
    let pid = task["pid"].as_i64().unwrap() as libc::pid_t;
    // An exact PID reported by our disposable fixture, never a process-group kill.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "original task process is no longer alive");
    let status = std::process::Command::new("tmux").args(["has-session", "-t",
        &format!("={}", task["session"].as_str().unwrap())]).status().unwrap();
    assert!(status.success(), "original tmux session no longer exists");
}

#[test]
#[ignore = "real detached tmux tasks survive coordinator SIGKILL at every invocation stage"]
fn live_tasks_survive_coordinator_sigkill_and_reattach_without_relaunch() {
    use std::os::unix::process::ExitStatusExt;
    let stages = ["01-author", "02-review", "03-work", "tests-a1-supervisor-0", "tests-a1-worker-0",
        "04-verify", "03-work-a2", "tests-a2-supervisor-0", "tests-a2-worker-0", "04-verify-a2"];
    for stage in stages {
        let scratch = Scratch::new("live-coordinator-crash");
        let root = scratch.join("run");
        std::fs::create_dir_all(&root).unwrap();
        let mut initial = live_child(&root, stage);
        await_marker(&mut initial, &root.join("task-started.json"));
        let task:Value = serde_json::from_slice(&std::fs::read(root.join("task-started.json")).unwrap()).unwrap();
        assert_task_live(&task);
        initial.kill().unwrap(); // only this test's coordinator; the task is not signalled
        assert_eq!(initial.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_task_live(&task);
        let before = receipts(&root);
        let mut resumed = live_child(&root, stage);
        await_marker(&mut resumed, &root.join("recovered-task"));
        assert_task_live(&task); // recovery happened while the original task remained running
        std::fs::write(root.join("release-task"), "continue fixture").unwrap();
        let status = resumed.wait().unwrap();
        assert!(status.success(), "recovery coordinator failed at {stage}");
        let result:Value = serde_json::from_slice(&std::fs::read(root.join("child-result.json")).unwrap()).unwrap();
        assert_eq!(result["report"]["outcome"], "settled", "{stage}: {result}");
        assert_eq!(&receipts(&root)[..before.len()], &before);
        assert_eq!(launches(&root), 10, "duplicate or missing invocation at {stage}");
        let log = std::fs::read_to_string(root.join("logs").join(format!("{stage}.log"))).unwrap();
        assert_eq!(log.matches("started-once").count(), 1);
        assert!(!std::process::Command::new("tmux").args(["has-session", "-t",
            &format!("={}", task["session"].as_str().unwrap())]).stderr(std::process::Stdio::null()).status().unwrap().success());
        println!("live coordinator SIGKILL recovery passed: {stage}");
    }
}

fn child(root: &Path, boundary: usize) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::lifecycle::crash_tests::coordinator_child", "--ignored", "--nocapture"])
        .env("HIVE_TEST_CRASH_ROOT", root).env("HIVE_TEST_CRASH_BOUNDARY", boundary.to_string())
        .stdin(std::process::Stdio::null()).output().unwrap()
}

fn receipts(root: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(root.join("runtime.db")).unwrap();
    let mut stmt = conn.prepare("SELECT envelope FROM runtime_inbox ORDER BY rowid").unwrap();
    stmt.query_map([], |r| r.get::<_,String>(0)).unwrap().map(Result::unwrap).collect()
}

fn launches(root: &Path) -> usize {
    std::fs::read_dir(root.join("logs")).map(|entries| entries.filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "launched")).count()).unwrap_or(0)
}

#[test]
#[ignore = "spawns and SIGKILLs one disposable coordinator per durable boundary"]
fn sigkill_at_every_commit_preserves_receipts_and_rework_without_duplicate_effects() {
    use std::os::unix::process::ExitStatusExt;
    let mut recovered = 0;
    let mut uncertain = 0;
    for boundary in 1..150 {
        let scratch = Scratch::new("process-crash");
        let root = scratch.join("run");
        std::fs::create_dir_all(&root).unwrap();
        let initial = child(&root, boundary);
        if initial.status.success() {
            let result:Value = serde_json::from_slice(&std::fs::read(root.join("child-result.json")).unwrap()).unwrap();
            assert_eq!(result["report"]["outcome"], "settled", "{result}");
            assert!(recovered >= 35);
            assert_eq!(uncertain, 10, "six model calls and four real acceptance commands");
            println!("SIGKILL matrix: {recovered} recovered, {uncertain} ambiguous launch intents refused; accepted envelopes unchanged, no repeated effects");
            return;
        }
        assert_eq!(initial.status.signal(), Some(libc::SIGKILL), "{}", String::from_utf8_lossy(&initial.stderr));
        let before = receipts(&root);
        let effects = launches(&root);
        // Parent and child share neither runtime state nor SQLite handles. This
        // also proves the dead coordinator's exclusive flock is no longer held.
        let resumed = child(&root, 0);
        assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
        let result:Value = serde_json::from_slice(&std::fs::read(root.join("child-result.json")).unwrap()).unwrap();
        let after = receipts(&root);
        assert_eq!(&after[..before.len()], &before, "accepted messages changed at {boundary}");
        if let Some(error) = result["error"].as_str() {
            assert!(error.contains("uncertain launch has no host evidence"), "boundary {boundary}: {error}");
            assert_eq!(launches(&root), effects, "uncertain effect was repeated");
            uncertain += 1;
        } else {
            assert_eq!(result["report"]["outcome"], "settled", "boundary {boundary}: {result}");
            assert_eq!(result["report"]["executions"].as_array().unwrap().len(), 4);
            assert_eq!(launches(&root), 10);
            assert_eq!(after.len(), 12);
            assert!(Journal::open(&root.join("runtime.db")).unwrap().pending().unwrap().is_empty());
            recovered += 1;
        }
    }
    panic!("did not exhaust durable boundaries");
}
