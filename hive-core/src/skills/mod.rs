//! Skill system — TOML-defined skills with triggers, parameters, and an
//! execution policy.
//!
//! A skill is a *prompt-layer* construct: trigger patterns decide when it is
//! active, `system_prompt.md` supplies its guidance, `ai_provider` names the
//! model its reasoning should run on, and `require_confirmation` routes
//! everything the skill's plan produces through the existing approval gate —
//! the same Tier-1 machinery `hive task` and the web chat already use. There
//! is deliberately no second execution path: a skill that could run commands
//! outside the watchdog's gate would be a hole in Phase 10, and Phase 10
//! depends on this phase by design.
//!
//! Format (see `docs/implementation-plan.md` "Phase 5"):
//!
//! ```text
//! ~/.hive/skills/<name>/skill.toml   # metadata, triggers, parameters, execution
//! ~/.hive/skills/<name>/system_prompt.md  # optional guidance
//! ~/.hive/skills/<name>/scripts/     # optional helper scripts (referenced by
//!                                    # the prompt; never auto-executed)
//! ```

pub mod loader;

use hive_common::config::SkillsConfig;
use hive_common::protocol::AiProvider;
use serde_json::{json, Value};

use crate::llm::LlmRouter;

/// One loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Natural-language trigger words/phrases, matched case-insensitively on
    /// word boundaries.
    pub patterns: Vec<String>,
    pub parameters: Vec<SkillParameter>,
    /// The provider this skill's reasoning should run on, if it names one.
    pub ai_provider: Option<AiProvider>,
    /// Whether every command produced while this skill is active must pass
    /// the approval gate — even commands the Tier-1 rules would allow.
    pub require_confirmation: bool,
    /// Contents of `system_prompt.md`, if present.
    pub system_prompt: Option<String>,
    /// Whether the skill directory carries a `scripts/` folder. Noted in the
    /// rendered prompt so the model knows helpers exist; they are never
    /// executed by the skill system itself.
    pub has_scripts: bool,
    /// Where the skill was loaded from.
    pub dir: std::path::PathBuf,
}

/// A declared parameter, surfaced in tool definitions.
#[derive(Debug, Clone)]
pub struct SkillParameter {
    pub name: String,
    pub kind: String,
    pub required: bool,
    pub default: Option<String>,
    pub options: Vec<String>,
}

/// Registry of loaded skills.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

/// A skill rendered as an LLM tool definition.
#[derive(Debug, Clone)]
pub struct SkillToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON schema for the parameters object.
    pub parameters: Value,
}

impl SkillRegistry {
    /// An empty registry — the state before anything is loaded.
    pub fn new() -> Self {
        Self { skills: vec![] }
    }

