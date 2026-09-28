//! Durable fleet delegation. Remote journals own native conversations; the
//! coordinator synchronizes evidence and never retries uncertain launches.
pub mod containers;
pub mod coordination;
pub mod inventory;
pub mod review;
pub mod relay;
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
    /// Peers whose replies or agreements are needed during this assignment.
    /// These are communication requirements, not completion prerequisites.
    #[serde(default)]
    pub peer_dependencies: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    /// Repository-relative globs exclusively owned by this assignment.
    #[serde(default)]
    pub owned_paths: Vec<String>,
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

/// When an exhausted provider quota recorded for a placement resets, if it is
/// still in the future. Runners record the latest usage snapshot (percent and
/// reset) from native rate-limit events; see `usage` in the runner journal.
pub fn quota_exhausted_until(attrs: &Value, now: i64) -> Option<i64> {
    let usage = &attrs["usage"];
    let resets_at = usage["resets_at"].as_f64()? as i64;
    let exhausted =
        usage["exhausted"] == true || usage["used_percent"].as_f64().is_some_and(|p| p >= 100.0);
    (exhausted && resets_at > now).then_some(resets_at)
}

pub fn quota_note(resets_at: i64) -> String {
    let time = chrono::DateTime::from_timestamp(resets_at, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| resets_at.to_string());
    format!("quota exhausted until {time}")
}

/// Whether a peer message hands work over by naming a branch or a commit:
/// "branch: main", "branch `main`", "on branch fix/usage-limit", "commit
/// 3f9c2ab". The bare words ("I'll push a branch soon") are not a handoff.
pub fn names_handoff(text: &str) -> bool {
    let branch = regex::Regex::new(
        r#"(?i)\bbranch(?:\s+name)?(?:\s*[:=]\s*[`'"]?[[:alnum:]][\w./-]*|\s+(?:is\s+)?[`'"][\w./-]+[`'"]|\s+(?:is\s+)?[[:alnum:]][\w.-]*[/._-][\w./-]*[[:alnum:]])"#,
    )
    .expect("static pattern");
    let commit = regex::Regex::new(r"(?i)\b(?:commit(?:ted)?|sha|head)\b[^.\n]{0,40}")
        .expect("static pattern");
    let hash = regex::Regex::new(r"(?i)\b[0-9a-f]{7,40}\b").expect("static pattern");
    branch.is_match(text)
        || commit.find_iter(text).any(|m| {
            hash.find_iter(m.as_str())
                .any(|h| h.as_str().chars().any(|c| c.is_ascii_digit()))
        })
}

