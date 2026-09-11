//! Durable fleet delegation. Remote journals own native conversations; the
//! coordinator synchronizes evidence and never retries uncertain launches.
pub mod inventory;
pub mod review;
pub mod store;
pub mod transport;

use crate::{agent::MasterAgent, memory::graph::entity_id};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub key: String,
    pub device: String,
    pub agent: String,
    pub model: Option<String>,
    pub workspace: String,
    pub objective: String,
    pub dependencies: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationPlan {
    pub summary: String,
    pub assignments: Vec<Assignment>,
}

pub fn enabled() -> bool {
    std::env::var("HIVE_DELEGATION").as_deref() == Ok("1")
}

pub fn validate(plan: &DelegationPlan, agent: &MasterAgent) -> anyhow::Result<()> {
    anyhow::ensure!(
        plan.assignments.len() <= 16,
        "At most 16 assignments per task"
    );
    let mut keys = std::collections::HashSet::new();
    for a in &plan.assignments {
        anyhow::ensure!(
            !a.key.is_empty() && keys.insert(a.key.clone()),
            "Assignment keys must be unique"
        );
        anyhow::ensure!(
            agent
                .workers
                .workers
                .iter()
                .any(|w| w.info.name == a.device),
            "Unknown device: {}",
            a.device
        );
        anyhow::ensure!(
            ["claude", "codex", "agy", "opencode"].contains(&a.agent.as_str()),
            "Unknown agent: {}",
            a.agent
        );
        anyhow::ensure!(
            a.workspace.starts_with("~/hive-workspaces/")
                && a.workspace.len() > 18
                && !a
                    .workspace
                    .split('/')
                    .any(|c| c == ".." || c == "." || c.is_empty())
                && !a.workspace.contains(['\n', '\r', '\0']),
            "Workspace must be a child of ~/hive-workspaces"
        );
        anyhow::ensure!(
            !a.objective.trim().is_empty() && !a.acceptance_criteria.is_empty(),
            "Objective and acceptance criteria required"
        );
        let machine = agent
            .memory
            .graph
            .entity(&entity_id("machine", &a.device))?
            .ok_or_else(|| anyhow::anyhow!("No inventory for {}", a.device))?;
        let tags = machine.attrs["tags"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // Existing placement restrictions stay deterministic. Heavy work cannot
        // be assigned to laptop/login nodes or bypass a scheduler.
        if a.required_capabilities
            .iter()
            .any(|c| c == "gpu-compute" || c == "heavy-compute")
        {
            anyhow::ensure!(
                !tags.iter().any(|t| ["light", "login-node", "slurm"]
                    .iter()
                    .any(|s| t.as_str() == Some(s))),
                "{} requires light work or a scheduler allocation; direct heavy placement rejected",
                a.device
            );
        }
        for cap in &a.required_capabilities {
            if cap == "heavy-compute" {
                continue;
            }
            anyhow::ensure!(
                agent
                    .memory
                    .graph
                    .neighbors(&machine.id, "has_capability")?
                    .iter()
                    .any(|c| &c.name == cap),
                "{} lacks capability {}",
                a.device,
                cap
            );
        }
    }
    // A topological walk detects unknown dependencies, self edges and cycles.
    let mut done = std::collections::HashSet::new();
    loop {
        let before = done.len();
        for a in &plan.assignments {
            if a.dependencies.iter().all(|d| done.contains(d)) {
                done.insert(a.key.clone());
            }
        }
        if done.len() == plan.assignments.len() {
            break;
        }
        anyhow::ensure!(
            done.len() > before,
            "Unknown or cyclic assignment dependencies"
        );
    }
    Ok(())
}

/// Canonical device/agent pairs named by the user are hard constraints, even
/// when a generated plan proposes an otherwise valid different placement.
pub fn validate_explicit(
    request: &str,
    plan: &DelegationPlan,
    agent: &MasterAgent,
) -> anyhow::Result<()> {
    for worker in &agent.workers.workers {
        let pattern = format!(
            r"(?i)\b(claude|codex|agy|opencode)\s+on\s+{}(?:$|[^[:alnum:]_.-])",
            regex::escape(&worker.info.name)
        );
        for captures in regex::Regex::new(&pattern)?.captures_iter(request) {
            let selected = captures[1].to_ascii_lowercase();
            anyhow::ensure!(
                plan.assignments
                    .iter()
                    .any(|a| a.device == worker.info.name && a.agent == selected),
                "Explicit placement requires {} on {}",
                selected,
                worker.info.name
            );
        }
    }
    Ok(())
}

pub async fn plan(
    agent: &MasterAgent,
    request: &str,
    history: &str,
) -> anyhow::Result<DelegationPlan> {
    agent.refresh_machine_graph().await?;
    inventory::refresh(agent).await?;
    let fleet = crate::memory::machines::describe_for_prompt(&agent.memory.graph)?;
    let agents = inventory::describe(&agent.memory.graph)?;
    let mut prompt = format!("You are Hive's coordinator. Plan work for real agent conversations on configured devices. \
        Return structured assignments, never shell commands or file contents. The complete fleet is below. \
        Select device, installed agent and available model automatically; explicit user device/agent/model choices take precedence. \
        Use null model when no model identifiers were verified; native default is resolved before execution. \
        Missing authentication, runtime or software is reported by Hive on that exact device; do not silently substitute explicit choices. \
        Prefer dedicated devices for ordinary work. Laptops/light hosts and login nodes only receive short light work. \
        GPU/shared scheduler work requires a scheduler allocation; never launch sustained work directly on login nodes. \
        Ordinary CLI coding tasks need required_capabilities=[]: Claude/Codex provider inference does NOT require local-inference on the worker. \
        Only require GPU or heavy-compute when the user explicitly needs that capability. \
        Every workspace is a fresh unique child of ~/hive-workspaces/. Each assignment has a unique key. \
        dependencies are assignment keys that must complete before this starts; peers that must negotiate concurrently have no dependency on each other. \
        Acceptance criteria must require implementation, independent verification, deployment evidence when requested and peer agreement. \
        For questions that need no work, answer in summary and use an empty assignments list.\n\
        Fleet:\n{fleet}\nAgent inventory (installation, authentication, runtime, models and invocation evidence are distinct):\n{agents}\n\
        Prior conversation (context only):\n{history}\nUser request:\n{request}");
    let schema = json!({"type":"object","additionalProperties":false,"required":["summary","assignments"],"properties":{
    "summary":{"type":"string"},"assignments":{"type":"array","items":{"type":"object","additionalProperties":false,
    "required":["key","device","agent","model","workspace","objective","dependencies","acceptance_criteria","required_capabilities"],"properties":{
        "key":{"type":"string"},"device":{"type":"string"},"agent":{"enum":["claude","codex","agy","opencode"]},
        "model":{"type":["string","null"]},"workspace":{"type":"string"},"objective":{"type":"string"},
        "dependencies":{"type":"array","items":{"type":"string"}},"acceptance_criteria":{"type":"array","items":{"type":"string"}},
        "required_capabilities":{"type":"array","items":{"type":"string"}}
    }}}}});
    for attempt in 0..2 {
        let response = agent
            .llm
            .complete_json_with(&prompt, hive_common::AiProvider::Local, &schema)
            .await?;
        let parsed = serde_json::from_str::<DelegationPlan>(&response.text)
            .map_err(anyhow::Error::from)
            .and_then(|p| {
                validate(&p, agent)?;
                validate_explicit(request, &p, agent)?;
                Ok(p)
            });
        match parsed {
            Ok(plan) => return Ok(plan),
            Err(error) if attempt == 0 => prompt.push_str(&format!("\nThe previous proposed plan was rejected before execution: {error}. Correct that error and return the complete plan. Keep explicit user placements.")),
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

pub fn setup_reason(
    agent: &MasterAgent,
    assignment: &Assignment,
) -> anyhow::Result<Option<String>> {
    let device = &assignment.device;
    let machine = agent.memory.graph.entity(&entity_id("machine", device))?;
    if machine
        .as_ref()
        .and_then(|m| m.attrs["reachable"].as_bool())
        != Some(true)
    {
        return Ok(Some(format!(
            "{device} is offline; retained inventory is stale"
        )));
    }
    let record = agent.memory.graph.entity(&entity_id(
        "device-agent",
        &format!("{device}/{}", assignment.agent),
    ))?;
    let Some(record) = record else {
        return Ok(Some(format!(
            "{device}: {} inventory missing",
            assignment.agent
        )));
    };
    let a = record.attrs;
    let reason = if !a["executable"].is_string() {
        Some("software is missing")
    } else if a["runtime_ready"] != true {
        Some("runner runtime or permission controls need setup")
    } else if a["authentication"] == "login-required" {
        Some("login is required")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Ok(Some(format!("{device}: {} {reason}", assignment.agent)));
    }
    if let Some(model) = &assignment.model {
        if !a["models"]
            .as_array()
            .is_some_and(|models| models.iter().any(|m| m == model))
        {
            return Ok(Some(format!(
                "{device}: {} model {model} has not been verified available",
                assignment.agent
            )));
        }
    }
    Ok(None)
}

pub fn remote_assignment(
    run: &store::Run,
    peers: &[store::Run],
    executable: Option<&str>,
) -> Value {
    let mut value = serde_json::to_value(&run.assignment).expect("serializable assignment");
    value["id"] = json!(run.id);
    value["task_id"] = json!(run.task_id);
    value["executable"] = json!(executable);
    value["peers"] = json!(peers.iter().filter(|p| p.id != run.id).map(|p| json!({"id":p.id,"key":p.assignment.key,"device":p.assignment.device,"agent":p.assignment.agent})).collect::<Vec<_>>());
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    fn agent() -> MasterAgent {
        let agent = MasterAgent::new(
            crate::llm::LlmRouter::new("http://127.0.0.1:1".into(), "test".into()),
            crate::workers::WorkerPool::new(vec![hive_common::protocol::WorkerInfo {
                name: "air".into(),
                host: "ssh-alias".into(),
                user: "test".into(),
                port: None,
                tags: vec!["light".into()],
            }]),
            crate::skills::SkillRegistry::new(),
            crate::memory::MemorySystem::new(),
        );
        crate::memory::machines::project_into_graph(
            &agent.memory.graph,
            &crate::memory::machines::MachineFacts {
                name: "air".into(),
                reachable: true,
                tags: vec!["light".into()],
                ..Default::default()
            },
        )
        .unwrap();
        agent
    }
    fn plan() -> DelegationPlan {
        serde_json::from_value(json!({"summary":"work","assignments":[{"key":"a","device":"air","agent":"claude","model":null,"workspace":"~/hive-workspaces/test","objective":"implement","dependencies":[],"acceptance_criteria":["verified"]}]})).unwrap()
    }
    #[test]
    fn validates_placement_dependencies_and_workspaces() {
        let agent = agent();
        let mut p = plan();
        assert!(validate(&p, &agent).is_ok());
        p.assignments[0].device = "ssh-alias".into();
        assert!(validate(&p, &agent).is_err());
        p.assignments[0].device = "air".into();
        p.assignments[0].dependencies = vec!["a".into()];
        assert!(validate(&p, &agent).is_err());
        p.assignments[0].dependencies.clear();
        p.assignments[0].workspace = "~/hive-workspaces/../secret".into();
        assert!(validate(&p, &agent).is_err());
        p.assignments[0].workspace = "~/hive-workspaces/test".into();
        p.assignments[0].required_capabilities = vec!["heavy-compute".into()];
        assert!(validate(&p, &agent).is_err());
    }
    #[test]
    fn explicit_agent_device_choices_override_automatic_placement() {
        let agent = agent();
        let p = plan();
        assert!(validate_explicit("Choose the best worker automatically", &p, &agent).is_ok());
        assert!(validate_explicit("Use CLAUDE on AIR, please", &p, &agent).is_ok());
        assert!(validate_explicit("Use Codex on air", &p, &agent).is_err());
        assert!(validate_explicit("Use Codex on air and Claude on air", &p, &agent).is_err());
        assert!(validate_explicit("Use Codex on air-worker", &p, &agent).is_ok());
        assert!(validate_explicit("Use Codex on air.example", &p, &agent).is_ok());
        assert!(validate_explicit("Use Codex on air_backup", &p, &agent).is_ok());
        let mut changed = p;
        changed.assignments[0].agent = "codex".into();
        assert!(validate_explicit("Use Codex on air", &changed, &agent).is_ok());
        assert!(validate_explicit("Use Claude on air", &changed, &agent).is_err());
    }
    #[test]
    fn missing_agent_and_unverified_model_report_exact_device() {
        let agent = agent();
        let mut p = plan();
        assert!(setup_reason(&agent, &p.assignments[0])
            .unwrap()
            .unwrap()
            .contains("air"));
        inventory::project(&agent.memory.graph,"air",&[json!({"agent":"claude","executable":"/claude","runtime_ready":true,"authentication":"authenticated","models":["available"]})]).unwrap();
        assert!(setup_reason(&agent, &p.assignments[0]).unwrap().is_none());
        p.assignments[0].model = Some("unavailable".into());
        assert!(setup_reason(&agent, &p.assignments[0])
            .unwrap()
            .unwrap()
            .contains("unavailable"));
    }
}