    /// Load from the configured directory (`skills.directory`).
    pub fn load(config: &SkillsConfig) -> Self {
        let dir = loader::expand_home(&config.directory);
        let skills = loader::load_dir(&dir);
        tracing::info!(count = skills.len(), dir = %dir.display(), "skills loaded");
        Self { skills }
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Every skill whose trigger patterns match `input`.
    ///
    /// Matching is word-boundary, case-insensitive substring on each pattern
    /// — "deploy" must not fire on "deployment server log" without saying
    /// so, but "push to production" should match the phrase.
    pub fn pattern_matches(&self, input: &str) -> Vec<&Skill> {
        let haystack = input.to_lowercase();
        self.skills
            .iter()
            .filter(|s| {
                s.patterns.iter().any(|p| {
                    let p = p.to_lowercase();
                    match p.split_whitespace().count() {
                        0 => false,
                        // Single word: word-boundary match.
                        1 => word_boundary_contains(&haystack, &p),
                        // Phrase: substring match.
                        _ => haystack.contains(&p),
                    }
                })
            })
            .collect()
    }

    /// Resolve `input` to one skill: pattern match first, then — only when
    /// more than one skill matched — local-LLM disambiguation. The LLM is
    /// the *tiebreaker*, not the matcher: a deterministic match costs
    /// nothing and cannot hallucinate a skill that was not triggered.
    pub async fn resolve(&self, input: &str, llm: &LlmRouter) -> Option<&Skill> {
        let matches = self.pattern_matches(input);
        match matches.len() {
            0 => None,
            1 => Some(matches[0]),
            _ => {
                let names: Vec<&str> = matches.iter().map(|s| s.name.as_str()).collect();
                let prompt = format!(
                    "Which of these skills best matches the request? Reply with ONLY the \
                     skill name, nothing else.\n\nRequest: {input}\nSkills: {}\n",
                    names.join(", ")
                );
                match llm.local_complete(&prompt).await {
                    Ok(answer) => {
                        let cleaned = answer.trim().trim_matches('"').to_lowercase();
                        matches
                            .iter()
                            .find(|s| s.name.to_lowercase() == cleaned)
                            .copied()
                            // An unusable answer falls back to the first
                            // deterministic match rather than dropping the
                            // skill entirely — the triggers all fired.
                            .or_else(|| matches.first().copied())
                    }
                    Err(_) => matches.first().copied(),
                }
            }
        }
    }

    /// Skills as LLM tool definitions, for callers that expose them to a
    /// model as callable functions.
    pub fn to_tool_definitions(&self) -> Vec<SkillToolDefinition> {
        self.skills
            .iter()
            .map(|s| SkillToolDefinition {
                name: s.name.clone(),
                description: s.description.clone(),
                parameters: s.schema(),
            })
            .collect()
    }
}

fn word_boundary_contains(haystack: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(at) = haystack[start..].find(word) {
        let from = start + at;
        let to = from + word.len();
        let before_ok = from == 0
            || !haystack[..from]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let after_ok = to == haystack.len()
            || !haystack[to..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = to;
    }
    false
}

impl Skill {
    /// The JSON schema for this skill's parameters.
    pub fn schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for p in &self.parameters {
            let mut field = match p.kind.as_str() {
                "number" | "integer" => json!({ "type": "number" }),
                "boolean" | "bool" => json!({ "type": "boolean" }),
                _ => json!({ "type": "string" }),
            };
            if !p.options.is_empty() {
                field["enum"] = json!(p.options);
            }
            if let Some(d) = &p.default {
                field["default"] = json!(d);
            }
            if p.required {
                required.push(json!(p.name));
            }
            properties.insert(p.name.clone(), field);
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    /// How the skill is announced inside a planner prompt.
    pub fn render_for_prompt(&self) -> String {
        let mut out = format!(
            "An active skill governs this request: {} — {}",
            self.name, self.description
        );
        if !self.parameters.is_empty() {
            let params: Vec<String> = self
                .parameters
                .iter()
                .map(|p| {
                    let mut s = format!("{} ({})", p.name, p.kind);
                    if p.required {
                        s.push_str(", required");
                    }
                    if let Some(d) = &p.default {
                        s.push_str(&format!(", default {d}"));
                    }
                    s
                })
                .collect();
            out.push_str(&format!("\nParameters: {}", params.join("; ")));
        }
        if let Some(prompt) = &self.system_prompt {
            out.push_str(&format!("\nSkill guidance:\n{prompt}"));
        }
        if self.has_scripts {
            out.push_str(&format!(
                "\nHelper scripts exist at {} — reference them by absolute path if needed.",
                self.dir.join("scripts").display()
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, patterns: &[&str]) -> Skill {
        Skill {
            name: name.into(),
            description: format!("{name} skill"),
            version: "1.0".into(),
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
            parameters: vec![],
            ai_provider: None,
            require_confirmation: false,
            system_prompt: None,
            has_scripts: false,
            dir: std::path::PathBuf::from("/tmp"),
        }
    }

    fn registry(skills: Vec<Skill>) -> SkillRegistry {
        SkillRegistry { skills }
    }

    #[test]
    fn single_word_patterns_match_on_word_boundaries() {
        let reg = registry(vec![skill("deploy", &["deploy"])]);
        assert_eq!(reg.pattern_matches("please deploy the api").len(), 1);
        assert!(
            reg.pattern_matches("read the deployment server log").is_empty(),
            "'deploy' must not fire inside 'deployment'"
        );
        assert!(reg.pattern_matches("DEPLOY now!").len() == 1, "case-insensitive");
    }

    #[test]
    fn phrase_patterns_match_as_substrings() {
        let reg = registry(vec![skill("rel", &["push to production"])]);
        assert_eq!(
            reg.pattern_matches("ok, push to production now").len(),
            1
        );
        assert!(reg.pattern_matches("push to staging").is_empty());
    }

    #[test]
    fn tool_definitions_carry_schemas_with_enums_and_required() {
        let mut s = skill("deploy", &["deploy"]);
        s.parameters = vec![
            SkillParameter {
                name: "service_name".into(),
                kind: "string".into(),
                required: true,
                default: None,
                options: vec![],
            },
            SkillParameter {
                name: "target_env".into(),
                kind: "string".into(),
                required: false,
                default: Some("staging".into()),
                options: vec!["staging".into(), "production".into()],
            },
        ];
        let reg = registry(vec![s]);
        let defs = reg.to_tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].parameters["required"][0], "service_name");
        assert_eq!(defs[0].parameters["properties"]["target_env"]["enum"][0], "staging");
    }

    #[test]
    fn prompt_rendering_mentions_guidance_and_scripts() {
        let mut s = skill("deploy", &["deploy"]);
        s.system_prompt = Some("Always --dry-run first.".into());
        s.has_scripts = true;
        let rendered = s.render_for_prompt();
        assert!(rendered.contains("deploy — deploy skill"));
        assert!(rendered.contains("Always --dry-run first."));
        assert!(rendered.contains("scripts"));
    }
}
