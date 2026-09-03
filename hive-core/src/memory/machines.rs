//! The machine knowledge graph.
//!
//! Hive has to answer "which computer should run this?" — today trivially,
//! because there is one Linux worker, but the answer stops being trivial the
//! moment a second machine appears. Rather than hardcode the choice now and
//! rewrite it later, every machine is probed and projected into the knowledge
//! graph as entities and relations:
//!
//! ```text
//!   machine:lawfinder ──runs_os──────► os:ubuntu-24.04
//!                     ──has_arch─────► arch:x86_64
//!                     ──has_tool─────► tool:claude, tool:codex, tool:tmux, …
//!                     ──has_capability► capability:agentic-cli
//! ```
//!
//! Selection then becomes a graph query ("machines with `tool:codex` and the
//! most free memory") instead of a hardcoded branch, and [`describe_for_prompt`]
//! renders the same graph into the planner's prompt so the LLM can reason about
//! the fleet in words.

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use super::graph::{entity_id, Entity, KnowledgeGraph};
use crate::workers::ssh::SshWorker;

/// One probe of one machine.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MachineFacts {
    pub name: String,
    /// SSH target, or `"local"` for the master itself.
    pub host: String,
    pub reachable: bool,
    pub os: String,
    pub os_version: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_cores: u32,
    pub memory_gb: f64,
    /// Memory actually free right now.
    ///
    /// On a shared machine this is the number that matters: a login node with
    /// 65 users can advertise 15 GB total while 11 GB of it belongs to other
    /// people. Ranking placement on total memory sends work to the busiest
    /// host precisely because it is large.
    #[serde(default)]
    pub memory_available_gb: f64,
    pub disk_free_gb: f64,
    pub gpu: Option<String>,
    /// Number of GPUs, so a multi-GPU host is distinguishable from a single.
    #[serde(default)]
    pub gpu_count: u32,
    /// Batch scheduler present (e.g. `slurm`), if any.
    ///
    /// A node running a scheduler is shared infrastructure: heavy work belongs
    /// in a queued job, not in a tmux session started behind the scheduler's
    /// back. Recorded so placement can respect that.
    #[serde(default)]
    pub scheduler: Option<String>,
    /// Tools found on `PATH`, from [`PROBED_TOOLS`].
    pub tools: Vec<String>,
    /// Operator-assigned tags from `workers.toml`.
    pub tags: Vec<String>,
    pub probed_at: String,
}

/// Tools worth knowing about when placing work.
pub const PROBED_TOOLS: &[&str] = &[
    "claude",
    "codex",
    "ollama",
    "git",
    "tmux",
    "docker",
    "cargo",
    "rustc",
    "node",
    "python3",
    "psql",
    "nginx",
    "ffmpeg",
    "gh",
    "nvidia-smi",
    // CUDA toolchain and batch scheduler: both change where work should go.
    "nvcc", "sbatch", "srun",
];

/// Capabilities inferred from what's installed. These are what a planner
/// actually wants to ask about — "can this box run an agentic CLI?" rather
/// than "does it have codex?".
fn capabilities_for(tools: &[String]) -> Vec<&'static str> {
    let has = |t: &str| tools.iter().any(|x| x == t);
    let mut caps = Vec::new();
    if has("claude") || has("codex") {
        caps.push("agentic-cli");
    }
    if has("ollama") {
        caps.push("local-inference");
    }
    if has("nvidia-smi") {
        caps.push("gpu-compute");
    }
    if has("sbatch") || has("srun") {
        caps.push("batch-scheduler");
    }
    if has("docker") {
        caps.push("containers");
    }
    if has("cargo") || has("node") || has("python3") {
        caps.push("build");
    }
    if has("tmux") {
        caps.push("supervised-sessions");
    }
    if has("psql") {
        caps.push("database");
    }
    caps
}

