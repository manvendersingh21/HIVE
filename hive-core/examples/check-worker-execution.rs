//! Short live SSH/tmux smoke test; creates only uniquely named temporary logs.
use hive_common::config::WorkersConfig;
use hive_core::workers::ssh::SshWorker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = WorkersConfig::from_project_root(std::path::Path::new("."))?;
    let results = futures::future::join_all(config.workers.iter().map(|w| async move {
        let check = async {
            let ssh = SshWorker::connect(&w.ssh_target()).await?;
            ssh.execution_ready().await?;
            let name = format!("hive-exec-check-{}", uuid::Uuid::new_v4().simple());
            let log = format!("/tmp/{name}.log");
            ssh.spawn_tmux(&name, "uname -n; command -v tmux; tmux -V", &log)
                .await?;
            let result = async {
                for _ in 0..20 {
                    if let Ok(out) = ssh.run(&format!("cat '{log}'")).await {
                        if out.contains("__HIVE_DONE__0") {
                            return Ok(out);
                        }
                        anyhow::ensure!(!out.contains("__HIVE_DONE__"), "command failed: {out}");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                anyhow::bail!("completion sentinel not received")
            }
            .await;
            // This exact log/session was created by this invocation only.
            let cleanup = ssh
                .run(&format!(
                    "tmux kill-session -t '={name}' 2>/dev/null || true; rm -f '{log}'"
                ))
                .await;
            let out = result?;
            cleanup?;
            println!(
                "PASS {}: {}",
                w.name,
                out.lines().collect::<Vec<_>>().join(" | ")
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), check).await?
    }))
    .await;
    for r in results {
        r?;
    }
    Ok(())
}