/// The reset of the exhausted quota of an assignment's (device, agent), if any.
pub fn placement_quota(agent: &MasterAgent, assignment: &Assignment) -> anyhow::Result<Option<i64>> {
    let record = agent.memory.graph.entity(&entity_id(
        "device-agent",
        &format!("{}/{}", assignment.device, assignment.agent),
    ))?;
    Ok(record.and_then(|r| quota_exhausted_until(&r.attrs, chrono::Utc::now().timestamp())))
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
    validate_owned_paths(&plan.assignments)?;
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
            ["claude", "codex", "agy", "opencode", "cursor"].contains(&a.agent.as_str()),
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
        if let Some(until) = placement_quota(agent, a)? {
            anyhow::bail!(
                "{}: {} {}; choose another device or agent until it resets",
                a.device,
                a.agent,
                quota_note(until)
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
    // A peer conversation may be bilateral, but its recipient must be able to
    // start before the asker finishes. Catch planned circular waits here;
    // runtime message handling is a fallback for unplanned questions.
    for a in &plan.assignments {
        if a.peer_dependencies.is_empty() {
            continue;
        }
        let mut queued_behind = std::collections::HashSet::from([a.key.as_str()]);
        loop {
            let before = queued_behind.len();
            for candidate in &plan.assignments {
                if candidate.dependencies.iter().any(|d| queued_behind.contains(d.as_str())) {
                    queued_behind.insert(candidate.key.as_str());
                }
            }
            if queued_behind.len() == before {
                break;
            }
        }
        for peer in &a.peer_dependencies {
            anyhow::ensure!(keys.contains(peer), "Unknown peer dependency {peer} for {}", a.key);
            anyhow::ensure!(peer != &a.key, "Assignment {} cannot be its own peer dependency", a.key);
            anyhow::ensure!(
                !queued_behind.contains(peer.as_str()),
                "Assignment {} needs replies from {peer}, which is queued behind it by completion dependencies; remove the blocking dependencies so these peers can run concurrently",
                a.key
            );
        }
    }
    Ok(())
}

fn ownership_root(path: &str) -> anyhow::Result<&str> {
    let path = path.trim().trim_start_matches("./").trim_end_matches('/');
    anyhow::ensure!(!path.is_empty() && !path.starts_with('/')
        && !path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        && !path.contains(['\n', '\r', '\0']),
        "Owned paths must be non-empty repository-relative globs");
    let wildcard = path.find(['*', '?', '[', '{']).unwrap_or(path.len());
    Ok(path[..wildcard].trim_end_matches('/'))
}

fn paths_overlap(left: &str, right: &str) -> anyhow::Result<bool> {
    let left = ownership_root(left)?;
    let right = ownership_root(right)?;
    Ok(left.is_empty() || right.is_empty() || left == right
        || left.strip_prefix(right).is_some_and(|rest| rest.starts_with('/'))
        || right.strip_prefix(left).is_some_and(|rest| rest.starts_with('/')))
}

/// Reject equal or parent/child ownership across assignments in one task.
pub fn validate_owned_paths(assignments: &[Assignment]) -> anyhow::Result<()> {
    for (index, left) in assignments.iter().enumerate() {
        for right in assignments.iter().skip(index + 1) {
            for left_path in &left.owned_paths {
                for right_path in &right.owned_paths {
                    anyhow::ensure!(!paths_overlap(left_path, right_path)?,
                        "Assignments {} and {} have overlapping owned paths: {} and {}",
                        left.key, right.key, left_path, right_path);
                }
            }
        }
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

/// `chars` with parenthesized and quoted spans blanked out, one space per
/// character, so a placement merely mentioned there never binds. An opener
/// without its closer, or an apostrophe inside a word, stays literal.
fn mask_asides(chars: &[char]) -> Vec<char> {
    let mut out = chars.to_vec();
    let mut i = 0;
    while i < chars.len() {
        let after = i + 1..chars.len();
        let end = match chars[i] {
            '(' => {
                let mut depth = 0;
                (i..chars.len()).find(|&j| {
                    match chars[j] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
            }
            '"' | '`' => after.clone().find(|&j| chars[j] == chars[i]),
            '“' => after.clone().find(|&j| chars[j] == '”'),
            '‘' => after.clone().find(|&j| chars[j] == '’'),
            '\'' if i == 0 || !chars[i - 1].is_alphanumeric() => after.clone().find(|&j| {
                chars[j] == '\'' && chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric())
            }),
            _ => None,
        };
        match end {
            Some(end) => {
                out[i..=end].fill(' ');
                i = end + 1;
            }
            None => i += 1,
        }
    }
    out
}

/// The request's sentences as (masked, original) pairs. A sentence ends at a
/// newline or at `.`, `!`, `?` or `;` before whitespace, so device names and
/// model ids such as `air.example` or `gpt-5.2` stay whole.
fn sentences(request: &str) -> Vec<(String, String)> {
    let original: Vec<char> = request.chars().collect();
    let masked = mask_asides(&original);
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..masked.len() {
        let boundary = masked[i] == '\n'
            || (matches!(masked[i], '.' | '!' | '?' | ';')
                && masked.get(i + 1).is_none_or(|n| n.is_whitespace()));
        if boundary || i + 1 == masked.len() {
            let text: String = masked[start..=i].iter().collect();
            if !text.trim().is_empty() {
                out.push((text, original[start..=i].iter().collect::<String>().trim().to_string()));
            }
            start = i + 1;
        }
    }
    out
}

/// Whether a placement mention assigns work rather than merely naming it:
/// "Assignment 1: codex on air", "assignments: codex on air and …", an
/// imperative such as "use codex on air" a few words before it, or the
/// placement acting, as in "codex on air implements …". Negated imperatives
/// ("don't use codex on air") never bind.
fn binds(before: &str, after: &str) -> bool {
    const NUMBERS: &str = r"#?(?:\d+|one|two|three|four|five|six|seven|eight)";
    let labelled = regex::Regex::new(&format!(
        r"(?i)(?:\bassignment\s*{NUMBERS}\s*[:\-–—]\s*(?:the\s+)?$|\bassignments?\s*:)"
    ))
    .expect("static pattern");
    let directive = regex::Regex::new(
        r"(?i)\b(?:(don'?t|don’t|do\s+not|never|avoid|not)\s+)?(?:use|run|delegate|assign|put|place|start|launch|schedule|dispatch|spawn)\b(?:\s+\S+){0,6}\s*$",
    )
    .expect("static pattern");
    let acts = regex::Regex::new(
        r"(?i)^[\s,]*(?:implements?|reviews?|verif(?:y|ies)|tests?|builds?|writes?|handles?|owns?|fixes|does|should|will|must|shall|takes?)\b",
    )
    .expect("static pattern");
    labelled.is_match(before)
        || acts.is_match(after)
        || directive.captures(before).is_some_and(|c| c.get(1).is_none())
}

pub fn validate_explicit(
    request: &str,
    plan: &DelegationPlan,
    agent: &MasterAgent,
) -> anyhow::Result<()> {
    let sentences = sentences(request);
    let mut placements = Vec::new();
    for worker in &targets(agent) {
        let pattern = regex::Regex::new(&format!(
            r"(?i)\b(claude|codex|agy|opencode|cursor)(?:\s+agent)?\s+on\s+{}(?:\s+(?:using|with)\s+(?:the\s+)?model\s+([[:alnum:]][[:alnum:]_.:/@\[\]-]*[[:alnum:]_\]])|(?:$|\.(?:\s|$)|[^[:alnum:]_.-]))",
            regex::escape(&worker.name)
        ))?;
        for (masked, original) in &sentences {
            for captures in pattern.captures_iter(masked) {
                let whole = captures.get(0).expect("whole match");
                if !binds(&masked[..whole.start()], &masked[whole.end()..]) {
                    continue;
                }
                let selected = captures[1].to_ascii_lowercase();
                let model = captures.get(2).map(|m| m.as_str().to_string());
                let matches = |a: &Assignment| {
                    a.device == worker.name
                        && a.agent == selected
                        && model.as_ref().is_none_or(|m| a.model.as_ref() == Some(m))
                };
                anyhow::ensure!(
                    plan.assignments.iter().any(matches),
                    "Explicit placement requires {} on {}{}, as requested in: \"{}\"",
                    selected,
                    worker.name,
                    model
                        .as_ref()
                        .map(|m| format!(" using model {m}"))
                        .unwrap_or_default(),
                    original
                );
                placements.push((worker.name.clone(), selected, model));
            }
        }
    }
    let count = regex::Regex::new(
        r"(?i)\bexactly\s+(\d+|one|two|three|four|five|six|seven|eight)\s+(?:[[:alpha:]-]+\s+)?assignments?\b",
    )?;
    if let Some((captures, sentence)) = sentences
        .iter()
        .find_map(|(masked, original)| count.captures(masked).map(|c| (c, original)))
    {
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
            "Request requires exactly {expected} assignments, plan has {}, as requested in: \"{sentence}\"",
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

/// `conversation_id` only labels warnings about a malformed answer.
pub async fn plan(
    agent: &MasterAgent,
    request: &str,
    history: &str,
    conversation_id: Option<&str>,
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
        An agent inventory record with quota \"quota exhausted until <time>\" has used up its provider quota: plans placing new work on that device and agent are rejected until then, so choose another agent or device. \
        Prefer dedicated devices for ordinary work. Laptops/light hosts and login nodes only receive short light work. \
        GPU/shared scheduler work requires a scheduler allocation; never launch sustained work directly on login nodes. \
        Ordinary CLI coding tasks need required_capabilities=[]: Claude/Codex provider inference does NOT require local-inference on the worker. \
        Only require GPU or heavy-compute when the user explicitly needs that capability. \
        Every workspace is a fresh unique child of ~/hive-workspaces/. Each assignment has a unique key. \
        owned_paths are repository-relative globs exclusively owned by that assignment; never assign equal, parent, or child paths to two assignments. \
        dependencies are assignment keys that normally complete before this starts; a waiting dependency may wake its dependent early with a peer message. \
        peer_dependencies lists assignment keys whose replies or agreements this assignment needs during its work, or [] when none are required. These do not delay launch. \
        Peers that must negotiate concurrently have no completion dependency on each other: validation rejects a required peer queued directly or transitively behind its asker. \
        Acceptance criteria must require implementation, independent verification, deployment evidence when requested and peer agreement. \
        Never instruct agents to edit CHANGELOG.md directly; instruct them to add a changelog.d/ fragment instead. \
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
    "required":["key","device","agent","model","workspace","objective","dependencies","peer_dependencies","acceptance_criteria","owned_paths","required_capabilities"],"properties":{
        "key":{"type":"string"},"device":{"type":"string"},"agent":{"enum":["claude","codex","agy","opencode","cursor"]},
        "model":{"type":["string","null"]},"workspace":{"type":"string"},"objective":{"type":"string"},
        "dependencies":{"type":"array","items":{"type":"string"}},"peer_dependencies":{"type":"array","items":{"type":"string"}},"acceptance_criteria":{"type":"array","items":{"type":"string"}},
        "owned_paths":{"type":"array","items":{"type":"string"}},
        "required_capabilities":{"type":"array","items":{"type":"string"}}
    }}}}});
    // One retry in total, whether the answer was malformed JSON or a plan
    // that failed validation. Both attempts share the caller's deadline.
    for attempt in 0..2 {
        let response = agent
            .llm
            .complete_json_with(&prompt, hive_common::AiProvider::Local, &schema)
            .await?;
        let plan = match serde_json::from_str::<DelegationPlan>(&response.text) {
            Ok(plan) => plan,
            Err(e) => {
                let failure = crate::llm::JsonReplyError::new(&response.text, &e);
                failure.warn("delegation plan", conversation_id);
                if attempt == 0 {
                    prompt.push_str(&failure.retry_instruction());
                    continue;
                }
                return Err(failure.after_retry());
            }
        };
        let validated = (|| {
            let mut p = plan;
            repair_workspaces(&mut p);
            repair_models(&mut p, agent)?;
            validate(&p, agent)?;
            validate_explicit(request, &p, agent)?;
            validate_coordinator(request, &p, agent)?;
            validate_container_request(request, &p)?;
            Ok(p)
        })();
        match validated {
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
    fn owned_paths_accept_disjoint_and_reject_equal_or_nested() {
        let mut left = plan().assignments.remove(0);
        left.owned_paths = vec!["hive-core/src/**".into()];
        let mut right = left.clone();
        right.key = "b".into();
        right.owned_paths = vec!["hive-web/src/**".into()];
        validate_owned_paths(&[left.clone(), right.clone()]).unwrap();
        right.owned_paths = vec!["hive-core/src/**".into()];
        assert!(validate_owned_paths(&[left.clone(), right.clone()]).is_err());
        right.owned_paths = vec!["hive-core/src/delegation/mod.rs".into()];
        assert!(validate_owned_paths(&[left, right]).is_err());
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

    fn peer_plan() -> DelegationPlan {
        let mut p = plan();
        let mut verifier = p.assignments[0].clone();
        verifier.key = "verifier".into();
        verifier.workspace = "~/hive-workspaces/verifier".into();
        verifier.objective = "verify implementation".into();
        verifier.dependencies = vec!["a".into()];
        p.assignments.push(verifier);
        p
    }

    #[test]
    fn peer_dependencies_allow_verification_and_concurrent_conversations() {
        let agent = agent();
        let mut p = peer_plan();
        // Old saved plans remain valid; verification can wait for implementation
        // when the implementer does not already require a verifier's reply.
        assert!(p.assignments[0].peer_dependencies.is_empty());
        validate(&p, &agent).unwrap();
        p.assignments[0].peer_dependencies = vec!["verifier".into()];
        let error = validate(&p, &agent).unwrap_err().to_string();
        assert!(error.contains("needs replies from verifier, which is queued behind it"), "{error}");
        // Mutual peer requirements are legal once both can launch concurrently.
        p.assignments[1].dependencies.clear();
        p.assignments[1].peer_dependencies = vec!["a".into()];
        validate(&p, &agent).unwrap();
    }

    #[test]
    fn peer_dependencies_reject_transitive_completion_waits() {
        let agent = agent();
        let mut p = peer_plan();
        let mut middle = p.assignments[1].clone();
        middle.key = "middle".into();
        middle.workspace = "~/hive-workspaces/middle".into();
        p.assignments[1].dependencies = vec!["middle".into()];
        p.assignments.push(middle);
        p.assignments[0].peer_dependencies = vec!["verifier".into()];
        let error = validate(&p, &agent).unwrap_err().to_string();
        assert!(error.contains("needs replies from verifier, which is queued behind it"), "{error}");
    }

    #[test]
    fn peer_dependencies_reject_unknown_and_self_peers_without_relaxing_cycles() {
        let agent = agent();
        let mut p = peer_plan();
        for (peer, expected) in [("missing", "Unknown peer dependency"), ("a", "own peer dependency")] {
            p.assignments[0].peer_dependencies = vec![peer.into()];
            let error = validate(&p, &agent).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
        p.assignments[0].peer_dependencies = vec!["verifier".into()];
        p.assignments[0].dependencies = vec!["verifier".into()];
        assert!(validate(&p, &agent).unwrap_err().to_string().contains("cyclic assignment dependencies"));
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
    fn only_assignment_phrasing_binds_a_placement() {
        let agent = agent();
        // The plan has claude on air; each binding mention of codex on air
        // must therefore be rejected, and each non-binding one ignored.
        let p = plan();
        for binding in [
            "Assignment 1: codex on air.",
            "Assignment 2 - codex on air using model gpt-5.2",
            "Assignment #3: the codex on air",
            "codex on air implements the parser.",
            "Fix the parser; codex on air should review it.",
            "Two assignments: claude on air and codex on air.",
            "Please use codex on air for this.",
            "Delegate the migration to the codex agent on air.",
        ] {
            assert!(validate_explicit(binding, &p, &agent).is_err(), "{binding}");
        }
        for mention in [
            "Fix the flaky test (codex on air failed last time).",
            "Fix the flaky test (see the earlier run (codex on air) for context).",
            "The log said \"codex on air hit its usage limit\"; retry it.",
            "The log said “codex on air hit its usage limit”.",
            "Reproduce the `codex on air` failure locally.",
            "Investigate why 'codex on air' paused.",
            "Previously codex on air hit its usage limit.",
            "Don't use codex on air until its quota resets.",
            "Assignment 1: fix the parser that codex on air broke.",
        ] {
            assert!(validate_explicit(mention, &p, &agent).is_ok(), "{mention}");
        }
        // A real placement still binds next to an aside that mentions another.
        let aside = "Use claude on air (codex on air is out of quota).";
        assert!(validate_explicit(aside, &p, &agent).is_ok());
        let unmatched = "Use codex on air (claude on air did the last run).";
        assert!(validate_explicit(unmatched, &p, &agent).is_err());
        // An unbalanced opener or an in-word apostrophe never hides a placement.
        assert!(validate_explicit("It's simple (really: use codex on air.", &p, &agent).is_err());
    }
    #[test]
    fn explicit_rejections_quote_the_sentence_that_required_them() {
        let agent = agent();
        let p = plan();
        let request = "Refactor the parser (claude on air did the last one). Assignment 1: codex on air implements it.";
        let err = validate_explicit(request, &p, &agent).unwrap_err().to_string();
        assert_eq!(
            err,
            "Explicit placement requires codex on air, as requested in: \"Assignment 1: codex on air implements it.\""
        );
        let mut two = p.clone();
        let mut second = two.assignments[0].clone();
        second.key = "b".into();
        two.assignments.push(second);
        let err = validate_explicit("Keep it small.\nDo this in exactly one assignment please.", &two, &agent)
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("as requested in: \"Do this in exactly one assignment please.\""), "{err}");
        // A count inside quotes or parentheses is not a requirement.
        assert!(validate_explicit("Last time (exactly one assignment) was too few.", &two, &agent).is_ok());
    }
    #[test]
    fn exhausted_quota_rejects_new_assignments_until_it_resets() {
        let agent = agent();
        let p = plan();
        let now = chrono::Utc::now().timestamp();
        let record = |usage: Value| {
            inventory::project(&agent.memory.graph, "air", &[json!({"agent":"claude","executable":"/claude",
                "runtime_ready":true,"authentication":"authenticated","usage":usage})]).unwrap();
        };
        record(json!({"agent":"claude","used_percent":100,"resets_at":now+3600,"exhausted":true}));
        let err = validate(&p, &agent).unwrap_err().to_string();
        let until = chrono::DateTime::from_timestamp(now + 3600, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        assert!(err.contains(&format!("air: claude quota exhausted until {until}")), "{err}");
        let described = inventory::describe(&agent.memory.graph).unwrap();
        assert!(described.contains(&format!("\"quota\": \"quota exhausted until {until}\"")), "{described}");
        // Other agents on the device, a reset quota and partial usage are fine.
        let mut codex = p.clone();
        codex.assignments[0].agent = "codex".into();
        assert!(validate(&codex, &agent).is_ok());
        record(json!({"agent":"claude","used_percent":100,"resets_at":now-1,"exhausted":true}));
        assert!(validate(&p, &agent).is_ok());
        record(json!({"agent":"claude","used_percent":82,"resets_at":now+3600,"exhausted":false}));
        assert!(validate(&p, &agent).is_ok());
        assert!(!inventory::describe(&agent.memory.graph).unwrap().contains("\"quota\""));
    }
    #[test]
    fn handoff_messages_name_a_branch_or_commit() {
        for handoff in [
            "Pushed branch fix/usage-limit-pause",
            "Work is on branch: main",
            "branch `main` has the parser",
            "branch name is 'feature_x'",
            "Committed 3f9c2ab0 with the interface",
            "HEAD is at 1a2b3c4d5e6f",
            "commit: 0123456789abcdef0123456789abcdef01234567",
        ] {
            assert!(names_handoff(handoff), "{handoff}");
        }
        for chatter in [
            "I'll push a branch soon",
            "Which branch should I use?",
            "I will commit once tests pass",
            "commit message defaced by the linter",
            "Quota paused; resuming at 3pm",
        ] {
            assert!(!names_handoff(chatter), "{chatter}");
        }
    }
    #[test]
    fn quota_snapshots_are_exhausted_only_until_their_reset() {
        let now = 1_790_000_000;
        let usage = |v: Value| json!({ "usage": v });
        assert_eq!(quota_exhausted_until(&usage(json!({"used_percent":100,"resets_at":now+5})), now), Some(now + 5));
        assert_eq!(quota_exhausted_until(&usage(json!({"exhausted":true,"used_percent":40,"resets_at":now+5})), now), Some(now + 5));
        assert_eq!(quota_exhausted_until(&usage(json!({"used_percent":99,"resets_at":now+5})), now), None);
        assert_eq!(quota_exhausted_until(&usage(json!({"used_percent":100,"resets_at":now})), now), None);
        assert_eq!(quota_exhausted_until(&usage(json!({"used_percent":100})), now), None);
        assert_eq!(quota_exhausted_until(&json!({}), now), None);
        assert_eq!(quota_note(1_790_000_000), "quota exhausted until 2026-09-21T14:13:20Z");
    }
    #[test]
    fn cursor_assignments_validate_and_honour_explicit_placements() {
        let agent = agent();
        let mut p = plan();
        p.assignments[0].agent = "cursor".into();
        p.assignments[0].model = Some("gpt-5.2".into());
        // Cursor is a known type, so the plan validates.
        assert!(validate(&p, &agent).is_ok());
        // It is also recognised as an explicit placement the user named.
        assert!(validate_explicit("Use cursor on air", &p, &agent).is_ok());
        assert!(validate_explicit("Use cursor on air using model gpt-5.2", &p, &agent).is_ok());
        // The model the user names has to be the one assigned.
        assert!(validate_explicit("Use cursor on air using model gpt-5.3-codex", &p, &agent).is_err());
        // A device name that merely starts like the worker's is not a
        // placement: codex is in no assignment, so a match would be an error.
        assert!(validate_explicit("Use codex on air_backup", &p, &agent).is_ok());
        assert!(validate_explicit("Use codex on air", &p, &agent).is_err());
        // An unknown type is still rejected.
        let mut unknown = p.clone();
        unknown.assignments[0].agent = "cursr".into();
        let err = validate(&unknown, &agent).unwrap_err().to_string();
        assert!(err.contains("Unknown agent"), "{err}");
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
