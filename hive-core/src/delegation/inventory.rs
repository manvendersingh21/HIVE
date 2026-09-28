use super::transport;
use crate::{
    agent::MasterAgent,
    memory::graph::{entity_id, Entity, KnowledgeGraph},
};
use serde_json::{json, Value};

/// A probe starts every agent CLI to read versions and auth, which takes close
/// to a minute on a loaded worker.
pub const PROBE_SECONDS: u64 = 150;

/// Probe every device's agents, the coordinator included.
pub async fn refresh(agent: &MasterAgent) -> anyhow::Result<()> {
    probe_devices(agent, super::targets(agent), PROBE_SECONDS).await
}

/// Probe the named devices now, such as a container that was just added.
pub async fn refresh_devices(agent: &MasterAgent, names: &[&str]) -> anyhow::Result<()> {
    let devices = super::targets(agent)
        .into_iter()
        .filter(|t| names.contains(&t.name.as_str()))
        .collect();
    probe_devices(agent, devices, PROBE_SECONDS).await
}

/// Probe only devices without an inventory verified in the last `max_age`
/// seconds. Planning uses this with a short budget: the background refresh
/// keeps healthy devices current, and a slow probe here must not mark their
/// fresh inventory stale.
pub async fn refresh_stale(agent: &MasterAgent, max_age: i64, seconds: u64) -> anyhow::Result<()> {
    let stale = stale_devices(agent, max_age, chrono::Utc::now().timestamp())?;
    probe_devices(agent, stale, seconds).await
}

fn stale_devices(
    agent: &MasterAgent,
    max_age: i64,
    now: i64,
) -> anyhow::Result<Vec<hive_common::protocol::WorkerInfo>> {
    let records = agent.memory.graph.entities_of_kind("device-agent")?;
    Ok(super::targets(agent)
        .into_iter()
        .filter(|t| {
            !records.iter().any(|r| {
                r.attrs["device"] == t.name.as_str()
                    && !r.attrs["probe_error"].is_string()
                    && r.attrs["verified_at"].as_i64().is_some_and(|v| now - v <= max_age)
            })
        })
        .collect())
}

async fn probe_devices(
    agent: &MasterAgent,
    devices: Vec<hive_common::protocol::WorkerInfo>,
    seconds: u64,
) -> anyhow::Result<()> {
    let probes = devices.iter().map(|worker| async move {
        // Probe existing bundle when present (SDK/runtime status is device-specific).
        let source = format!(
            "__file__ = str(__import__('pathlib').Path.home()/'.hive/runners/{}/runner.py')\n{}",
            transport::bundle_name(),
            transport::RUNNER
        );
        let command = format!("python3 -c {} probe", transport::quote(&source));
        (
            worker.name.clone(),
            transport::ssh_timeout(worker, &command, None, seconds).await,
        )
    });
    for (device, result) in futures::future::join_all(probes).await {
        match result {
            Ok(raw) => match serde_json::from_str::<Vec<Value>>(&raw) {
                // One device that can't be recorded (for example, a worker the
                // machine graph hasn't seen yet) must not drop the rest.
                Ok(records) => {
                    if let Err(e) = project(&agent.memory.graph, &device, &records) {
                        tracing::warn!(%device,error=%e,"could not record agent inventory");
                    }
                }
                Err(e) => {
                    mark_stale(&agent.memory.graph, &device, &e.to_string())?;
                    tracing::warn!(%device,error=%e,"invalid agent inventory");
                }
            },
            Err(e) => {
                mark_stale(&agent.memory.graph, &device, &e.to_string())?;
                tracing::warn!(%device,error=%e,"agent inventory stale");
            }
        }
    }
    Ok(())
}

fn mark_stale(graph: &KnowledgeGraph, device: &str, reason: &str) -> anyhow::Result<()> {
    for mut record in graph.entities_of_kind("device-agent")? {
        if record.attrs["device"] == device {
            record.attrs["probe_error"] = json!(reason);
            graph.upsert_entity(&record)?;
        }
    }
    Ok(())
}

