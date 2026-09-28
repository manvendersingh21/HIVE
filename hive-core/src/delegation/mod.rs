//! Durable fleet delegation. Remote journals own native conversations; the
//! coordinator synchronizes evidence and never retries uncertain launches.
pub mod containers;
pub mod inventory;
pub mod review;
pub mod store;
pub mod transport;

use crate::{agent::MasterAgent, memory::graph::entity_id};
use hive_common::protocol::WorkerInfo;
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
    /// Containers Hive creates before any assignment starts. Only the planner
    /// proposes them, and only when the user asked; worker agents never can.
    #[serde(default)]
    pub containers: Vec<NewContainer>,
}

/// A container the planner wants. Hive chooses the image, mounts and flags;
/// the model names only the new device and the machine that runs it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NewContainer {
    pub name: String,
    pub host: String,
}

/// At most this many new containers per request.
pub const MAX_NEW_CONTAINERS: usize = 2;

pub fn enabled() -> bool {
    std::env::var("HIVE_DELEGATION").as_deref() == Ok("1")
}

fn valid_workspace(workspace: &str) -> bool {
    workspace.starts_with("~/hive-workspaces/")
        && workspace.len() > 18
        && !workspace
            .split('/')
            .any(|c| c == ".." || c == "." || c.is_empty())
        && !workspace.contains(['\n', '\r', '\0'])
}

/// Workspaces are Hive bookkeeping, not a user choice: a planner that puts one
/// elsewhere (e.g. "in a temporary directory") gets a fresh generated child of
/// ~/hive-workspaces instead of failing the plan. validate() still checks it.
fn repair_workspaces(plan: &mut DelegationPlan) {
    for a in &mut plan.assignments {
        if !valid_workspace(&a.workspace) {
            let key: String = a
                .key
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .take(48)
                .collect();
            let id = uuid::Uuid::new_v4().simple().to_string();
            a.workspace = format!("~/hive-workspaces/{}-{}", &id[..8], key);
        }
    }
}

/// Where delegated work for `device` runs. A configured SSH worker wins; the
/// coordinator's own name runs agents here without SSH; a registered
/// container runs them inside Docker on its machine.
pub fn target(agent: &MasterAgent, device: &str) -> Option<WorkerInfo> {
    resolve(agent, device, &containers::load())
}

fn resolve(agent: &MasterAgent, device: &str, registered: &[containers::Container]) -> Option<WorkerInfo> {
    machine(agent, device).or_else(|| {
        registered
            .iter()
            .find(|c| c.name == device)
            .and_then(|c| in_container(agent, c))
    })
}

/// A fleet machine or the coordinator, never a container.
pub fn machine(agent: &MasterAgent, device: &str) -> Option<WorkerInfo> {
    if let Some(worker) = agent.workers.find(device) {
        return Some(worker.info);
    }
    (device == agent.master_name()).then(|| WorkerInfo {
        name: device.to_string(),
        host: "localhost".into(),
        user: std::env::var("USER").unwrap_or_default(),
        port: None,
        tags: vec![],
        local: true,
        container: None,
    })
}

/// The container's machine, addressed as the container. `local` comes from
/// the machine, never from the registry file.
fn in_container(agent: &MasterAgent, c: &containers::Container) -> Option<WorkerInfo> {
    let mut info = machine(agent, &c.host)?;
    info.name = c.name.clone();
    info.container = Some(c.container.clone());
    Some(info)
}

/// Every device delegation can reach: the SSH fleet, the coordinator and the
/// registered containers whose machine is still configured.
pub fn targets(agent: &MasterAgent) -> Vec<WorkerInfo> {
    let mut all: Vec<WorkerInfo> = agent.workers.snapshot().into_iter().map(|w| w.info).collect();
    all.extend(machine(agent, agent.master_name()).filter(|t| t.local));
    all.extend(containers::load().iter().filter_map(|c| in_container(agent, c)));
    all
}