/// One portable script, run on Linux and macOS alike, emitting `key=value`.
///
/// It is deliberately tolerant: every lookup falls back to empty rather than
/// failing the probe, because a machine that answers half the questions is
/// still worth having in the graph.
fn probe_script() -> String {
    let tool_checks = PROBED_TOOLS
        .iter()
        .map(|t| format!("command -v {t} >/dev/null 2>&1 && echo tool={t}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"
kernel_name=$(uname -s 2>/dev/null || echo unknown)
echo "arch=$(uname -m 2>/dev/null)"
echo "kernel=$(uname -r 2>/dev/null)"
echo "hostname=$(uname -n 2>/dev/null)"
if [ "$kernel_name" = "Darwin" ]; then
  echo "os=macos"
  echo "os_version=$(sw_vers -productVersion 2>/dev/null)"
  echo "cores=$(sysctl -n hw.ncpu 2>/dev/null)"
  echo "memory_gb=$(echo "scale=2; $(sysctl -n hw.memsize 2>/dev/null) / 1073741824" | bc 2>/dev/null)"
  echo "memory_available_gb=$(echo "scale=2; $(vm_stat 2>/dev/null | awk '/Pages free/ {{gsub(/\./,"",$3); f=$3}} /Pages inactive/ {{gsub(/\./,"",$3); i=$3}} END {{print (f+i)*4096}}') / 1073741824" | bc 2>/dev/null)"
  echo "disk_free_gb=$(df -g / 2>/dev/null | awk 'NR==2 {{print $4}}')"
  echo "gpu=$(system_profiler SPDisplaysDataType 2>/dev/null | awk -F': ' '/Chipset Model/ {{print $2; exit}}')"
else
  echo "os=$(. /etc/os-release 2>/dev/null && echo "$ID" || echo linux)"
  echo "os_version=$(. /etc/os-release 2>/dev/null && echo "$VERSION_ID")"
  echo "cores=$(nproc 2>/dev/null)"
  echo "memory_gb=$(awk '/MemTotal/ {{printf "%.2f", $2/1048576}}' /proc/meminfo 2>/dev/null)"
  echo "memory_available_gb=$(awk '/MemAvailable/ {{printf "%.2f", $2/1048576}}' /proc/meminfo 2>/dev/null)"
  echo "disk_free_gb=$(df -BG / 2>/dev/null | awk 'NR==2 {{gsub(/G/,"",$4); print $4}}')"
  # Report the whole set, not just the first card: a two-GPU box and a
  # one-GPU box of the same model are very different placement targets.
  # `sort | uniq -c` rather than awk, whose braces collide with format!.
  echo "gpu=$(command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null | sort | uniq -c | sed 's/^ *//;s/$/ each/' | paste -sd'; ' -)"
  echo "gpu_count=$(command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l | tr -d ' ')"
  echo "scheduler=$(command -v sbatch >/dev/null 2>&1 && echo slurm)"
fi
{tool_checks}
# Every probe line is best-effort, and `command -v` for a missing tool exits
# non-zero — without this the whole script's status reflects whichever check
# happened to run last, and the probe is discarded.
exit 0
"#
    )
}

fn parse_probe(name: &str, host: &str, tags: Vec<String>, raw: &str) -> MachineFacts {
    let mut facts = MachineFacts {
        name: name.to_string(),
        host: host.to_string(),
        reachable: true,
        tags,
        probed_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };

    for line in raw.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "os" => facts.os = value.to_string(),
            "os_version" => facts.os_version = value.to_string(),
            "kernel" => facts.kernel = value.to_string(),
            "arch" => facts.arch = value.to_string(),
            "cores" => facts.cpu_cores = value.parse().unwrap_or(0),
            "memory_gb" => facts.memory_gb = value.parse().unwrap_or(0.0),
            "memory_available_gb" => facts.memory_available_gb = value.parse().unwrap_or(0.0),
            "disk_free_gb" => facts.disk_free_gb = value.parse().unwrap_or(0.0),
            "gpu" => facts.gpu = Some(value.trim().to_string()),
            "gpu_count" => facts.gpu_count = value.parse().unwrap_or(0),
            "scheduler" => facts.scheduler = Some(value.to_string()),
            "tool" => facts.tools.push(value.to_string()),
            _ => {}
        }
    }
    facts.tools.sort();
    facts.tools.dedup();
    facts
}