pub fn project(graph: &KnowledgeGraph, device: &str, records: &[Value]) -> anyhow::Result<()> {
    let machine = entity_id("machine", device);
    graph.clear_relation(&machine, "has_agent")?;
    for record in records {
        let Some(name) = record["agent"].as_str() else {
            continue;
        };
        let id = format!("{device}/{name}");
        let mut attrs = record.clone();
        if let Some(existing) = graph.entity(&entity_id("device-agent", &id))? {
            // Discovery never fabricates successful invocation evidence or erases
            // evidence merely because no new inference was requested.
            for key in ["invocation", "models"] {
                if attrs[key].is_null() || attrs[key].as_array().is_some_and(Vec::is_empty) {
                    attrs[key] = existing.attrs[key].clone();
                }
            }
        }
        attrs["device"] = json!(device);
        let entity = Entity::new("device-agent", &id, attrs);
        graph.upsert_entity(&entity)?;
        graph.add_edge(&machine, "has_agent", &entity.id)?;
    }
    Ok(())
}

pub fn describe(graph: &KnowledgeGraph) -> anyhow::Result<String> {
    let mut records = Vec::new();
    for mut entity in graph.entities_of_kind("device-agent")? {
        let device = entity.attrs["device"].as_str().unwrap_or_default();
        let reachable = graph
            .entity(&entity_id("machine", device))?
            .is_some_and(|m| m.attrs["reachable"] == true);
        let age = entity.attrs["verified_at"]
            .as_i64()
            .map(|t| chrono::Utc::now().timestamp() - t)
            .unwrap_or(i64::MAX);
        entity.attrs["stale"] =
            json!(!reachable || age > 180 || entity.attrs["probe_error"].is_string());
        records.push(entity.attrs);
    }
    Ok(serde_json::to_string_pretty(&records)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planning_reprobes_only_devices_without_a_fresh_clean_inventory() {
        let agent = crate::agent::MasterAgent::new(
            crate::llm::LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            crate::workers::WorkerPool::new(
                ["fresh", "old", "failed", "never"]
                    .iter()
                    .map(|name| hive_common::protocol::WorkerInfo {
                        name: (*name).into(),
                        host: (*name).into(),
                        user: "test".into(),
                        port: None,
                        tags: vec![],
                        local: false,
                        container: None,
                    })
                    .collect(),
            ),
            crate::skills::SkillRegistry::new(),
            crate::memory::MemorySystem::new(),
        )
        .with_master_name("coordinator");
        let graph = &agent.memory.graph;
        for name in ["fresh", "old", "failed", "never", "coordinator"] {
            crate::memory::machines::project_into_graph(
                graph,
                &crate::memory::machines::MachineFacts { name: name.into(), reachable: true, ..Default::default() },
            )
            .unwrap();
        }
        let now = 10_000;
        for (device, age) in [("fresh", 30), ("old", 500), ("failed", 30), ("coordinator", 30)] {
            project(graph, device, &[json!({"agent":"codex","executable":"/codex","verified_at":now-age})]).unwrap();
        }
        mark_stale(graph, "failed", "timed out").unwrap();
        let names: Vec<_> = stale_devices(&agent, 120, now).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["old", "failed", "never"]);
    }
    #[test]
    fn all_devices_survive_offline_with_evidence_distinct_from_installation() {
        let graph = KnowledgeGraph::in_memory().unwrap();
        for device in ["mac-air", "archlinux-worker", "cis-a6000", "cis-linux2"] {
            let facts = crate::memory::machines::MachineFacts {
                name: device.into(),
                reachable: true,
                ..Default::default()
            };
            crate::memory::machines::project_into_graph(&graph, &facts).unwrap();
            project(&graph,device,&[json!({"agent":"claude","executable":"/claude","verified_at":chrono::Utc::now().timestamp(),"models":[],"invocation":null})]).unwrap();
        }
        crate::memory::machines::project_into_graph(
            &graph,
            &crate::memory::machines::MachineFacts {
                name: "mac-air".into(),
                reachable: false,
                ..Default::default()
            },
        )
        .unwrap();
        let records: Vec<Value> = serde_json::from_str(&describe(&graph).unwrap()).unwrap();
        assert_eq!(records.len(), 4);
        let air = records.iter().find(|r| r["device"] == "mac-air").unwrap();
        assert_eq!(air["stale"], true);
        assert_eq!(air["executable"], "/claude");
        assert!(air["invocation"].is_null());
    }

    #[test]
    fn opencode_connected_models_and_configured_default_are_persisted_and_reported() {
        let graph = KnowledgeGraph::in_memory().unwrap();
        crate::memory::machines::project_into_graph(
            &graph,
            &crate::memory::machines::MachineFacts {
                name: "air".into(),
                reachable: true,
                ..Default::default()
            },
        )
        .unwrap();
        project(&graph, "air", &[json!({"agent":"opencode","executable":"/opencode","verified_at":chrono::Utc::now().timestamp(),
            "models":["alibaba/qwq-plus","zai-coding-plan/glm-5.3"],"default_model":"zai-coding-plan/glm-5.3"})]).unwrap();
        let records: Vec<Value> = serde_json::from_str(&describe(&graph).unwrap()).unwrap();
        let record = records.iter().find(|r| r["agent"] == "opencode").unwrap();
        assert_eq!(record["models"], json!(["alibaba/qwq-plus","zai-coding-plan/glm-5.3"]));
        assert_eq!(record["default_model"], "zai-coding-plan/glm-5.3");
        // A re-probe that could not query the registry keeps the last evidence.
        project(&graph, "air", &[json!({"agent":"opencode","executable":"/opencode","verified_at":chrono::Utc::now().timestamp(),"models":[]})]).unwrap();
        let records: Vec<Value> = serde_json::from_str(&describe(&graph).unwrap()).unwrap();
        assert_eq!(records[0]["models"], json!(["alibaba/qwq-plus","zai-coding-plan/glm-5.3"]));
    }

    #[test]
    fn stale_inventory_and_invocation_evidence_survive_restart_and_refresh() {
        let path = std::env::temp_dir().join(format!("hive-inventory-{}.db", uuid::Uuid::new_v4()));
        let graph = KnowledgeGraph::open(&path).unwrap();
        crate::memory::machines::project_into_graph(
            &graph,
            &crate::memory::machines::MachineFacts {
                name: "cis-a6000".into(),
                reachable: true,
                ..Default::default()
            },
        )
        .unwrap();
        let evidence = json!({"model":"verified-model","run_id":"successful-run"});
        project(&graph, "cis-a6000", &[json!({"agent":"claude","executable":"/claude","verified_at":chrono::Utc::now().timestamp(),"models":["verified-model"],"invocation":evidence})]).unwrap();
        mark_stale(&graph, "cis-a6000", "SSH disconnected").unwrap();
        drop(graph);
        let graph = KnowledgeGraph::open(&path).unwrap();
        let records: Vec<Value> = serde_json::from_str(&describe(&graph).unwrap()).unwrap();
        assert_eq!(records[0]["stale"], true);
        assert_eq!(records[0]["probe_error"], "SSH disconnected");
        assert_eq!(records[0]["invocation"], evidence);
        assert_eq!(records[0]["models"], json!(["verified-model"]));
        project(&graph, "cis-a6000", &[json!({"agent":"claude","executable":"/new/claude","verified_at":chrono::Utc::now().timestamp(),"models":[],"invocation":null})]).unwrap();
        let records: Vec<Value> = serde_json::from_str(&describe(&graph).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["stale"], false);
        assert!(records[0]["probe_error"].is_null());
        assert_eq!(records[0]["executable"], "/new/claude");
        assert_eq!(records[0]["invocation"], evidence);
        assert_eq!(records[0]["models"], json!(["verified-model"]));
        project(
            &graph,
            "cis-a6000",
            &[json!({"agent":"claude","verified_at":chrono::Utc::now().timestamp()-181})],
        )
        .unwrap();
        let records: Vec<Value> = serde_json::from_str(&describe(&graph).unwrap()).unwrap();
        assert_eq!(records[0]["stale"], true);
        drop(graph);
        std::fs::remove_file(path).unwrap();
    }
}
