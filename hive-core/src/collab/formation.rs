//! Formation — deriving how many agents a goal needs, and who takes which role.
//!
//! This is where HIVE's central claim either becomes true or stays marketing: the number
//! of agents is a **decision derived from the task**, not a configuration value and not a
//! fixed template. `spec/HACP.md` §7 makes that testable — an implementation that emits
//! the same count for every goal is non-conformant, and `rationale` must say *why this
//! many*, not restate how many.
//!
//! Two design choices here are worth stating up front, because both were made against the
//! easier alternative:
//!
//! * **The model never chooses the topology or the count.** It proposes *roles*; the count
//!   is `roles.len()` and the topology is [`Topology::for_agent_count`]. Letting the model
//!   emit all three independently produces disagreements that
//!   [`TaskDecomposition::validate`] then rejects — the model would be graded on
//!   arithmetic rather than on reasoning, which is the one thing it is here to do.
//!
//! * **The prompt is a worked table, not a rule.** `docs/PLACEMENT.md` §3 measured this on
//!   the same local model: a worked table scored 14/14 where a prose rule scored 11/15.
//!   The table's job is to carry one idea a rule states badly — *components are not
//!   agents* — so at least two rows deliberately show a count that is lower than the
//!   component count.
//!
//! What is honestly not yet true: nothing here checks that the count actually varies
//! across goals. That is a conformance test against a live model (§15), not a unit test,
//! and it is unwritten. The tests below cover everything that does not need a live LLM.

use std::sync::Arc;

use async_trait::async_trait;
use hacp::topology::{Component, RoleSpec, TaskDecomposition, Topology};
use hive_common::Complexity;
use serde::Deserialize;
use uuid::Uuid;

use crate::collab::{AgentCandidate, Formation, Result};
use crate::llm::LlmRouter;

/// Formation by asking a model to analyse the goal's structure.
///
/// Holds the router behind an `Arc` because the orchestrator shares one router across the
/// whole run, as [`crate::agent`] and [`crate::workers`] already do.
pub struct LlmFormation {
    llm: Arc<LlmRouter>,
}