/// Probe the master itself.
pub async fn probe_local(name: &str) -> MachineFacts {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(probe_script())
        .output()
        .await;

    match output {
        Ok(out) => parse_probe(
            name,
            "local",
            vec!["master".to_string()],
            &String::from_utf8_lossy(&out.stdout),
        ),
        Err(e) => {
            warn!(error = %e, "local machine probe failed");
            MachineFacts {
                name: name.to_string(),
                host: "local".into(),
                reachable: false,
                ..Default::default()
            }
        }
    }
}

/// Probe a worker over SSH. An unreachable worker still produces facts — with
/// `reachable: false` — so it stays in the graph and can be reported as down
/// rather than silently vanishing.
pub async fn probe_remote(name: &str, ssh_target: &str, tags: Vec<String>) -> MachineFacts {
    let unreachable = || MachineFacts {
        name: name.to_string(),
        host: ssh_target.to_string(),
        reachable: false,
        tags: tags.clone(),
        probed_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };

    let worker = match SshWorker::connect(ssh_target).await {
        Ok(w) => w,
        Err(e) => {
            warn!(worker = name, error = %e, "probe: SSH connect failed");
            return unreachable();
        }
    };

    // The probe runs through a login shell so PATH additions like ~/.local/bin
    // are visible — the same reason session launches use `bash -lc`.
    match worker
        .run(&format!("bash -lc {}", shell_quote(&probe_script())))
        .await
    {
        Ok(stdout) => parse_probe(name, ssh_target, tags, &stdout),
        Err(e) => {
            warn!(worker = name, error = %e, "probe: command failed");
            unreachable()
        }
    }
}

/// Single-quote a string for POSIX shells.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Write one machine's facts into the graph.
///
/// A **failed probe does not erase what we already knew.** An unreachable
/// machine still has an OS, a core count and a set of installed tools; a probe
/// that could not connect learned nothing about them, and overwriting them with
/// zeros turns "temporarily offline" into "unknown machine". That is precisely
/// backwards for a graph whose job is answering "which computer should run
/// this?" — you want to know that the box currently down is the one with
/// `claude` and the legal corpus on it.
///
/// So an unreachable probe updates only reachability and the probe timestamp,
/// leaving specs and relations as last observed. A successful probe replaces
/// everything, since it did observe it.
pub fn project_into_graph(kg: &KnowledgeGraph, facts: &MachineFacts) -> anyhow::Result<()> {
    if !facts.reachable {
        if let Ok(Some(existing)) = kg.entity(&entity_id("machine", &facts.name)) {
            let mut attrs = existing.attrs.clone();
            if let Some(map) = attrs.as_object_mut() {
                map.insert("reachable".into(), json!(false));
                map.insert("probed_at".into(), json!(facts.probed_at));
            }
            kg.upsert_entity(&Entity {
                attrs,
                ..existing
            })?;
            debug!(machine = %facts.name, "unreachable; kept last known facts");
            return Ok(());
        }
        // Never seen before and unreachable: record what little we have, so it
        // at least appears in the fleet as a machine that exists and is down.
    }

    let machine = Entity::new(
        "machine",
        &facts.name,
        json!({
            "host": facts.host,
            "reachable": facts.reachable,
            "os": facts.os,
            "os_version": facts.os_version,
            "kernel": facts.kernel,
            "arch": facts.arch,
            "cpu_cores": facts.cpu_cores,
            "memory_gb": facts.memory_gb,
            "memory_available_gb": facts.memory_available_gb,
            "disk_free_gb": facts.disk_free_gb,
            "gpu": facts.gpu,
            "gpu_count": facts.gpu_count,
            "scheduler": facts.scheduler,
            "tags": facts.tags,
            "probed_at": facts.probed_at,
        }),
    );
    kg.upsert_entity(&machine)?;

    // Re-probing must not leave behind tools that were uninstalled.
    for relation in ["runs_os", "has_arch", "has_tool", "has_capability"] {
        kg.clear_relation(&machine.id, relation)?;
    }

    if !facts.os.is_empty() {
        let os_name = if facts.os_version.is_empty() {
            facts.os.clone()
        } else {
            format!("{}-{}", facts.os, facts.os_version)
        };
        let os = Entity::new("os", &os_name, json!({"family": facts.os}));
        kg.upsert_entity(&os)?;
        kg.add_edge(&machine.id, "runs_os", &os.id)?;
    }

    if !facts.arch.is_empty() {
        let arch = Entity::new("arch", &facts.arch, json!({}));
        kg.upsert_entity(&arch)?;
        kg.add_edge(&machine.id, "has_arch", &arch.id)?;
    }

    for tool in &facts.tools {
        let t = Entity::new("tool", tool, json!({}));
        kg.upsert_entity(&t)?;
        kg.add_edge(&machine.id, "has_tool", &t.id)?;
    }

    for cap in capabilities_for(&facts.tools) {
        let c = Entity::new("capability", cap, json!({}));
        kg.upsert_entity(&c)?;
        kg.add_edge(&machine.id, "has_capability", &c.id)?;
    }

    debug!(machine = %facts.name, tools = facts.tools.len(), "projected into knowledge graph");
    Ok(())
}

