//! Task planner — decomposes user requests into subtasks via the LLM router.

use hive_common::{AiProvider, Complexity};
use serde::{Deserialize, Serialize};

use crate::llm::LlmRouter;

/// A planned task with subtasks ready for delegation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    /// High-level summary of the plan.
    pub summary: String,
    /// Individual subtasks to execute.
    pub subtasks: Vec<SubTask>,
    /// Which provider actually produced this plan. This is not always the one
    /// the complexity router asked for — an unconfigured or failing cloud
    /// provider falls back to the local model, and callers that display a
    /// model badge need the truth rather than the intent.
    #[serde(default = "default_provider")]
    pub provider_used: AiProvider,
}

/// The planner deserializes `TaskPlan` straight from model output, which never
/// contains this field — it is stamped on afterwards from the LLM response.
fn default_provider() -> AiProvider {
    AiProvider::Local
}

/// What the planner is told about the machines it is planning for.
///
/// Without this the model has no idea whether it is writing commands for macOS
/// or Linux, and guesses — which in practice means GNU-only flags
/// (`ps --sort=`, `find -printf`, `stat -c`) that fail silently on the master.
/// Measured on qwen2.5:14b, supplying this took wrong-OS commands from 3/12
/// to 0/12. The text comes from the machine knowledge graph, so it stays
/// accurate as the fleet changes.
#[derive(Debug, Clone, Default)]
pub struct FleetContext {
    /// Name of the machine local subtasks run on.
    pub local_machine: String,
    /// OS family of that machine (`macos`, `ubuntu`, …), from the graph.
    pub local_os: String,
    /// Fleet description, as rendered by `memory::machines::describe_for_prompt`.
    pub description: String,
}

impl FleetContext {
    /// Empty context — used where no graph is available; the planner then
    /// behaves as it did before the graph existed.
    pub fn none() -> Self {
        Self::default()
    }

    /// The fleet listing, placed before the request so the model can choose a
    /// machine.
    fn header(&self) -> String {
        if self.description.trim().is_empty() {
            return String::new();
        }
        format!(
            "{}\nCommands with requires_remote=false run on '{}'.\n\n",
            self.description.trim(),
            self.local_machine
        )
    }

    /// The OS constraint, placed *after* the schema.
    ///
    /// Position, specificity, and worked examples all matter, and the effect
    /// is large. Measured on qwen3.5:9b over 12 plans each:
    ///
    /// | prompt | broken commands |
    /// |---|---|
    /// | constraint at the top of the prompt | 3/12 |
    /// | terse constraint at the end | 9/12 |
    /// | forbid GNU flags at the end | 5/12 |
    /// | forbid GNU flags **and show BSD equivalents** | 0/12 |
    ///
    /// Telling the model what not to write is not enough — it needs the
    /// replacement it should reach for instead. Restating which machine local
    /// commands land on also matters, since the fleet listing above may name
    /// hosts running a different OS.
    fn trailer(&self) -> String {
        let os = self.local_os.to_ascii_lowercase();
        if os.is_empty() {
            return String::new();
        }
        let machine = &self.local_machine;
        if os.contains("mac") || os.contains("darwin") {
            format!(
                "\nCRITICAL: requires_remote=false commands run on {machine}, which is \
                 macOS (BSD userland), NOT Linux — even if a Linux machine appears above. \
                 GNU-only flags such as `find -printf`, `ps --sort=`, `top -b`, `stat -c` \
                 and `du --max-depth` DO NOT EXIST on macOS and will fail. Use BSD \
                 equivalents, for example: `ps -A -o pid,rss,comm | sort -nrk2`, \
                 `find ~ -type f -exec stat -f '%z %N' {{}} +`, `top -l 1 -o cpu`.\n"
            )
        } else {
            format!(
                "\nCRITICAL: requires_remote=false commands run on {machine}, which is \
                 {os} (GNU/Linux userland), NOT macOS — even if a macOS machine appears \
                 above. BSD-only flags such as `stat -f`, `sed -i ''`, `du -d` and \
                 `top -l` DO NOT EXIST there and will fail. Use GNU equivalents, for \
                 example: `ps -eo pid,rss,comm --sort=-rss`, `find ~ -type f -printf \
                 '%s %p\\n'`, `top -b -n1`.\n"
            )
        }
    }
}

/// A single subtask within a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    /// Description of what this subtask does.
    pub description: String,
    /// Whether this subtask requires remote execution (on a worker).
    #[serde(default)]
    pub requires_remote: bool,
    /// Commands to execute.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Expected behavior (for the watchdog, once Phase 10 lands).
    #[serde(default)]
    pub expected_behavior: Option<String>,
    /// Capabilities the target machine must have, e.g. `gpu-compute`.
    ///
    /// This is what connects the machine knowledge graph to actual placement.
    /// Without it every remote subtask asked for the same fixed capability, so
    /// a GPU job and a `wc -l` were routed identically — the graph knew which
    /// box had the A6000s and was never asked.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

/// Decomposes user requests into a `TaskPlan` via the LLM router.
pub struct Planner;

impl Planner {
    pub fn new() -> Self {
        Self
    }