impl LlmFormation {
    pub fn new(llm: Arc<LlmRouter>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl Formation for LlmFormation {
    async fn decompose(&self, goal: &str, available: &[AgentCandidate]) -> Result<TaskDecomposition> {
        let prompt = build_prompt(goal, available);

        // Formation is the most consequential reasoning in a run — it decides the shape of
        // everything after it — so it asks for the strongest provider the router has.
        // `route_and_execute` falls back to local on its own if that one is unconfigured.
        let response = self.llm.route_and_execute(&prompt, Complexity::Complex).await?;

        let proposed = match extract_decomposition(&response.text) {
            Ok(d) if d.roles.is_empty() => {
                // A decomposition with no roles is not a small answer, it is a non-answer:
                // `validate` accepts `agent_count == 0` and would let it through as if a
                // team of nobody had been reasoned out.
                tracing::warn!("Formation: model returned no roles for goal: {goal}");
                return Ok(undecomposed(goal, "the model proposed no roles"));
            }
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Formation: could not parse a decomposition ({e}) for goal: {goal}");
                return Ok(undecomposed(goal, &format!("the model's reply did not parse ({e})")));
            }
        };

        // A validation failure is NOT fallback territory. The model did reason; its
        // reasoning is structurally wrong, and saying so is more useful than replacing it
        // with a default that pretends nothing happened.
        finish(goal, proposed)
    }

    async fn assign(
        &self,
        decomposition: &TaskDecomposition,
        available: &[AgentCandidate],
    ) -> Result<Vec<(String, String)>> {
        assign_roles(decomposition, available)
    }
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// The worked table that carries the sizing rule.
///
/// Kept as its own constant so a future edit has to confront it deliberately. Rows 3 and 6
/// are the load-bearing ones: both show *fewer agents than components*, which is the fact a
/// prose rule ("merge components that share an interface") reliably fails to convey.
const WORKED_EXAMPLES: &str = "\
| goal | components | agents | topology | why THAT many |\n\
|---|---|---|---|---|\n\
| Add a --json flag to an existing CLI command | 1 | 1 | solo | One file, one interface, nothing to hand off. A second agent would only create a merge to reconcile. |\n\
| Build a REST API and a web page that consumes it | 2 | 2 | peer | Two components, one shared interface (the HTTP schema). Two parties can settle an interface between themselves. |\n\
| Port one library to macOS, Linux and Windows behind a shared abstraction layer | 4 | 2 | peer | The three ports are the SAME work against ONE interface, so one agent does all three; the abstraction layer is the other. Components are not agents. |\n\
| Build an ingest service, a job scheduler, a worker pool and a metrics dashboard | 4 | 4 | federated | Four components that can proceed in parallel, three distinct interfaces between them. Three or more parties cannot settle a disagreement alone, so an arbiter is required. |\n\
| Write a design document for a caching layer | 1 | 1 | solo | Analysis, not construction. Splitting prose across agents produces seams, not speed. |\n\
| Add OAuth login: server endpoints, session storage, and the login form | 3 | 2 | peer | The endpoints and session storage share one data model and change together, so one agent takes both. The form is separate work against a published interface. |\n";

/// The capability table, in the same worked-example style and for the same measured reason
/// (`docs/PLACEMENT.md` §2–3). Naming a capability nothing in the fleet has makes a role
/// unassignable, so the fleet's actual capabilities are listed alongside it.
const CAPABILITY_EXAMPLES: &str = "\
  compiling, cargo build, make, npm build       -> [\"build\"]\n  \
  CUDA, nvidia-smi, training a model            -> [\"gpu-compute\"]\n  \
  docker, podman, containers                    -> [\"containers\"]\n  \
  ollama, running a local model                 -> [\"local-inference\"]\n  \
  sbatch, srun, queueing a job                  -> [\"batch-scheduler\"]\n  \
  psql, schema migrations                       -> [\"database\"]\n  \
  writing code or docs, ordinary shell work     -> []\n";

fn build_prompt(goal: &str, available: &[AgentCandidate]) -> String {
    let fleet = fleet_summary(available);
    format!(
        "You are the arbiter of a multi-agent run. Analyse the goal below into components \
         and their dependencies, then DERIVE how many agents it needs.\n\n\
         Goal: {goal}\n\n\
         {fleet}\n\
         How to size the team. Study these examples and match the reasoning in the last \
         column — the agent count follows from the DEPENDENCY STRUCTURE and is NOT simply \
         the number of components:\n\n\
         {WORKED_EXAMPLES}\n\
         Working rules drawn from those rows:\n  \
           - Components that share one interface AND change together belong to ONE agent.\n  \
           - Split only where the pieces can genuinely proceed in parallel.\n  \
           - Every artifact a component consumes must be produced by some component. \
         Name the artifacts; that is what makes the dependencies checkable.\n  \
           - \"rationale\" must say why THIS many. \"Three agents for three components\" is \
         not a reason; \"three components, but two share a data model and change together, \
         so two agents\" is.\n\n\
         required_capabilities names what the agent's MACHINE must provide:\n\
         {CAPABILITY_EXAMPLES}\
         Do not invent a requirement the work does not need: an unnecessary one leaves the \
         role with no agent to take it.\n\n\
         Respond with ONLY a JSON object of this exact shape, no prose, no markdown fences:\n\
         {{\n  \
           \"analysis\": \"the goal's structure: the components, and which depend on which\",\n  \
           \"components\": [\n    \
             {{\n      \
               \"component_id\": \"short-id\",\n      \
               \"description\": \"what this piece is\",\n      \
               \"required_capabilities\": [],\n      \
               \"produces\": [\"artifact-id\"],\n      \
               \"consumes\": []\n    \
             }}\n  \
           ],\n  \
           \"roles\": [\n    \
             {{\"role_id\": \"a\", \"components\": [\"short-id\"], \"required_capabilities\": []}}\n  \
           ],\n  \
           \"rationale\": \"why THIS many agents, from the dependency structure\"\n\
         }}\n\n\
         One role is one agent. Give every component to exactly one role, and give every \
         role at least one component. Do not emit \"agent_count\" or \"topology\" — both \
         follow from the roles you write and are computed, not chosen.\n"
    )
}

/// What the model is told about who is actually available.
///
/// The count is a ceiling worth stating: a decomposition asking for eight agents when four
/// exist is reasoning that cannot be executed. Listing the fleet's real capabilities keeps
/// `required_capabilities` to names that can be satisfied.
fn fleet_summary(available: &[AgentCandidate]) -> String {
    if available.is_empty() {
        return String::new();
    }
    let mut caps: Vec<&str> = available
        .iter()
        .flat_map(|c| c.capabilities.iter().map(String::as_str))
        .collect();
    caps.sort_unstable();
    caps.dedup();

    let n = available.len();
    let caps = if caps.is_empty() {
        "none declared".to_string()
    } else {
        caps.join(", ")
    };
    format!(
        "There are {n} agents available, so do not propose more than {n} roles. \
         Capabilities present in this fleet: {caps}.\n"
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// What the model is asked for — deliberately narrower than [`TaskDecomposition`].
///
/// `agent_count` is accepted only so a disagreement with `roles.len()` can be *logged*;
/// `topology` is not accepted at all. Both are derived in [`finish`].
///
/// The fields mirror the hacp types rather than reusing them so that `description` and the
/// capability lists can default: a local model that omits a prose description has still
/// produced a real decomposition, and throwing it away over a missing string would be
/// strictness in the wrong place.
#[derive(Debug, Deserialize)]
struct ProposedDecomposition {
    #[serde(default)]
    analysis: String,
    #[serde(default)]
    components: Vec<ProposedComponent>,
    #[serde(default)]
    roles: Vec<ProposedRole>,
    #[serde(default)]
    rationale: String,
    /// The model's own claim about the count, kept only to notice when it disagrees.
    #[serde(default)]
    agent_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ProposedComponent {
    component_id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    required_capabilities: Vec<String>,
    #[serde(default)]
    produces: Vec<String>,
    #[serde(default)]
    consumes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProposedRole {
    role_id: String,
    #[serde(default)]
    components: Vec<String>,
    #[serde(default)]
    required_capabilities: Vec<String>,
}

/// Extract a decomposition from a reply that may be wrapped in prose or markdown fences.
///
/// Same span-between-outermost-braces approach as [`crate::agent::planner`]'s
/// `extract_plan`: a local model answers "Sure, here's the plan:" and fences its JSON, and
/// this repo already settled on how to cope. Keeping the two identical means a reply that
/// works for one works for the other.
fn extract_decomposition(text: &str) -> anyhow::Result<ProposedDecomposition> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in response"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in response"))?;
    if end < start {
        anyhow::bail!("malformed JSON in response");
    }
    Ok(serde_json::from_str(&text[start..=end])?)
}

/// Turn a proposal into a validated [`TaskDecomposition`].
///
/// The count and the topology are computed here and nowhere else. Errors are returned
/// rather than repaired: a decomposition that fails §7 is a wrong answer to the run's most
/// consequential question, and quietly patching it would hide that from the audit trail
/// `run.plan` exists to provide.
fn finish(goal: &str, proposed: ProposedDecomposition) -> Result<TaskDecomposition> {
    let agent_count = proposed.roles.len();

    if let Some(claimed) = proposed.agent_count {
        if claimed != agent_count {
            // Not fatal — the roles are the substance and the count is bookkeeping — but a
            // model that cannot count its own roles is worth noticing in the log.
            tracing::warn!(
                "Formation: model claimed agent_count={claimed} but wrote {agent_count} roles; \
                 using the roles"
            );
        }
    }

    // A role with no components is an agent with nothing to do. §7's structural rules do
    // not catch it (`validate` only walks components -> roles), yet it inflates the count
    // the whole section exists to make honest, so it is refused here.
    if let Some(empty) = proposed.roles.iter().find(|r| r.components.is_empty()) {
        anyhow::bail!(
            "role {:?} has no components: an agent with nothing to do inflates agent_count",
            empty.role_id
        );
    }

    let decomposition = TaskDecomposition {
        decomposition_id: format!("d-{}", Uuid::new_v4()),
        goal: goal.to_string(),
        analysis: proposed.analysis,
        components: proposed
            .components
            .into_iter()
            .map(|c| Component {
                component_id: c.component_id,
                description: c.description,
                required_capabilities: c.required_capabilities,
                produces: c.produces,
                consumes: c.consumes,
            })
            .collect(),
        roles: proposed
            .roles
            .into_iter()
            .map(|r| RoleSpec {
                role_id: r.role_id,
                components: r.components,
                required_capabilities: r.required_capabilities,
            })
            .collect(),
        agent_count,
        // Never the model's choice: §4 ties the topology to the count, and a model that
        // picks them separately produces a pair that `validate` rejects.
        topology: Topology::for_agent_count(agent_count),
        rationale: proposed.rationale,
    };

    decomposition.validate()?;
    Ok(decomposition)
}

/// The fallback when nothing usable came back.
///
/// It must not read like a decision. One agent is the conservative shape — solo needs no
/// contract and no arbiter, so it commits the run to the least — but the `rationale` is
/// required by §7 to say why *this many*, and the only honest answer here is that nothing
/// derived it. Anything smoother would launder a parse failure into reasoning, and the
/// audit trail would show a confident sizing that never happened.
fn undecomposed(goal: &str, why: &str) -> TaskDecomposition {
    TaskDecomposition {
        decomposition_id: format!("d-{}", Uuid::new_v4()),
        goal: goal.to_string(),
        analysis: format!("NOT DERIVED — {why}. The goal's structure was not analysed."),
        components: vec![Component {
            component_id: "goal".to_string(),
            description: goal.to_string(),
            required_capabilities: Vec::new(),
            produces: Vec::new(),
            consumes: Vec::new(),
        }],
        roles: vec![RoleSpec {
            role_id: "a".to_string(),
            components: vec!["goal".to_string()],
            required_capabilities: Vec::new(),
        }],
        agent_count: 1,
        topology: Topology::Solo,
        rationale: format!(
            "NOT DERIVED — {why}, so this decomposition was not reasoned out. One agent is \
             the conservative default, chosen because no analysis is available, NOT because \
             the goal's structure calls for it. Treat this count as unjustified."
        ),
    }
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

/// Everything a candidate must declare to take `role`: the role's own requirements plus
/// those of every component assigned to it.
///
/// The union matters. A model routinely puts `gpu-compute` on the component that trains
/// the model and leaves the role's own list empty; reading only the role would then place
/// that work on a box with no GPU — the exact silent substitution `docs/PLACEMENT.md` §4
/// forbids.
fn requirements_for(decomposition: &TaskDecomposition, role: &RoleSpec) -> Vec<String> {
    let mut caps: Vec<String> = role.required_capabilities.clone();
    for id in &role.components {
        if let Some(c) = decomposition
            .components
            .iter()
            .find(|c| &c.component_id == id)
        {
            caps.extend(c.required_capabilities.iter().cloned());
        }
    }
    caps.sort();
    caps.dedup();
    caps
}

fn declares_all(candidate: &AgentCandidate, needed: &[String]) -> bool {
    needed
        .iter()
        .all(|n| candidate.capabilities.iter().any(|have| have == n))
}

/// Bind roles to candidates.
///
/// Capability claims *inform* this and gate nothing else (§8): admission happened before
/// we got here, and a role that requires nothing admits an agent that declares nothing —
/// including one whose adapter declared wrongly. That asymmetry is the whole content of
/// §8, and it is why the empty-requirement path below has no filter in it at all.
fn assign_roles(
    decomposition: &TaskDecomposition,
    available: &[AgentCandidate],
) -> Result<Vec<(String, String)>> {
    if decomposition.roles.is_empty() {
        return Ok(Vec::new());
    }
    if available.is_empty() {
        anyhow::bail!(
            "no agents available: {} role(s) to fill, none offered",
            decomposition.roles.len()
        );
    }

    let requirements: Vec<Vec<String>> = decomposition
        .roles
        .iter()
        .map(|r| requirements_for(decomposition, r))
        .collect();

    // Fill the most constrained roles first. Taking roles in declaration order lets an
    // unconstrained role consume the fleet's only GPU box and leave the GPU role with
    // nothing — a collision that is avoidable, so it should be avoided.
    let mut order: Vec<usize> = (0..decomposition.roles.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(requirements[i].len()), i));

    let mut taken = vec![false; available.len()];
    let mut bound: Vec<Option<String>> = vec![None; decomposition.roles.len()];

    for &i in &order {
        let role = &decomposition.roles[i];
        let needed = &requirements[i];

        let fits: Vec<usize> = (0..available.len())
            .filter(|&k| declares_all(&available[k], needed))
            .collect();

        if fits.is_empty() {
            // Refuse, naming what is missing. Substituting an agent that cannot do the
            // work fails far later and far more confusingly than this error does
            // (`docs/PLACEMENT.md` §4).
            let unmet: Vec<&str> = needed
                .iter()
                .filter(|n| !available.iter().any(|c| c.capabilities.contains(n)))
                .map(String::as_str)
                .collect();
            if unmet.is_empty() {
                anyhow::bail!(
                    "role {:?} requires {:?} together; each is declared by some agent, but no \
                     single agent declares all of them. Refusing to substitute one that \
                     cannot do the work",
                    role.role_id,
                    needed
                );
            }
            anyhow::bail!(
                "role {:?} requires capability {:?}, which no available agent declares \
                 ({} candidates). Refusing to substitute an agent that cannot do the work",
                role.role_id,
                unmet,
                available.len()
            );
        }

        // Prefer an agent that is not already holding a role: one process doing two roles
        // serialises work the decomposition said could run in parallel.
        match fits.iter().copied().find(|&k| !taken[k]) {
            Some(k) => {
                taken[k] = true;
                bound[i] = Some(available[k].cli_label.clone());
            }
            None => {
                let k = fits[0];
                let also: Vec<&str> = bound
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.as_deref() == Some(available[k].cli_label.as_str()))
                    .map(|(j, _)| decomposition.roles[j].role_id.as_str())
                    .collect();
                tracing::warn!(
                    "Formation: agent {:?} takes role {:?} while already holding {:?} — no \
                     other available agent satisfies {:?}. Those roles will run serially.",
                    available[k].cli_label,
                    role.role_id,
                    also,
                    needed
                );
                bound[i] = Some(available[k].cli_label.clone());
            }
        }
    }

    Ok(decomposition
        .roles
        .iter()
        .zip(bound)
        .map(|(r, label)| {
            // Every slot is filled: the loop above either binds or returns an error.
            (r.role_id.clone(), label.expect("every role was bound"))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(label: &str, caps: &[&str]) -> AgentCandidate {
        AgentCandidate {
            cli_label: label.to_string(),
            machine: "m".to_string(),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A decomposition with `n` roles, one component each, no requirements.
    fn plain(n: usize) -> TaskDecomposition {
        TaskDecomposition {
            decomposition_id: "d-test".into(),
            goal: "g".into(),
            analysis: String::new(),
            components: (0..n)
                .map(|i| Component {
                    component_id: format!("c{i}"),
                    description: String::new(),
                    required_capabilities: vec![],
                    produces: vec![],
                    consumes: vec![],
                })
                .collect(),
            roles: (0..n)
                .map(|i| RoleSpec {
                    role_id: format!("r{i}"),
                    components: vec![format!("c{i}")],
                    required_capabilities: vec![],
                })
                .collect(),
            agent_count: n,
            topology: Topology::for_agent_count(n),
            rationale: "because".into(),
        }
    }

    // --- parsing -----------------------------------------------------------

    #[test]
    fn extracts_clean_json() {
        let text = r#"{"analysis":"a","components":[{"component_id":"c","description":"d"}],
                       "roles":[{"role_id":"r","components":["c"]}],"rationale":"why one"}"#;
        let d = extract_decomposition(text).unwrap();
        assert_eq!(d.roles.len(), 1);
        assert_eq!(d.components[0].component_id, "c");
        assert_eq!(d.rationale, "why one");
    }

    #[test]
    fn extracts_from_markdown_fenced_json_wrapped_in_prose() {
        // Exactly what a local model actually returns.
        let text = "Sure! Here is the decomposition:\n\
                    ```json\n\
                    {\"components\":[{\"component_id\":\"c\"}],\
                      \"roles\":[{\"role_id\":\"r\",\"components\":[\"c\"]}],\
                      \"rationale\":\"one component, one agent\"}\n\
                    ```\n\
                    Let me know if you want it split differently!";
        let d = extract_decomposition(text).expect("fenced JSON must parse");
        assert_eq!(d.roles.len(), 1);
        assert_eq!(d.components[0].description, "", "description may default");
    }

    #[test]
    fn rejects_output_with_no_json() {
        assert!(extract_decomposition("I'd rather explain it in words.").is_err());
        assert!(extract_decomposition("}{").is_err());
    }

    #[test]
    fn tolerates_a_missing_description() {
        // Losing a real decomposition over an absent prose string would be strictness in
        // the wrong place.
        let d = extract_decomposition(
            r#"{"components":[{"component_id":"c"}],"roles":[{"role_id":"r","components":["c"]}],"rationale":"x"}"#,
        )
        .unwrap();
        assert_eq!(d.components.len(), 1);
    }

    // --- topology and count derivation -------------------------------------

    #[test]
    fn topology_and_count_follow_the_roles_not_the_model() {
        // The model says one agent, solo. It wrote three roles. The roles win, and the
        // topology follows them — otherwise §4 and §7 disagree and `validate` rejects.
        let text = r#"{
            "components":[{"component_id":"a"},{"component_id":"b"},{"component_id":"c"}],
            "roles":[{"role_id":"x","components":["a"]},
                     {"role_id":"y","components":["b"]},
                     {"role_id":"z","components":["c"]}],
            "agent_count": 1,
            "topology": "solo",
            "rationale":"three independent components"
        }"#;
        let d = finish("goal", extract_decomposition(text).unwrap()).expect("must validate");
        assert_eq!(d.agent_count, 3);
        assert_eq!(d.topology, Topology::Federated, "3 agents require an arbiter (§4)");
        assert!(d.validate().is_ok());
    }

    #[test]
    fn each_count_gets_the_topology_section_four_requires() {
        for (roles, want) in [(1, Topology::Solo), (2, Topology::Peer), (5, Topology::Federated)] {
            let proposed = ProposedDecomposition {
                analysis: String::new(),
                components: (0..roles)
                    .map(|i| ProposedComponent {
                        component_id: format!("c{i}"),
                        description: String::new(),
                        required_capabilities: vec![],
                        produces: vec![],
                        consumes: vec![],
                    })
                    .collect(),
                roles: (0..roles)
                    .map(|i| ProposedRole {
                        role_id: format!("r{i}"),
                        components: vec![format!("c{i}")],
                        required_capabilities: vec![],
                    })
                    .collect(),
                rationale: "derived".into(),
                agent_count: None,
            };
            let d = finish("g", proposed).expect("must validate");
            assert_eq!(d.topology, want, "{roles} roles");
            assert_eq!(d.agent_count, roles);
        }
    }

    // --- validation is never skipped ---------------------------------------

    #[test]
    fn rejects_a_component_no_role_owns() {
        let text = r#"{"components":[{"component_id":"a"},{"component_id":"orphan"}],
                       "roles":[{"role_id":"x","components":["a"]}],
                       "rationale":"one agent"}"#;
        let e = finish("g", extract_decomposition(text).unwrap()).unwrap_err();
        assert!(e.to_string().contains("orphan"), "error must name it: {e}");
    }

    #[test]
    fn rejects_an_artifact_nothing_produces() {
        let text = r#"{"components":[{"component_id":"ui","consumes":["http-schema"]}],
                       "roles":[{"role_id":"x","components":["ui"]}],
                       "rationale":"one agent"}"#;
        let e = finish("g", extract_decomposition(text).unwrap()).unwrap_err();
        assert!(e.to_string().contains("http-schema"), "error must name it: {e}");
    }

    #[test]
    fn rejects_a_missing_rationale() {
        // §7's point: the count must be justified, so an unjustified one is not a
        // decomposition at all.
        let text = r#"{"components":[{"component_id":"a"}],
                       "roles":[{"role_id":"x","components":["a"]}],
                       "rationale":"   "}"#;
        let e = finish("g", extract_decomposition(text).unwrap()).unwrap_err();
        assert!(e.to_string().contains("rationale"), "{e}");
    }

    #[test]
    fn rejects_a_role_with_nothing_to_do() {
        let text = r#"{"components":[{"component_id":"a"}],
                       "roles":[{"role_id":"x","components":["a"]},{"role_id":"idle","components":[]}],
                       "rationale":"two agents"}"#;
        let e = finish("g", extract_decomposition(text).unwrap()).unwrap_err();
        assert!(e.to_string().contains("idle"), "{e}");
    }

    // --- the fallback must not pretend -------------------------------------

    #[test]
    fn fallback_is_valid_but_says_plainly_that_it_derived_nothing() {
        let d = undecomposed("ship the thing", "the model's reply did not parse");
        d.validate().expect("the fallback must still be a legal decomposition");
        assert_eq!(d.agent_count, 1);
        assert_eq!(d.topology, Topology::Solo);

        let r = d.rationale.to_lowercase();
        assert!(r.contains("not derived"), "must not read like a decision: {}", d.rationale);
        assert!(
            r.contains("not because"),
            "must say the count is a default, not a conclusion: {}",
            d.rationale
        );
        assert!(d.analysis.to_lowercase().contains("not derived"));
        assert_eq!(d.goal, "ship the thing", "the goal is still recorded verbatim");
    }

    // --- assignment: capabilities inform, they never gate -------------------

    #[test]
    fn refuses_to_substitute_when_a_capability_is_missing() {
        let mut d = plain(1);
        d.roles[0].required_capabilities = vec!["gpu-compute".into()];

        let e = assign_roles(&d, &[agent("cli-1", &["build"]), agent("cli-2", &["containers"])])
            .expect_err("must refuse rather than pick any available agent");

        let msg = e.to_string();
        assert!(msg.contains("gpu-compute"), "must name the missing capability: {msg}");
        assert!(
            !msg.contains("cli-1") && !msg.contains("cli-2"),
            "must not have chosen a substitute: {msg}"
        );
    }

    #[test]
    fn refuses_when_a_component_needs_what_nothing_has() {
        // The requirement lives on the component, not the role. Reading only the role
        // would place training work on a box with no GPU.
        let mut d = plain(1);
        d.components[0].required_capabilities = vec!["gpu-compute".into()];
        assert!(d.roles[0].required_capabilities.is_empty());

        let e = assign_roles(&d, &[agent("cli-1", &["build"])]).expect_err("must refuse");
        assert!(e.to_string().contains("gpu-compute"), "{e}");
    }

    #[test]
    fn refuses_when_the_combination_exists_nowhere_even_though_each_part_does() {
        let mut d = plain(1);
        d.roles[0].required_capabilities = vec!["gpu-compute".into(), "database".into()];

        let e = assign_roles(&d, &[agent("gpu-box", &["gpu-compute"]), agent("db-box", &["database"])])
            .expect_err("half a match is not a match");
        assert!(e.to_string().contains("together"), "{e}");
    }

    #[test]
    fn assigns_on_declared_capabilities() {
        let mut d = plain(2);
        d.roles[0].required_capabilities = vec!["gpu-compute".into()];
        d.roles[1].required_capabilities = vec!["build".into()];

        let got = assign_roles(
            &d,
            &[agent("builder", &["build"]), agent("trainer", &["gpu-compute", "build"])],
        )
        .expect("both roles are satisfiable");

        assert_eq!(
            got,
            vec![("r0".to_string(), "trainer".to_string()), ("r1".to_string(), "builder".to_string())],
            "the constrained role must not lose the only GPU box to the unconstrained one"
        );
    }

    #[test]
    fn does_not_double_book_when_an_alternative_exists() {
        let d = plain(3);
        let got = assign_roles(
            &d,
            &[agent("a", &[]), agent("b", &[]), agent("c", &[]), agent("spare", &[])],
        )
        .unwrap();

        let mut labels: Vec<&str> = got.iter().map(|(_, l)| l.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 3, "three roles must land on three distinct agents");
    }

    #[test]
    fn reuses_an_agent_only_when_there_is_no_alternative() {
        let d = plain(2);
        let got = assign_roles(&d, &[agent("only-one", &[])]).expect("scarcity is not a refusal");
        assert_eq!(
            got,
            vec![
                ("r0".to_string(), "only-one".to_string()),
                ("r1".to_string(), "only-one".to_string())
            ]
        );
        // The doubling is reported through `tracing::warn!`; the contract this test pins is
        // that scarcity does not turn into a hard failure the way a missing capability does.
    }

    #[test]
    fn an_agent_declaring_nothing_is_still_assignable() {
        // §8: capability claims inform assignment and MUST NOT gate admission. A role that
        // requires nothing has nothing to filter on, so an empty (or wrongly empty)
        // manifest must not lock a capable agent out.
        let d = plain(1);
        let got = assign_roles(&d, &[agent("undeclared", &[])]).expect("must be assignable");
        assert_eq!(got, vec![("r0".to_string(), "undeclared".to_string())]);
    }

    #[test]
    fn no_agents_at_all_is_an_error_not_an_empty_binding() {
        let e = assign_roles(&plain(2), &[]).unwrap_err();
        assert!(e.to_string().contains("no agents available"), "{e}");
        // But a decomposition with no roles asks for nothing, and gets nothing.
        assert!(assign_roles(&plain(0), &[]).unwrap().is_empty());
    }

    // --- the prompt carries the measured design ----------------------------

    #[test]
    fn prompt_teaches_by_worked_table_not_by_prose_rule() {
        // `docs/PLACEMENT.md` §3 measured 14/14 for a worked table against 11/15 for a
        // prose rule on the same local model. If someone replaces the table with a
        // sentence, this fails.
        let p = build_prompt("build a thing", &[agent("a", &["build"])]);
        assert!(p.contains("| goal | components | agents | topology |"), "no worked table");
        assert!(
            p.contains("| 4 | 2 | peer |"),
            "the table must show fewer agents than components — that is the lesson"
        );
        assert!(p.contains("Components are not agents."));
    }

    #[test]
    fn prompt_states_the_fleet_ceiling_and_its_real_capabilities() {
        let p = build_prompt("g", &[agent("a", &["build"]), agent("b", &["gpu-compute"])]);
        assert!(p.contains("2 agents available"), "the count is a ceiling worth stating");
        assert!(p.contains("build, gpu-compute"), "must list capabilities that can be satisfied");
        // With no fleet there is nothing true to say about one.
        assert_eq!(fleet_summary(&[]), "");
    }

    #[test]
    fn prompt_never_asks_the_model_for_the_count_or_the_topology() {
        let p = build_prompt("g", &[]);
        assert!(p.contains("Do not emit \"agent_count\" or \"topology\""));
        assert!(p.contains("why THIS many"), "§7 requires the reason, not the number");
    }
}
