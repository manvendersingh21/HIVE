//! Task planner — decomposes user requests into subtasks via the LLM router.

use hive_common::Complexity;
use serde::{Deserialize, Serialize};

use crate::llm::LlmRouter;

/// A planned task with subtasks ready for delegation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    /// High-level summary of the plan.
    pub summary: String,
    /// Individual subtasks to execute.
    pub subtasks: Vec<SubTask>,
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
    ) -> anyhow::Result<TaskPlan> {
        let prompt = format!(
            "You are a task planner for a distributed agent system. Decompose the \
             following request into a short JSON plan.\n\n\
             Request: {user_input}\n\n\
             Respond with ONLY a JSON object of this exact shape, no prose, no markdown fences:\n\
             {{\n  \
               \"summary\": \"one-sentence summary of the plan\",\n  \
               \"subtasks\": [\n    \
                 {{\n      \
                   \"description\": \"what this subtask does\",\n      \
                   \"requires_remote\": false,\n      \
                   \"commands\": [\"shell command\"],\n      \
                   \"expected_behavior\": \"what success looks like, for safety monitoring\"\n    \
                 }}\n  \
               ]\n\
             }}\n\n\
             Use requires_remote=true only if the task must run on a separate worker \
             machine. Keep commands minimal and only include ones you are confident are \
             correct and safe. If the request doesn't need any commands, use an empty \
             commands array."
        );

        let response = llm.route_and_execute(&prompt, complexity).await?;

        match extract_plan(&response.text) {
            Ok(plan) => Ok(plan),
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
                    }],
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

    #[test]
    fn extract_plan_rejects_non_json() {
        assert!(extract_plan("I refuse to answer in JSON.").is_err());
    }
}