    /// Ask the LLM to decompose `user_input` into a plan, using the
    /// provider recommended for `complexity`. Falls back to a single
    /// no-op subtask (no commands — nothing is assumed safe to run without
    /// a real plan) if the response isn't parseable JSON.
    pub async fn plan(
        &self,
        llm: &LlmRouter,
        user_input: &str,
        complexity: Complexity,
        fleet: &FleetContext,
    ) -> anyhow::Result<TaskPlan> {
        let fleet_header = fleet.header();
        let fleet_trailer = fleet.trailer();
        let prompt = format!(
            "You are a task planner for a distributed agent system. Decompose the \
             following request into a short JSON plan.\n\n\
             {fleet_header}\
             Request: {user_input}\n\n\
             Respond with ONLY a JSON object of this exact shape, no prose, no markdown fences:\n\
             {{\n  \
               \"summary\": \"one-sentence summary of the plan\",\n  \
               \"subtasks\": [\n    \
                 {{\n      \
                   \"description\": \"what this subtask does\",\n      \
                   \"requires_remote\": false,\n      \
                   \"commands\": [\"shell command\"],\n      \
                   \"expected_behavior\": \"what success looks like, for safety monitoring\",\n      \
                   \"required_capabilities\": []\n    \
                 }}\n  \
               ]\n\
             }}\n\n\
             Set required_capabilities only when the work genuinely needs them, choosing \
             from: gpu-compute (CUDA/GPU work), local-inference (running a local LLM), \
             containers (docker), build (compiling), database, batch-scheduler. Leave it \
             empty for ordinary shell commands — an unnecessary requirement can leave a \
             task unplaceable.\n\
             Use requires_remote=true only if the task must run on a separate worker \
             machine. When you do, write the command exactly as it should run ON that \
             machine — do NOT wrap it in ssh, and do not name the machine in the command. \
             Hive opens the connection for you; an `ssh worker-name ...` command fails, \
             because those names are Hive's, not DNS. Keep commands minimal and only include ones you are confident are \
             correct and safe. If the request doesn't need any commands, use an empty \
             commands array.\n\
             {fleet_trailer}"
        );

        let response = llm.route_and_execute(&prompt, complexity).await?;

        match extract_plan(&response.text) {
            Ok(mut plan) => {
                plan.provider_used = response.provider;
                Ok(plan)
            }
            Err(e) => {
                tracing::warn!(
                    "Planner couldn't parse a JSON plan ({e}), falling back to a single \
                     no-op subtask for: {user_input}"
                );
                Ok(TaskPlan {
                    summary: format!("Received: {user_input}"),
                    subtasks: vec![SubTask {
                        description: user_input.to_string(),
                        requires_remote: false,
                        commands: vec![],
                        expected_behavior: None,
                        required_capabilities: vec![],
                    }],
                    provider_used: response.provider,
                })
            }
        }
    }
}

impl Default for Planner {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract a `TaskPlan` from an LLM response that may be wrapped in prose or
/// markdown code fences.
fn extract_plan(text: &str) -> anyhow::Result<TaskPlan> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in response"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in response"))?;
    if end < start {
        anyhow::bail!("malformed JSON in response");
    }
    let json_str = &text[start..=end];
    let plan: TaskPlan = serde_json::from_str(json_str)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_plan_from_clean_json() {
        let text = r#"{"summary":"do a thing","subtasks":[{"description":"step 1","commands":["echo hi"]}]}"#;
        let plan = extract_plan(text).unwrap();
        assert_eq!(plan.summary, "do a thing");
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.subtasks[0].commands, vec!["echo hi"]);
        assert!(!plan.subtasks[0].requires_remote);
    }

    #[test]
    fn extract_plan_from_markdown_fenced_json() {
        let text = "Sure, here's the plan:\n```json\n{\"summary\":\"s\",\"subtasks\":[]}\n```\nLet me know!";
        let plan = extract_plan(text).unwrap();
        assert_eq!(plan.summary, "s");
        assert!(plan.subtasks.is_empty());
    }

    fn ctx(os: &str) -> FleetContext {
        FleetContext {
            local_machine: "manus-mac-mini".into(),
            local_os: os.into(),
            description: "Known machines:\n- manus-mac-mini (online): macos-26.5.2, 10 cores."
                .into(),
        }
    }

    #[test]
    fn header_lists_the_fleet_and_names_the_local_machine() {
        let h = ctx("macos").header();
        assert!(h.contains("macos-26.5.2"));
        assert!(h.contains("run on 'manus-mac-mini'"));
    }

    #[test]
    fn trailer_warns_about_the_right_userland_and_offers_replacements() {
        let mac = ctx("macos").trailer();
        assert!(mac.contains("BSD userland"));
        assert!(mac.contains("manus-mac-mini"), "must name the machine commands land on");
        assert!(mac.contains("find -printf"), "must enumerate the forbidden GNU flags");
        assert!(mac.contains("stat -f"), "must offer the BSD replacement, not just a ban");

        let linux = ctx("ubuntu").trailer();
        assert!(linux.contains("GNU/Linux userland"));
        assert!(linux.contains("stat -f"), "must name the forbidden BSD flag");
        assert!(linux.contains("--sort=-rss"), "must offer the GNU replacement");
    }

    #[test]
    fn empty_fleet_context_adds_nothing_to_the_prompt() {
        // No graph, no fabricated machine facts — the planner should just see
        // the request, exactly as it did before the graph existed.
        assert_eq!(FleetContext::none().header(), "");
        assert_eq!(FleetContext::none().trailer(), "");
        assert_eq!(
            FleetContext {
                local_machine: "x".into(),
                local_os: String::new(),
                description: "   ".into()
            }
            .header(),
            ""
        );
    }

    #[test]
    fn extracted_plan_defaults_provider_then_is_stamped_with_the_real_one() {
        // Model output never carries `provider_used`; it must deserialize
        // anyway, and the caller overwrites it with whoever actually answered.
        let mut plan = extract_plan(r#"{"summary":"s","subtasks":[]}"#).expect("parses");
        assert_eq!(plan.provider_used, AiProvider::Local);
        plan.provider_used = AiProvider::Claude;
        assert_eq!(plan.provider_used, AiProvider::Claude);
    }

    #[test]
    fn extract_plan_rejects_non_json() {
        assert!(extract_plan("I refuse to answer in JSON.").is_err());
    }
}