/// Whether a machine is shared infrastructure, per its operator-assigned tags.
///
/// `shared` marks a host other people are using too (a university cluster node,
/// a login server). It is a placement hint, not a capability: such a machine can
/// do the work, it just should not be the default destination for it.
fn is_shared(machine: &Entity) -> bool {
    machine
        .attrs
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|tags| {
            tags.iter()
                .filter_map(|t| t.as_str())
                .any(|t| t == "shared" || t == "login-node")
        })
        .unwrap_or(false)
}

/// Remove machines from the graph that are not in `known`.
///
/// Returns the names dropped. Called after a refresh, so the graph tracks the
/// configured fleet rather than accumulating every host ever probed.
pub fn prune_unknown(kg: &KnowledgeGraph, known: &[String]) -> anyhow::Result<Vec<String>> {
    let mut removed = Vec::new();
    for machine in kg.entities_of_kind("machine")? {
        if !known.iter().any(|k| k == &machine.name) {
            if kg.remove_entity(&machine.id)? {
                removed.push(machine.name);
            }
        }
    }
    Ok(removed)
}

/// Machines that have every one of `capabilities`, best first.
///
/// "Best" is free memory then core count — a starting heuristic, and the point
/// at which a smarter policy would slot in without callers changing.
pub fn machines_with_capabilities(
    kg: &KnowledgeGraph,
    capabilities: &[&str],
) -> anyhow::Result<Vec<Entity>> {
    let mut candidates: Option<Vec<Entity>> = None;

    for cap in capabilities {
        let matching = kg.sources_of("has_capability", &entity_id("capability", cap))?;
        candidates = Some(match candidates {
            None => matching,
            Some(prev) => prev
                .into_iter()
                .filter(|m| matching.iter().any(|x| x.id == m.id))
                .collect(),
        });
    }

    let mut out: Vec<Entity> = match candidates {
        Some(list) => list,
        None => kg.entities_of_kind("machine")?,
    };

    out.retain(|m| {
        m.attrs
            .get("reachable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });
    // Rank on memory that is actually free, falling back to total where a probe
    // did not report availability. Cores break ties.
    //
    // Total memory is the wrong key on shared hosts: a login node with 65 users
    // can advertise more RAM than a quiet dedicated box while almost none of it
    // is usable, and ranking on the advertised figure sends work to the busiest
    // machine precisely because it is the largest.
    out.sort_by(|a, b| {
        let key = |e: &Entity| {
            let free = e
                .attr_f64("memory_available_gb")
                .filter(|v| *v > 0.0)
                .or_else(|| e.attr_f64("memory_gb"))
                .unwrap_or(0.0);
            // Dedicated machines outrank shared ones regardless of size.
            //
            // Otherwise adding one big shared host silently redirects *all*
            // default work onto it: a 251 GB university node with 30 other
            // users would outrank a quiet 11 GB box we actually own. Shared
            // infrastructure should be chosen when its capabilities are asked
            // for, not merely because it is the largest thing available.
            (!is_shared(e), free, e.attr_f64("cpu_cores").unwrap_or(0.0))
        };
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// Render the fleet as prose for an LLM prompt.
///
/// This is the bridge between the graph and the planner: the model sees what
/// machines exist and what each can do, so "run the scraper on the box with the
/// legal corpus" can resolve to a real host.
pub fn describe_for_prompt(kg: &KnowledgeGraph) -> anyhow::Result<String> {
    let machines = kg.entities_of_kind("machine")?;
    if machines.is_empty() {
        return Ok("No machines are known yet.".to_string());
    }

    let mut out = String::from("Known machines:\n");
    for m in &machines {
        let tools = kg
            .neighbors(&m.id, "has_tool")?
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>();
        let caps = kg
            .neighbors(&m.id, "has_capability")?
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>();
        let os = kg
            .neighbors(&m.id, "runs_os")?
            .first()
            .map(|o| o.name.clone())
            .unwrap_or_else(|| "unknown".into());

        let reachable = m
            .attrs
            .get("reachable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push_str(&format!(
            "- {} ({}): {}, {} cores, {} RAM, {:.0} GB disk free{}. Capabilities: {}. Tools: {}.\n",
            m.name,
            if reachable { "online" } else { "OFFLINE" },
            os,
            m.attr_f64("cpu_cores").unwrap_or(0.0) as u32,
            match m.attr_f64("memory_available_gb").filter(|v| *v > 0.0) {
                Some(free) => format!(
                    "{:.1} GB free of {:.1} GB",
                    free,
                    m.attr_f64("memory_gb").unwrap_or(0.0)
                ),
                None => format!("{:.1} GB", m.attr_f64("memory_gb").unwrap_or(0.0)),
            },
            m.attr_f64("disk_free_gb").unwrap_or(0.0),
            m.attr_str("gpu")
                .filter(|g| !g.trim().is_empty())
                .map(|g| format!(", GPU: {}", g.trim()))
                .unwrap_or_default(),
            if caps.is_empty() { "none detected".into() } else { caps.join(", ") },
            if tools.is_empty() { "none detected".into() } else { tools.join(", ") },
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str, tools: &[&str], mem: f64, reachable: bool) -> MachineFacts {
        MachineFacts {
            name: name.into(),
            host: name.into(),
            reachable,
            os: "ubuntu".into(),
            os_version: "24.04".into(),
            arch: "x86_64".into(),
            cpu_cores: 2,
            memory_gb: mem,
            tools: tools.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_a_probe_transcript() {
        let raw = "arch=x86_64\nkernel=6.17.0\nos=ubuntu\nos_version=24.04\ncores=2\n\
                   memory_gb=7.75\ndisk_free_gb=48\ntool=claude\ntool=codex\ntool=tmux\n";
        let f = parse_probe("lawfinder", "hive-worker-1", vec!["legal".into()], raw);
        assert_eq!(f.arch, "x86_64");
        assert_eq!(f.cpu_cores, 2);
        assert_eq!(f.memory_gb, 7.75);
        assert_eq!(f.tools, vec!["claude", "codex", "tmux"]);
        assert_eq!(f.tags, vec!["legal"]);
        assert!(f.reachable);
    }

    #[test]
    fn probe_tolerates_missing_and_malformed_fields() {
        let f = parse_probe(
            "x",
            "h",
            vec![],
            "arch=arm64\ncores=\ngarbage line\nos=macos\n",
        );
        assert_eq!(f.arch, "arm64");
        assert_eq!(f.cpu_cores, 0);
        assert_eq!(f.os, "macos");
    }

    #[test]
    fn a_scheduler_node_is_flagged_as_such() {
        let caps = capabilities_for(&["sbatch".into(), "srun".into(), "nvidia-smi".into()]);
        assert!(caps.contains(&"batch-scheduler"), "shared cluster nodes must be identifiable");
        assert!(caps.contains(&"gpu-compute"));
    }

    #[test]
    fn probe_reads_multiple_gpus_and_a_scheduler() {
        let raw = "gpu=2x NVIDIA RTX A6000 (49140 MiB each) \ngpu_count=2\nscheduler=slurm\n";
        let f = parse_probe("cis-a6000", "cis-a6000", vec![], raw);
        assert_eq!(f.gpu_count, 2);
        assert_eq!(f.scheduler.as_deref(), Some("slurm"));
        assert!(f.gpu.unwrap().contains("A6000"));
    }

    #[test]
    fn infers_capabilities_from_tools() {
        let caps = capabilities_for(&["claude".into(), "tmux".into(), "cargo".into()]);
        assert!(caps.contains(&"agentic-cli"));
        assert!(caps.contains(&"supervised-sessions"));
        assert!(caps.contains(&"build"));
        assert!(!caps.contains(&"gpu-compute"));
    }

    #[test]
    fn projects_a_machine_and_answers_capability_queries() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(
            &kg,
            &facts("lawfinder", &["claude", "codex", "tmux"], 7.7, true),
        )
        .unwrap();
        project_into_graph(&kg, &facts("tiny", &["git"], 1.0, true)).unwrap();

        let agentic = machines_with_capabilities(&kg, &["agentic-cli"]).unwrap();
        assert_eq!(agentic.len(), 1);
        assert_eq!(agentic[0].name, "lawfinder");

        assert_eq!(
            kg.neighbors("machine:lawfinder", "runs_os").unwrap()[0].name,
            "ubuntu-24.04"
        );
    }

    #[test]
    fn ranks_on_free_memory_not_total() {
        // The case this exists for: a shared login node advertising more total
        // RAM than a quiet dedicated box, while almost none of it is free.
        let kg = KnowledgeGraph::in_memory().unwrap();
        let mut busy = facts("login-node", &["claude"], 15.0, true);
        busy.memory_available_gb = 3.8;
        let mut quiet = facts("dedicated", &["claude"], 11.6, true);
        quiet.memory_available_gb = 10.4;
        project_into_graph(&kg, &busy).unwrap();
        project_into_graph(&kg, &quiet).unwrap();

        let ranked = machines_with_capabilities(&kg, &["agentic-cli"]).unwrap();
        assert_eq!(
            ranked.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["dedicated", "login-node"],
            "a busy machine must not outrank a free one just for being large"
        );
    }

    #[test]
    fn dedicated_machines_outrank_shared_ones_however_big() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        let mut cluster = facts("uni-cluster", &["claude"], 251.0, true);
        cluster.memory_available_gb = 243.0;
        cluster.tags = vec!["shared".into(), "gpu".into()];
        let mut mine = facts("my-box", &["claude"], 11.6, true);
        mine.memory_available_gb = 10.9;
        project_into_graph(&kg, &cluster).unwrap();
        project_into_graph(&kg, &mine).unwrap();

        let ranked = machines_with_capabilities(&kg, &["agentic-cli"]).unwrap();
        assert_eq!(
            ranked.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["my-box", "uni-cluster"],
            "a 251 GB shared cluster must not become the default destination"
        );
        // Still selectable — it is deprioritized, not excluded.
        assert!(ranked.iter().any(|m| m.name == "uni-cluster"));
    }

    #[test]
    fn a_login_node_counts_as_shared() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        let mut login = facts("login", &["claude"], 15.0, true);
        login.memory_available_gb = 14.0;
        login.tags = vec!["login-node".into()];
        let mut dedicated = facts("mine", &["claude"], 8.0, true);
        dedicated.memory_available_gb = 7.0;
        project_into_graph(&kg, &login).unwrap();
        project_into_graph(&kg, &dedicated).unwrap();
        let ranked = machines_with_capabilities(&kg, &["agentic-cli"]).unwrap();
        assert_eq!(ranked[0].name, "mine");
    }

    #[test]
    fn falls_back_to_total_memory_when_availability_is_unknown() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("small", &["claude"], 8.0, true)).unwrap();
        project_into_graph(&kg, &facts("big", &["claude"], 64.0, true)).unwrap();
        let ranked = machines_with_capabilities(&kg, &["agentic-cli"]).unwrap();
        assert_eq!(ranked[0].name, "big");
    }

    #[test]
    fn ranks_by_memory_and_skips_unreachable() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("small", &["claude"], 8.0, true)).unwrap();
        project_into_graph(&kg, &facts("big", &["claude"], 64.0, true)).unwrap();
        project_into_graph(&kg, &facts("down", &["claude"], 128.0, false)).unwrap();

        let ranked = machines_with_capabilities(&kg, &["agentic-cli"]).unwrap();
        assert_eq!(
            ranked.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["big", "small"],
            "offline machines must not be selectable, and bigger should rank first"
        );
    }

    #[test]
    fn reprobing_drops_uninstalled_tools() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("m", &["claude", "docker"], 8.0, true)).unwrap();
        project_into_graph(&kg, &facts("m", &["claude"], 8.0, true)).unwrap();
        let tools = kg.neighbors("machine:m", "has_tool").unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["claude"]
        );
    }

    #[test]
    fn an_unreachable_probe_keeps_what_we_already_knew() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("lawfinder", &["claude", "codex", "psql"], 7.7, true)).unwrap();

        // The box goes offline; the probe learns nothing.
        let mut down = facts("lawfinder", &[], 0.0, false);
        down.os = String::new();
        down.arch = String::new();
        down.cpu_cores = 0;
        project_into_graph(&kg, &down).unwrap();

        let m = kg.entity("machine:lawfinder").unwrap().expect("still present");
        assert_eq!(m.attrs.get("reachable").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(m.attr_f64("memory_gb"), Some(7.7), "specs must survive");
        assert_eq!(m.attr_f64("cpu_cores"), Some(2.0));
        let tools: Vec<String> = kg
            .neighbors("machine:lawfinder", "has_tool")
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(tools.contains(&"claude".to_string()), "tools must survive: {tools:?}");
    }

    #[test]
    fn a_machine_first_seen_while_down_is_still_recorded() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        let mut down = facts("never-seen", &[], 0.0, false);
        down.os = String::new();
        project_into_graph(&kg, &down).unwrap();
        let m = kg.entity("machine:never-seen").unwrap().expect("recorded");
        assert_eq!(m.attrs.get("reachable").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn a_successful_reprobe_replaces_stale_facts() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("m", &["claude", "docker"], 8.0, true)).unwrap();
        project_into_graph(&kg, &facts("m", &["claude"], 16.0, true)).unwrap();
        let m = kg.entity("machine:m").unwrap().unwrap();
        assert_eq!(m.attr_f64("memory_gb"), Some(16.0));
        let tools: Vec<String> = kg.neighbors("machine:m", "has_tool").unwrap()
            .into_iter().map(|t| t.name).collect();
        assert_eq!(tools, vec!["claude"], "a real probe still prunes removed tools");
    }

    #[test]
    fn pruning_drops_decommissioned_machines_only() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("keep-me", &["claude"], 8.0, true)).unwrap();
        project_into_graph(&kg, &facts("retired", &["claude"], 8.0, false)).unwrap();

        let dropped = prune_unknown(&kg, &["keep-me".to_string()]).unwrap();
        assert_eq!(dropped, vec!["retired"]);
        assert!(kg.entity("machine:retired").unwrap().is_none());
        assert!(kg.entity("machine:keep-me").unwrap().is_some());

        // Idempotent: a second pass finds nothing left to drop.
        assert!(prune_unknown(&kg, &["keep-me".to_string()]).unwrap().is_empty());
    }

    #[test]
    fn describes_the_fleet_for_a_prompt() {
        let kg = KnowledgeGraph::in_memory().unwrap();
        project_into_graph(&kg, &facts("lawfinder", &["claude", "codex"], 7.7, true)).unwrap();
        let text = describe_for_prompt(&kg).unwrap();
        assert!(text.contains("lawfinder (online)"));
        assert!(text.contains("ubuntu-24.04"));
        assert!(text.contains("agentic-cli"));
    }
}
