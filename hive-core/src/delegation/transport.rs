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
    let wrapped;
    let command = match &worker.container {
        Some(container) => {
            wrapped = super::containers::exec(container, command, false);
            wrapped.as_str()
        }
        None => command,
    };
    let mut cmd = if worker.local {
        local_shell(command)
    } else {
        remote_shell(worker, command)
    };
    cmd.stdin(Stdio::piped())
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

fn remote_shell(worker: &WorkerInfo, command: &str) -> Command {
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
    cmd.arg(worker.ssh_target()).arg(format!(
        "{}; {command}",
        crate::workers::ssh::REMOTE_PATH
    ));
    cmd
}

/// The environment an SSH login would give, and nothing else: hive-web's own
/// variables (credentials, config paths) never reach agents on the coordinator.
const LOCAL_ENV: [&str; 6] = ["HOME", "USER", "LOGNAME", "SHELL", "LANG", "TMPDIR"];

pub fn local_shell(command: &str) -> Command {
    let mut std_cmd = std::process::Command::new("/bin/sh");
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        std_cmd.pre_exec(|| {
            for fd in 3..1024 {
                libc::close(fd);
            }
            Ok(())
        });
    }
    let mut cmd = Command::from(std_cmd);
    cmd.env_clear()
        .envs(LOCAL_ENV.iter().filter_map(|k| Some((*k, std::env::var_os(k)?))))
        .env("PATH", crate::memory::machines::login_path())
        .arg("-c")
        .arg(format!("{}; {command}", crate::workers::ssh::REMOTE_PATH));
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    cmd
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

// Compare and enqueue in one transaction: retries are no-ops, while returning
// to an earlier roster is a new transition and must produce a fresh message.
const UPDATE_PEERS_SCRIPT: &str = r#"import sqlite3,json,pathlib,sys,uuid
v=json.load(sys.stdin)
p=pathlib.Path.home()/'.hive'/'runs'/v['id']/'journal.db'
c=sqlite3.connect(str(p),timeout=30)
with c:
 c.execute('BEGIN IMMEDIATE')
 a=json.loads(c.execute("SELECT value FROM metadata WHERE key='assignment'").fetchone()[0])
 if a.get('peers') != v['peers']:
  a['peers']=v['peers']
  s=json.dumps(v['peers'],sort_keys=True)
  id='topology-'+str(uuid.uuid4())
  message=json.dumps({'id':id,'text':'Hive updated the task team (roles, owned paths, status and dependencies). Continue your same workspace and conversation. Superseded peers will not reply. Use these current peer run IDs: '+s})
  c.execute("UPDATE metadata SET value=? WHERE key='assignment'",(json.dumps(a),))
  c.execute("INSERT INTO inbox(id,payload) VALUES (?,?)",(id,message))
"#;

/// Update peer topology without replacing the native conversation or replaying
/// its original prompt. This control operation is compatible with v1 journals.
pub async fn update_peers(worker: &WorkerInfo, id: &str, peers: &Value) -> anyhow::Result<()> {
    uuid::Uuid::parse_str(id)?;
    ssh(
        worker,
        &format!("python3 -c {}", quote(UPDATE_PEERS_SCRIPT)),
        Some(&json!({"id":id,"peers":peers})),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_transition_queues_exactly_one_prompt_refresh() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let path = std::env::temp_dir().join(format!("hive-team-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE inbox(id TEXT PRIMARY KEY,payload TEXT); INSERT INTO metadata VALUES ('assignment','{\"peers\":[]}');").unwrap();
        let script = UPDATE_PEERS_SCRIPT.replace(
            "pathlib.Path.home()/'.hive'/'runs'/v['id']/'journal.db'",
            &format!("pathlib.Path({})", serde_json::to_string(&path).unwrap()),
        );
        let update = |peers: Value| {
            let mut child = Command::new("python3").args(["-c", &script])
                .stdin(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(json!({"peers":peers}).to_string().as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
        };
        let peers = json!([{"id":"peer", "role":"backend", "owned_paths":["src/**"], "status":"working", "dependencies":[]}]);
        update(peers.clone());
        update(peers.clone()); // Lost reply / repeated synchronization.
        assert_eq!(db.query_row("SELECT count(*) FROM inbox", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
        let prompt: String = db.query_row("SELECT payload FROM inbox", [], |r| r.get(0)).unwrap();
        assert!(prompt.contains("backend") && prompt.contains("src/**") && prompt.contains("working"));
        update(json!([]));
        update(peers); // A -> B -> A is another change, not a duplicate.
        assert_eq!(db.query_row("SELECT count(*) FROM inbox", [], |r| r.get::<_,i64>(0)).unwrap(), 3);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    fn coordinator() -> WorkerInfo {
        WorkerInfo {
            name: "mac-mini".into(),
            host: "localhost".into(),
            user: "test".into(),
            port: None,
            tags: vec![],
            local: true,
            container: None,
        }
    }

    #[tokio::test]
    async fn coordinator_commands_run_locally_with_only_a_login_environment() {
        // Stands in for hive-web's credentials in its own environment.
        std::env::set_var("HIVE_TRANSPORT_TEST_SECRET", "leaked");
        let out = ssh(
            &coordinator(),
            r#"printf '%s|%s|' "${HIVE_TRANSPORT_TEST_SECRET-unset}" "$(pwd -P)"; cat; case ":$PATH:" in *:/opt/homebrew/bin:*) printf '|path';; esac"#,
            Some(&json!({"a": 1})),
        )
        .await
        .unwrap();
        let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
        assert_eq!(out, format!("unset|{}|{{\"a\":1}}|path", home.display()));
    }

    #[tokio::test]
    async fn coordinator_failures_and_timeouts_report_like_ssh() {
        let err = ssh(&coordinator(), "echo nope >&2; exit 3", None).await.unwrap_err();
        assert_eq!(err.to_string(), "mac-mini: nope\n");
        assert!(ssh_timeout(&coordinator(), "sleep 5", None, 1).await.is_err());
    }

    #[tokio::test]
    async fn coordinator_commands_use_login_shell_path() {
        let expected = crate::memory::machines::login_path();
        let out = ssh(&coordinator(), "printf '%s' \"$PATH\"", None)
            .await
            .unwrap();
        assert!(out.contains(&expected));
    }
}