/// New containers must be few, well named, new, and hosted on a real
/// machine (never inside another container).
fn validate_new_containers(plan: &DelegationPlan, agent: &MasterAgent) -> anyhow::Result<()> {
    anyhow::ensure!(
        plan.containers.len() <= MAX_NEW_CONTAINERS,
        "At most {MAX_NEW_CONTAINERS} new containers per request"
    );
    let existing: Vec<String> = targets(agent).into_iter().map(|t| t.name).collect();
    let mut names = std::collections::HashSet::new();
    for c in &plan.containers {
        anyhow::ensure!(
            containers::valid_name(&c.name),
            "Invalid container name {:?}: use letters, numbers, '-', '_' or '.'",
            c.name
        );
        anyhow::ensure!(names.insert(c.name.as_str()), "Container {} is planned twice", c.name);
        anyhow::ensure!(
            !existing.contains(&c.name),
            "{} already exists; use it instead of creating a container",
            c.name
        );
        anyhow::ensure!(
            machine(agent, &c.host).is_some(),
            "Container {} must be hosted on a fleet machine or the coordinator, not {}",
            c.name,
            c.host
        );
    }
    Ok(())
}

/// Model ids from OpenCode's qwq/qvq reasoning families answer prompts but
/// never call tools; a turn run on one can end with no actions at all.
fn reasoning_family(model: &str) -> bool {
    let id = model.to_ascii_lowercase();
    id.contains("qwq") || id.contains("qvq")
}

/// The verified models an OpenCode install on `device` offers: the probe's
/// connected catalog plus any model with real invocation evidence.
fn opencode_models(agent: &MasterAgent, device: &str) -> anyhow::Result<Vec<String>> {
    let Some(record) = agent
        .memory
        .graph
        .entity(&entity_id("device-agent", &format!("{device}/opencode")))?
    else {
        return Ok(Vec::new());
    };
    let mut models: Vec<String> = record.attrs["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if let Some(invoked) = record.attrs["invocation"]["model"].as_str() {
        if !models.iter().any(|m| m == invoked) {
            models.push(invoked.to_string());
        }
    }
    Ok(models)
}

/// A verified tool-capable OpenCode model for `device`, preferring the model
/// OpenCode itself is configured to default to, never a reasoning family.
fn select_opencode_model(agent: &MasterAgent, device: &str) -> anyhow::Result<Option<String>> {
    let models = opencode_models(agent, device)?;
    let default = agent
        .memory
        .graph
        .entity(&entity_id("device-agent", &format!("{device}/opencode")))?
        .and_then(|r| r.attrs["default_model"].as_str().map(str::to_string));
    if let Some(default) = default {
        if models.contains(&default) && !reasoning_family(&default) {
            return Ok(Some(default));
        }
    }
    Ok(models.into_iter().find(|m| !reasoning_family(m)))
}

/// OpenCode's own default can be a reasoning model that never calls tools,
/// so a null model is filled with a verified tool-capable model before the
/// plan is validated. Null for other agents stays a native-default choice.
fn repair_models(plan: &mut DelegationPlan, agent: &MasterAgent) -> anyhow::Result<()> {
    for a in &mut plan.assignments {
        if a.agent == "opencode" && a.model.is_none() {
            a.model = select_opencode_model(agent, &a.device)?;
        }
    }
    Ok(())
}

