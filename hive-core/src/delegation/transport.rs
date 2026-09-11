use hive_common::protocol::WorkerInfo;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::process::Stdio;
use tokio::{io::AsyncWriteExt, process::Command};

pub const RUNNER: &str = include_str!("runner/runner.py");
const CLAUDE: &str = include_str!("runner/claude.mjs");
const CLAUDE_PYTHON: &str = include_str!("runner/claude_python.py");
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}
pub fn bundle_name() -> String {
    format!(
        "v1-{:x}",
        Sha256::digest(format!("{RUNNER}{CLAUDE}{CLAUDE_PYTHON}").as_bytes())
    )
}

pub async fn ssh(
    worker: &WorkerInfo,
    command: &str,
    input: Option<&Value>,
) -> anyhow::Result<String> {
    ssh_timeout(worker, command, input, 45).await
}

pub async fn ssh_timeout(
    worker: &WorkerInfo,
    command: &str,
    input: Option<&Value>,
    seconds: u64,
) -> anyhow::Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=2",
    ]);
    if let Some(port) = worker.port {
        cmd.args(["-p", &port.to_string()]);
    }
    cmd.arg(worker.ssh_target())
        .arg(format!("{}; {command}", crate::workers::ssh::REMOTE_PATH))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let data = input.map(Value::to_string).unwrap_or_default();
    let mut stdin = child.stdin.take().unwrap();
    let writer = tokio::spawn(async move {
        stdin.write_all(data.as_bytes()).await?;
        stdin.shutdown().await
    });
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(seconds),
        child.wait_with_output(),
    )
    .await??;
    writer.await??;
    anyhow::ensure!(
        output.status.success(),
        "{}: {}",
        worker.name,
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(2000)
            .collect::<String>()
    );
    Ok(String::from_utf8(output.stdout)?)
}

pub async fn deploy(worker: &WorkerInfo) -> anyhow::Result<String> {
    // Private, content-addressed bundle. Never edit a runner used by active work.
    let script = r#"import json,os,pathlib,sys
os.umask(0o077)
v=json.load(sys.stdin)
p=pathlib.Path.home()/'.hive'/'runners'/v['version']
p.mkdir(parents=True,exist_ok=True)
for name,content in v['files'].items():
 f=p/name
 if f.exists() and f.read_text()!=content: raise RuntimeError('Runner bundle digest collision')
 if not f.exists(): f.write_text(content)
print(str(p/'runner.py'))"#;
    let output = ssh(
        worker,
        &format!("python3 -c {}", quote(script)),
        Some(&json!({"version":bundle_name(),"files":{"runner.py":RUNNER,"claude.mjs":CLAUDE,"claude_python.py":CLAUDE_PYTHON}})),
    )
    .await?;
    Ok(output.trim().to_string())
}

pub async fn control(
    worker: &WorkerInfo,
    runner: &str,
    operation: &str,
    id: &str,
    after: i64,
    input: Option<&Value>,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        [
            "launch",
            "snapshot",
            "enqueue",
            "decide",
            "reconcile-inspect",
            "reconcile"
        ]
        .contains(&operation),
        "Invalid runner operation"
    );
    uuid::Uuid::parse_str(id)?;
    ssh(
        worker,
        &format!(
            "python3 {} {operation} --run-id {} --after {after}",
            quote(runner),
            quote(id)
        ),
        input,
    )
    .await
}

/// Update peer topology without replacing the native conversation or replaying
/// its original prompt. This control operation is compatible with v1 journals.
pub async fn update_peers(worker: &WorkerInfo, id: &str, peers: &Value) -> anyhow::Result<()> {
    uuid::Uuid::parse_str(id)?;
    let script = r#"import sqlite3,json,pathlib,sys,hashlib
v=json.load(sys.stdin)
p=pathlib.Path.home()/'.hive'/'runs'/v['id']/'journal.db'
c=sqlite3.connect(str(p),timeout=30)
a=json.loads(c.execute("SELECT value FROM metadata WHERE key='assignment'").fetchone()[0])
a['peers']=v['peers']
s=json.dumps(v['peers'],sort_keys=True)
id='topology-'+hashlib.sha256(s.encode()).hexdigest()
message=json.dumps({'id':id,'text':'Hive updated the task peers. Continue your same workspace and conversation. Superseded peers will not reply. Use these current peer run IDs: '+s})
with c:
 c.execute("UPDATE metadata SET value=? WHERE key='assignment'",(json.dumps(a),))
 c.execute("INSERT OR IGNORE INTO inbox(id,payload) VALUES (?,?)",(id,message))
"#;
    ssh(
        worker,
        &format!("python3 -c {}", quote(script)),
        Some(&json!({"id":id,"peers":peers})),
    )
    .await?;
    Ok(())
}
