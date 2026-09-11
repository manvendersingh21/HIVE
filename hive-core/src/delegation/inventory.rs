use super::transport;
use crate::{
    agent::MasterAgent,
    memory::graph::{entity_id, Entity, KnowledgeGraph},
};
use serde_json::{json, Value};

pub async fn refresh(agent: &MasterAgent) -> anyhow::Result<()> {
    let probes = agent.workers.workers.iter().map(|worker| async move {
        // Probe existing bundle when present (SDK/runtime status is device-specific).
        let source = format!(
            "__file__ = str(__import__('pathlib').Path.home()/'.hive/runners/{}/runner.py')\n{}",
            transport::bundle_name(),
            transport::RUNNER
        );
        let command = format!("python3 -c {} probe", transport::quote(&source));
        (
            worker.info.name.clone(),
            transport::ssh(&worker.info, &command, None).await,
        )
    });
    for (device, result) in futures::future::join_all(probes).await {
        match result {
            Ok(raw) => match serde_json::from_str::<Vec<Value>>(&raw) {
                Ok(records) => project(&agent.memory.graph, &device, &records)?,
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