pub fn validate(plan: &DelegationPlan, agent: &MasterAgent) -> anyhow::Result<()> {
    anyhow::ensure!(
        plan.assignments.len() <= 16,
        "At most 16 assignments per task"
    );
    validate_new_containers(plan, agent)?;
    // Assignments may target a container this plan creates. It has no
    // inventory yet; its setup is checked once it exists.
    let planned: Vec<&str> = plan.containers.iter().map(|c| c.name.as_str()).collect();
    let mut keys = std::collections::HashSet::new();
    for a in &plan.assignments {
        anyhow::ensure!(
            !a.key.is_empty() && keys.insert(a.key.clone()),
            "Assignment keys must be unique"
        );
        let new_container = planned.contains(&a.device.as_str());
        if !new_container && target(agent, &a.device).is_none() {
            anyhow::bail!("Unknown device: {}", a.device);
        }
        anyhow::ensure!(
            ["claude", "codex", "agy", "opencode"].contains(&a.agent.as_str()),
            "Unknown agent: {}",
            a.agent
        );
        if a.agent == "opencode" {
            if let Some(model) = &a.model {
                anyhow::ensure!(
                    !reasoning_family(model),
                    "{}: OpenCode model {model} is from a qwq/qvq reasoning family that never calls tools",
                    a.device
                );
            }
        }
        anyhow::ensure!(
            valid_workspace(&a.workspace),
            "Workspace must be a child of ~/hive-workspaces"
        );
        anyhow::ensure!(
            !a.objective.trim().is_empty() && !a.acceptance_criteria.is_empty(),
            "Objective and acceptance criteria required"
        );
        if new_container {
            anyhow::ensure!(
                !a.required_capabilities.iter().any(|c| c == "gpu-compute" || c == "heavy-compute"),
                "{} is a new container; it can't be promised GPU or heavy compute",
                a.device
            );
            continue;
        }
        if a.agent == "opencode" && a.model.is_none() {
            anyhow::bail!(
                "{}: no verified OpenCode model is available; connect a model provider on that device, name a verified model explicitly, or use another agent",
                a.device
            );
        }
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

/// Canonical device/agent pairs (and models) named by the user are hard
/// constraints, even when a generated plan proposes an otherwise valid
/// different placement. "exactly N assignments" also bounds the plan, and with
/// named placements every assignment must be one of them.
/// Whether `request` names the coordinator: its full name or any run of two
/// or more of its name's parts, so "mac mini" or "macmini" name
/// "manus-mac-mini".
pub fn names_coordinator(request: &str, master: &str) -> bool {
    let parts: Vec<String> = master
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .map(regex::escape)
        .collect();
    let min = parts.len().min(2).max(1);
    (min..=parts.len()).any(|len| {
        parts.windows(len).any(|w| {
            regex::Regex::new(&format!(r"(?i)\b{}\b", w.join(r"[\s._-]*")))
                .is_ok_and(|r| r.is_match(request))
        })
    })
}

/// Whether the request asks for a container: creating one is never the
/// planner's own idea.
pub fn asks_for_container(request: &str) -> bool {
    regex::Regex::new(r"(?i)\b(containers?|sandbox(es)?|docker)\b")
        .is_ok_and(|r| r.is_match(request))
}

fn validate_container_request(request: &str, plan: &DelegationPlan) -> anyhow::Result<()> {
    anyhow::ensure!(
        plan.containers.is_empty() || asks_for_container(request),
        "The user did not ask for a new container; use existing devices and return an empty containers list"
    );
    Ok(())
}

/// Agents on the coordinator sit next to Hive's own config and database, so
/// work goes there only when the user asks for that machine by name. A
/// container on the coordinator is walled off from both and needs no ask.
fn validate_coordinator(request: &str, plan: &DelegationPlan, agent: &MasterAgent) -> anyhow::Result<()> {
    for a in &plan.assignments {
        if target(agent, &a.device).is_some_and(|t| t.local && t.container.is_none()) {
            anyhow::ensure!(
                names_coordinator(request, agent.master_name()),
                "{} is the Hive coordinator; place work there only when the user names it",
                a.device
            );
        }
    }
    Ok(())
}

pub fn validate_explicit(
    request: &str,
    plan: &DelegationPlan,
    agent: &MasterAgent,
) -> anyhow::Result<()> {
    let mut placements = Vec::new();
    for worker in &targets(agent) {
        let pattern = format!(
            r"(?i)\b(claude|codex|agy|opencode)(?:\s+agent)?\s+on\s+{}(?:\s+(?:using|with)\s+(?:the\s+)?model\s+([[:alnum:]][[:alnum:]_.:/@\[\]-]*[[:alnum:]_\]])|(?:$|\.(?:\s|$)|[^[:alnum:]_.-]))",
            regex::escape(&worker.name)
        );
        for captures in regex::Regex::new(&pattern)?.captures_iter(request) {
            let selected = captures[1].to_ascii_lowercase();
            let model = captures.get(2).map(|m| m.as_str().to_string());
            let matches = |a: &Assignment| {
                a.device == worker.name
                    && a.agent == selected
                    && model.as_ref().is_none_or(|m| a.model.as_ref() == Some(m))
            };
            anyhow::ensure!(
                plan.assignments.iter().any(matches),
                "Explicit placement requires {} on {}{}",
                selected,
                worker.name,
                model
                    .as_ref()
                    .map(|m| format!(" using model {m}"))
                    .unwrap_or_default()
            );
            placements.push((worker.name.clone(), selected, model));
        }
    }
    let count = regex::Regex::new(
        r"(?i)\bexactly\s+(\d+|one|two|three|four|five|six|seven|eight)\s+(?:[[:alpha:]-]+\s+)?assignments?\b",
    )?;
    if let Some(captures) = count.captures(request) {
        let words = [
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ];
        let word = captures[1].to_ascii_lowercase();
        let expected = match words.iter().position(|w| *w == word) {
            Some(i) => i + 1,
            None => word.parse()?,
        };
        anyhow::ensure!(
            plan.assignments.len() == expected,
            "Request requires exactly {expected} assignments, plan has {}",
            plan.assignments.len()
        );
        if !placements.is_empty() {
            for a in &plan.assignments {
                anyhow::ensure!(
                    placements.iter().any(|(device, agent, model)| {
                        &a.device == device
                            && &a.agent == agent
                            && model.as_ref().is_none_or(|m| a.model.as_ref() == Some(m))
                    }),
                    "Assignment {} ({} on {}) was not requested",
                    a.key,
                    a.agent,
                    a.device
                );
            }
        }
    }
    Ok(())
}

/// Tells the planner the coordinator's own hostname — which it sees in the
/// fleet — is not a delegation target unless it is also an SSH worker.
fn coordinator_note(agent: &MasterAgent) -> String {
    if agent.workers.find(agent.master_name()).is_some() {
        return String::new();
    }
    format!("{} is this Hive coordinator (the machine the user is talking to). Its agents run locally, and its inventory is listed like any device. \
        Assign work to it only when the user names it (a shortened name like \"mac mini\" counts); otherwise prefer the other devices.\n",
        agent.master_name())
}

pub async fn plan(
    agent: &MasterAgent,
    request: &str,
    history: &str,
) -> anyhow::Result<DelegationPlan> {
    agent.refresh_machine_graph().await?;
    inventory::refresh_stale(agent, 120, 45).await?;
    let fleet = crate::memory::machines::describe_for_prompt(&agent.memory.graph)?;
    let agents = inventory::describe(&agent.memory.graph)?;
    let coordinator = coordinator_note(agent);
    let mut prompt = format!("You are Hive's coordinator. Plan work for real agent conversations on configured devices. \
        Return structured assignments, never shell commands or file contents. The complete fleet is below. \
        Select device, installed agent and available model automatically; explicit user device/agent/model choices take precedence. \
        For opencode, set model to a verified id from that device's agent inventory (provider/model ids) and never a qwq/qvq reasoning model: those never call tools, so a turn can end with no actions. \
        A null opencode model is filled from the device's verified models (the configured default first); a plan with no verified opencode model is rejected instead of guessing. \
        For other agents use null model when no model identifiers were verified; native default is resolved before execution. \
        Missing authentication, runtime or software is reported by Hive on that exact device; do not silently substitute explicit choices. \
        Prefer dedicated devices for ordinary work. Laptops/light hosts and login nodes only receive short light work. \
        GPU/shared scheduler work requires a scheduler allocation; never launch sustained work directly on login nodes. \
        Ordinary CLI coding tasks need required_capabilities=[]: Claude/Codex provider inference does NOT require local-inference on the worker. \
        Only require GPU or heavy-compute when the user explicitly needs that capability. \
        Every workspace is a fresh unique child of ~/hive-workspaces/. Each assignment has a unique key. \
        dependencies are assignment keys that must complete before this starts; peers that must negotiate concurrently have no dependency on each other. \
        Acceptance criteria must require implementation, independent verification, deployment evidence when requested and peer agreement. \
        For questions that need no work, answer in summary and use an empty assignments list. \
        containers: leave it empty unless the user explicitly asks for a new container or sandbox; existing containers are already in the fleet with a container tag, so reuse them. \
        When asked, list at most {MAX_NEW_CONTAINERS} new containers as {{name, host}}: host is a fleet machine or the coordinator (never a container), and name becomes a new device that assignments in this plan may use. \
        Hive creates them with its own image and the host's agent logins before any assignment starts; you never choose images, mounts or flags.\n\
        Fleet:\n{fleet}\n{coordinator}Agent inventory (installation, authentication, runtime, models and invocation evidence are distinct):\n{agents}\n\
        Prior conversation (context only):\n{history}\nUser request:\n{request}");
    let schema = json!({"type":"object","additionalProperties":false,"required":["summary","assignments","containers"],"properties":{
    "summary":{"type":"string"},
    "containers":{"type":"array","maxItems":MAX_NEW_CONTAINERS,"items":{"type":"object","additionalProperties":false,
        "required":["name","host"],"properties":{"name":{"type":"string"},"host":{"type":"string"}}}},
    "assignments":{"type":"array","items":{"type":"object","additionalProperties":false,
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
            .and_then(|mut p| {
                repair_workspaces(&mut p);
                repair_models(&mut p, agent)?;
                validate(&p, agent)?;
                validate_explicit(request, &p, agent)?;
                validate_coordinator(request, &p, agent)?;
                validate_container_request(request, &p)?;
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
        // `models` holds the CLI's aliases (e.g. "sonnet"); a model that was
        // actually invoked on this device is verified evidence too.
        let listed = a["models"]
            .as_array()
            .is_some_and(|models| models.iter().any(|m| m == model));
        let invoked = a["invocation"]["model"] == model.as_str();
        if !listed && !invoked {
            return Ok(Some(format!(
                "{device}: {} model {model} has not been verified available",
                assignment.agent
            )));
        }
    }
    if assignment.agent == "opencode"
        && assignment.model.is_none()
        && select_opencode_model(agent, device)?.is_none()
    {
        return Ok(Some(format!(
            "{device}: opencode has no verified tool-capable model; qwq/qvq reasoning models never call tools"
        )));
    }
    Ok(None)
}

/// How much an agent Hive launches may do without asking.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    /// No prompts: every tool call runs and native sandboxes are off.
    #[default]
    Yolo,
    /// Hive's task policy decides; anything else waits for a person.
    Ask,
}

/// Persisted next to the other private coordinator state, never in the repo.
fn autonomy_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".hive/autonomy.json"))
}

/// The configured autonomy. Missing or unreadable state means the default.
pub fn autonomy() -> Autonomy {
    autonomy_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| serde_json::from_value(value["mode"].clone()).ok())
        .unwrap_or_default()
}

pub fn set_autonomy(mode: Autonomy) -> anyhow::Result<()> {
    let path = autonomy_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    std::fs::create_dir_all(path.parent().expect("nested path"))?;
    std::fs::write(path, json!({ "mode": mode }).to_string())?;
    Ok(())
}

pub fn remote_assignment(
    run: &store::Run,
    peers: &[store::Run],
    executable: Option<&str>,
) -> Value {
    let mut value = serde_json::to_value(&run.assignment).expect("serializable assignment");
    value["autonomy"] = json!(autonomy());
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
                local: false,
                container: None,
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
    fn nc(name: &str, host: &str) -> NewContainer {
        NewContainer { name: name.into(), host: host.into() }
    }

    #[test]
    fn plans_parse_with_and_without_containers_and_reject_extra_fields() {
        assert!(plan().containers.is_empty());
        let with: DelegationPlan = serde_json::from_value(json!({"summary":"s","assignments":[],
            "containers":[{"name":"box","host":"air"}]})).unwrap();
        assert_eq!(with.containers, vec![nc("box", "air")]);
        // The model never picks images, mounts or flags.
        assert!(serde_json::from_value::<DelegationPlan>(json!({"summary":"s","assignments":[],
            "containers":[{"name":"box","host":"air","image":"alpine"}]})).is_err());
    }

    #[test]
    fn assignments_may_target_a_container_the_plan_creates() {
        let agent = agent();
        let mut p = plan();
        p.containers = vec![nc("sandbox-1", agent.master_name())];
        p.assignments[0].device = "sandbox-1".into();
        validate(&p, &agent).unwrap();
        // Without the container in the plan the device is unknown.
        p.containers.clear();
        assert_eq!(validate(&p, &agent).unwrap_err().to_string(), "Unknown device: sandbox-1");
        // A brand-new container can't be promised heavy compute.
        p.containers = vec![nc("sandbox-1", "air")];
        p.assignments[0].required_capabilities = vec!["gpu-compute".into()];
        assert!(validate(&p, &agent).unwrap_err().to_string().contains("new container"));
        // Dependency checks still apply.
        p.assignments[0].required_capabilities.clear();
        p.assignments[0].dependencies = vec!["missing".into()];
        assert!(validate(&p, &agent).unwrap_err().to_string().contains("dependencies"));
    }

    #[test]
    fn new_containers_are_few_well_named_new_and_on_real_machines() {
        let agent = agent();
        let check = |containers: Vec<NewContainer>| {
            let mut p = plan();
            p.containers = containers;
            validate(&p, &agent).map_err(|e| e.to_string())
        };
        assert!(check(vec![nc("a", "air"), nc("b", "air")]).is_ok());
        assert!(check(vec![nc("a", "air"), nc("b", "air"), nc("c", "air")])
            .unwrap_err()
            .contains("At most 2"));
        assert!(check(vec![nc("bad name", "air")]).unwrap_err().contains("Invalid container name"));
        assert!(check(vec![nc("a", "air"), nc("a", "air")]).unwrap_err().contains("planned twice"));
        assert!(check(vec![nc("air", "air")]).unwrap_err().contains("already exists"));
        assert!(check(vec![nc("a", "nowhere")]).unwrap_err().contains("fleet machine or the coordinator"));
    }

    #[test]
    fn only_a_request_for_a_container_may_create_one() {
        let mut p = plan();
        assert!(validate_container_request("fix the tests", &p).is_ok());
        p.containers = vec![nc("box", "air")];
        for asked in ["run it in a new container", "use a Sandbox", "spin up docker for this"] {
            assert!(validate_container_request(asked, &p).is_ok(), "{asked}");
        }
        for not_asked in ["fix the tests on air", "contain the blast radius", "sandboxed-ish"] {
            let err = validate_container_request(not_asked, &p).unwrap_err().to_string();
            assert!(err.contains("did not ask for a new container"), "{not_asked}");
        }
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
    fn coordinator(name: &str) -> MasterAgent {
        let agent = agent().with_master_name(name);
        crate::memory::machines::project_into_graph(
            &agent.memory.graph,
            &crate::memory::machines::MachineFacts {
                name: name.into(),
                reachable: true,
                ..Default::default()
            },
        )
        .unwrap();
        agent
    }
    #[test]
    fn containers_resolve_through_their_machine_and_never_shadow_one() {
        let agent = agent();
        let c = |name: &str, host: &str| containers::Container {
            name: name.into(),
            host: host.into(),
            container: format!("{name}-docker"),
            managed: false,
            image: None,
        };
        let registered = [c("box", "air"), c("here", agent.master_name()), c("air", "air"), c("lost", "gone")];
        let on_worker = resolve(&agent, "box", &registered).unwrap();
        assert_eq!((on_worker.host.as_str(), on_worker.local), ("ssh-alias", false));
        assert_eq!(on_worker.container.as_deref(), Some("box-docker"));
        let here = resolve(&agent, "here", &registered).unwrap();
        assert!(here.local && here.container.as_deref() == Some("here-docker"));
        // The fleet machine wins over a same-named container.
        assert_eq!(resolve(&agent, "air", &registered).unwrap().container, None);
        // A container whose machine left the fleet resolves to nothing.
        assert!(resolve(&agent, "lost", &registered).is_none());
    }

    #[test]
    fn the_coordinator_runs_agents_locally_unless_a_worker_has_its_name() {
        let agent = coordinator("mac-mini");
        let local = target(&agent, "mac-mini").unwrap();
        assert!(local.local);
        assert_eq!(local.name, "mac-mini");
        assert!(!target(&agent, "air").unwrap().local);
        assert!(target(&agent, "nowhere").is_none());
        let names: Vec<_> = targets(&agent).into_iter().map(|t| (t.name, t.local)).collect();
        assert_eq!(names, [("air".to_string(), false), ("mac-mini".to_string(), true)]);
        let mut p = plan();
        p.assignments[0].device = "mac-mini".into();
        assert!(validate(&p, &agent).is_ok());
        p.assignments[0].device = "nowhere".into();
        assert_eq!(validate(&p, &agent).unwrap_err().to_string(), "Unknown device: nowhere");
        // A configured worker with the coordinator's name is reached over SSH.
        let both = coordinator("air");
        assert!(!target(&both, "air").unwrap().local);
        assert_eq!(targets(&both).len(), 1);
    }
    #[test]
    fn work_goes_to_the_coordinator_only_when_the_user_names_it() {
        let agent = coordinator("manus-mac-mini");
        let mut p = plan();
        p.assignments[0].device = "manus-mac-mini".into();
        for request in [
            "start a session on mac-mini",
            "use codex on the Mac Mini",
            "run it on macmini",
            "on manus-mac-mini please",
        ] {
            assert!(validate_coordinator(request, &p, &agent).is_ok(), "{request}");
        }
        for request in ["fix the failing tests", "start a session on mac-air", "use my mini"] {
            let err = validate_coordinator(request, &p, &agent).unwrap_err().to_string();
            assert!(err.contains("only when the user names it"), "{request}: {err}");
        }
        // Other devices never need naming.
        p.assignments[0].device = "air".into();
        assert!(validate_coordinator("fix the failing tests", &p, &agent).is_ok());
    }
    #[test]
    fn coordinator_names_match_whole_words_of_two_or_more_parts() {
        assert!(names_coordinator("on mac mini", "manus-mac-mini"));
        assert!(names_coordinator("on MANUS.MAC", "manus-mac-mini"));
        assert!(!names_coordinator("on mac", "manus-mac-mini"));
        assert!(!names_coordinator("on mac-minimal", "manus-mac-mini"));
        assert!(names_coordinator("on master", "master"));
        assert!(!names_coordinator("on masters", "master"));
    }
    #[test]
    fn planner_is_told_the_coordinator_runs_agents_when_named() {
        let note = coordinator_note(&agent().with_master_name("mac-mini"));
        assert!(note.starts_with("mac-mini is this Hive coordinator"), "{note}");
        assert!(note.contains("only when the user names it"), "{note}");
        assert!(coordinator_note(&agent().with_master_name("air")).is_empty());
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
    fn invalid_workspaces_are_replaced_and_valid_ones_kept() {
        let agent = agent();
        for bad in [
            "/tmp/hive-qa",
            "hive-workspaces/x",
            "~/hive-workspaces/../secret",
            "",
            "~/hive-workspaces/",
        ] {
            let mut p = plan();
            p.assignments[0].key = "qa step/1".into();
            p.assignments[0].workspace = bad.into();
            let before = p.assignments[0].clone();
            repair_workspaces(&mut p);
            let a = &p.assignments[0];
            assert!(
                a.workspace.starts_with("~/hive-workspaces/")
                    && a.workspace.ends_with("-qa-step-1"),
                "{bad} -> {}",
                a.workspace
            );
            assert!(validate(&p, &agent).is_ok(), "{bad}");
            assert_eq!(
                (&a.device, &a.agent, &a.model, &a.objective),
                (
                    &before.device,
                    &before.agent,
                    &before.model,
                    &before.objective
                )
            );
        }
        let mut p = plan();
        repair_workspaces(&mut p);
        assert_eq!(p.assignments[0].workspace, "~/hive-workspaces/test");
        let mut twice = plan();
        twice.assignments[0].workspace = "/tmp".into();
        let mut other = twice.clone();
        repair_workspaces(&mut twice);
        repair_workspaces(&mut other);
        assert_ne!(
            twice.assignments[0].workspace,
            other.assignments[0].workspace
        );
    }
    #[test]
    fn explicit_models_and_assignment_counts_are_enforced() {
        let agent = agent();
        let mut p = plan();
        p.assignments[0].agent = "codex".into();
        p.assignments[0].model = Some("gpt-5.6-luna".into());
        let one =
            "Delegate exactly one assignment to the codex agent on air using model gpt-5.6-luna.";
        assert!(validate_explicit(one, &p, &agent).is_ok());
        assert!(validate_explicit("Use codex on air.", &p, &agent).is_ok());
        assert!(validate_explicit("Use claude on air.", &p, &agent).is_err());
        let mut wrong = p.clone();
        wrong.assignments[0].model = Some("gpt-6-astra".into());
        assert!(validate_explicit(one, &wrong, &agent).is_err());
        wrong.assignments[0].model = None;
        assert!(validate_explicit(one, &wrong, &agent).is_err());
        let mut extra = p.clone();
        let mut second = extra.assignments[0].clone();
        second.key = "b".into();
        extra.assignments.push(second.clone());
        assert!(validate_explicit(one, &extra, &agent).is_err());
        assert!(validate_explicit("Do it with exactly 1 assignment", &extra, &agent).is_err());
        assert!(validate_explicit("Pick any workers you like", &extra, &agent).is_ok());
        let two = "Delegate exactly two collaborating assignments: codex on air using model gpt-5.6-luna and claude on air using model haiku";
        extra.assignments[1].agent = "claude".into();
        extra.assignments[1].model = Some("haiku".into());
        assert!(validate_explicit(two, &extra, &agent).is_ok());
        extra.assignments[1].model = Some("sonnet".into());
        assert!(validate_explicit(two, &extra, &agent).is_err());
        extra.assignments[1].model = Some("haiku".into());
        extra.assignments[1].agent = "opencode".into();
        assert!(validate_explicit(two, &extra, &agent).is_err());
        let slash = "Delegate exactly one assignment to the opencode agent on air using model zai-coding-plan/glm-5.3-flash";
        let mut oc = p.clone();
        oc.assignments[0].agent = "opencode".into();
        oc.assignments[0].model = Some("zai-coding-plan/glm-5.3-flash".into());
        assert!(validate_explicit(slash, &oc, &agent).is_ok());
        oc.assignments[0].model = Some("zai-coding-plan/glm-5.2".into());
        assert!(validate_explicit(slash, &oc, &agent).is_err());
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
    #[test]
    fn verified_invocation_model_counts_as_available() {
        let agent = agent();
        let mut p = plan();
        inventory::project(&agent.memory.graph,"air",&[json!({"agent":"claude","executable":"/claude","runtime_ready":true,"authentication":"authenticated","models":["sonnet"],"invocation":{"model":"claude-sonnet-5","verified_at":1}})]).unwrap();
        p.assignments[0].model = Some("claude-sonnet-5".into());
        assert!(setup_reason(&agent, &p.assignments[0]).unwrap().is_none());
        p.assignments[0].model = Some("claude-other".into());
        assert!(setup_reason(&agent, &p.assignments[0])
            .unwrap()
            .unwrap()
            .contains("claude-other"));
    }
    #[test]
    fn opencode_null_model_selects_verified_model_and_skips_qwq_plus() {
        let agent = agent();
        inventory::project(&agent.memory.graph,"air",&[json!({"agent":"opencode","executable":"/opencode","runtime_ready":true,"authentication":"authenticated",
            "models":["alibaba/qwq-plus","alibaba/qwen3.5-plus","zai-coding-plan/glm-5.3"],"default_model":"alibaba/qwq-plus"})]).unwrap();
        let mut p = plan();
        p.assignments[0].agent = "opencode".into();
        repair_models(&mut p, &agent).unwrap();
        // The configured default is a qwq reasoning model; the first coding-capable model wins.
        assert_eq!(p.assignments[0].model.as_deref(), Some("alibaba/qwen3.5-plus"));
        assert!(validate(&p, &agent).is_ok());
        assert!(setup_reason(&agent, &p.assignments[0]).unwrap().is_none());
        // A tool-capable configured default is preferred over other verified models.
        inventory::project(&agent.memory.graph,"air",&[json!({"agent":"opencode","executable":"/opencode","runtime_ready":true,"authentication":"authenticated",
            "models":["alibaba/qwq-plus","zai-coding-plan/glm-5.3"],"default_model":"zai-coding-plan/glm-5.3"})]).unwrap();
        let mut preferred = plan();
        preferred.assignments[0].agent = "opencode".into();
        repair_models(&mut preferred, &agent).unwrap();
        assert_eq!(preferred.assignments[0].model.as_deref(), Some("zai-coding-plan/glm-5.3"));
        // A qwq family id is rejected even when it is named explicitly.
        preferred.assignments[0].model = Some("alibaba/qwq-plus".into());
        let err = validate(&preferred, &agent).unwrap_err().to_string();
        assert!(err.contains("reasoning family"), "{err}");
        assert!(err.contains("qwq"), "{err}");
    }
    #[test]
    fn opencode_plan_without_a_verified_model_is_rejected_with_a_clear_reason() {
        let agent = agent();
        inventory::project(&agent.memory.graph,"air",&[json!({"agent":"opencode","executable":"/opencode","runtime_ready":true,"authentication":"authenticated","models":[],"default_model":null})]).unwrap();
        let mut p = plan();
        p.assignments[0].agent = "opencode".into();
        repair_models(&mut p, &agent).unwrap();
        assert!(p.assignments[0].model.is_none());
        let err = validate(&p, &agent).unwrap_err().to_string();
        assert!(err.contains("no verified OpenCode model"), "{err}");
        // The same gap is reported at launch time on a probed device.
        let reason = setup_reason(&agent, &p.assignments[0]).unwrap().unwrap();
        assert!(reason.contains("no verified tool-capable model"), "{reason}");
        // A model with real invocation evidence counts as verified even with an empty catalog.
        inventory::project(&agent.memory.graph,"air",&[json!({"agent":"opencode","executable":"/opencode","runtime_ready":true,"authentication":"authenticated",
            "models":[],"invocation":{"model":"zai-coding-plan/glm-5.3"}})]).unwrap();
        let mut invoked = plan();
        invoked.assignments[0].agent = "opencode".into();
        repair_models(&mut invoked, &agent).unwrap();
        assert_eq!(invoked.assignments[0].model.as_deref(), Some("zai-coding-plan/glm-5.3"));
        assert!(validate(&invoked, &agent).is_ok());
    }
}
