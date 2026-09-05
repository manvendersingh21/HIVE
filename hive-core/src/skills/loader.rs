//! Skill loader — turn `~/.hive/skills/<name>/skill.toml` into [`Skill`]s.
//!
//! A malformed skill is skipped with a warning naming the file, never an
//! error: one broken skill in a user-writable directory must not take the
//! whole registry (and therefore every agent request) down with it.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use hive_common::protocol::AiProvider;

use super::Skill;

/// The parsed shape of `skill.toml` (see `docs/implementation-plan.md`
/// "Phase 5" for the file format's origin).
#[derive(Debug, Deserialize)]
struct SkillFile {
    skill: SkillMeta,
    #[serde(default)]
    trigger: Trigger,
    #[serde(default)]
    parameters: std::collections::BTreeMap<String, ParameterSpec>,
    #[serde(default)]
    execution: Execution,
}

#[derive(Debug, Deserialize)]
struct SkillMeta {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Default, Deserialize)]
struct Trigger {
    #[serde(default)]
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ParameterSpec {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    options: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Execution {
    #[serde(default)]
    ai_provider: Option<String>,
    #[serde(default)]
    require_confirmation: bool,
}

/// Parse one skill directory: `skill.toml` required, `system_prompt.md`
/// optional beside it, `scripts/` noted for the prompt.
pub fn load_skill_dir(dir: &Path) -> anyhow::Result<Skill> {
    let toml_path = dir.join("skill.toml");
    let raw = std::fs::read_to_string(&toml_path).map_err(|e| {
        anyhow::anyhow!("{}: {e}", toml_path.display())
    })?;
    let file: SkillFile = toml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("{}: {e}", toml_path.display()))?;

    let system_prompt = std::fs::read_to_string(dir.join("system_prompt.md"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let has_scripts = dir.join("scripts").is_dir();

    Ok(Skill {
        name: file.skill.name,
        description: file.skill.description,
        version: file.skill.version,
        patterns: file.trigger.patterns,
        parameters: file
            .parameters
            .into_iter()
            .map(|(name, spec)| super::SkillParameter {
                name,
                kind: if spec.kind.is_empty() { "string".into() } else { spec.kind },
                required: spec.required,
                default: spec.default,
                options: spec.options,
            })
            .collect(),
        ai_provider: file.execution.ai_provider.as_deref().and_then(parse_provider),
        require_confirmation: file.execution.require_confirmation,
        system_prompt,
        has_scripts,
        dir: dir.to_path_buf(),
    })
}

/// Provider names as written in skill.toml. Unknown names are `None` with a
/// warning rather than an error — the skill still works, it just loses its
/// override — because the trigger words, prompt, and confirmation gate do
/// not depend on the provider at all.
fn parse_provider(raw: &str) -> Option<AiProvider> {
    match raw.trim().to_lowercase().as_str() {
        "local" | "ollama" => Some(AiProvider::Local),
        "gemini" | "gemini-flash" => Some(AiProvider::GeminiFlash),
        "claude" | "anthropic" => Some(AiProvider::Claude),
        "codex" | "openai" | "gpt" => Some(AiProvider::Codex),
        other => {
            tracing::warn!(provider = %other, "unknown ai_provider in skill.toml; ignoring override");
            None
        }
    }
}

/// Load every skill under `dir`, skipping broken entries with a warning.
/// Returns paths of the skills that loaded, in name order.
pub fn load_dir(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let mut skills = Vec::new();
    for d in dirs {
        match load_skill_dir(&d) {
            Ok(skill) => skills.push(skill),
            Err(e) => tracing::warn!(error = %e, "skipping unloadable skill directory"),
        }
    }
    skills
}

/// Expand a leading `~` — `skills.directory` defaults to `~/.hive/skills`
/// and TOML has no notion of home.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, toml: &str, prompt: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("skill.toml"), toml).unwrap();
        if let Some(p) = prompt {
            std::fs::write(dir.join("system_prompt.md"), p).unwrap();
        }
    }

    #[test]
    fn a_full_skill_toml_parses_with_every_field() {
        let root = std::env::temp_dir().join(format!("hive-skill-{}", std::process::id()));
        let dir = root.join("deploy_service");
        write_skill(
            &dir,
            r#"
[skill]
name = "deploy_service"
description = "Deploy a service to a target machine"
version = "1.0.0"

[trigger]
patterns = ["deploy", "push to production", "release"]

[parameters]
service_name = { type = "string", required = true }
target_env = { type = "string", default = "staging", options = ["staging", "production"] }

[execution]
ai_provider = "claude"
require_confirmation = true
"#,
            Some("Always deploy with --dry-run first."),
        );
        let skill = load_skill_dir(&dir).unwrap();
        assert_eq!(skill.name, "deploy_service");
        assert_eq!(skill.patterns.len(), 3);
        assert_eq!(skill.parameters.len(), 2);
        assert_eq!(skill.ai_provider, Some(AiProvider::Claude));
        assert!(skill.require_confirmation);
        assert_eq!(
            skill.system_prompt.as_deref(),
            Some("Always deploy with --dry-run first.")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn defaults_are_sane_for_a_minimal_skill() {
        let root = std::env::temp_dir().join(format!("hive-skill-min-{}", std::process::id()));
        let dir = root.join("tiny");
        write_skill(
            &dir,
            r#"
[skill]
name = "tiny"
description = "does a thing"
"#,
            None,
        );
        let skill = load_skill_dir(&dir).unwrap();
        assert!(skill.patterns.is_empty());
        assert!(!skill.require_confirmation);
        assert_eq!(skill.ai_provider, None);
        assert!(skill.system_prompt.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn broken_skills_are_skipped_not_fatal() {
        let root = std::env::temp_dir().join(format!("hive-skill-broken-{}", std::process::id()));
        write_skill(&root.join("good"), "[skill]\nname = \"good\"\ndescription = \"d\"\n", None);
        std::fs::create_dir_all(root.join("bad")).unwrap();
        std::fs::write(root.join("bad/skill.toml"), "not = [valid").unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();

        let skills = load_dir(&root);
        assert_eq!(skills.len(), 1, "good loads, bad toml and empty dir skip");
        assert_eq!(skills[0].name, "good");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unknown_providers_warn_and_lose_the_override_but_keep_the_skill() {
        let root = std::env::temp_dir().join(format!("hive-skill-prov-{}", std::process::id()));
        let dir = root.join("s");
        write_skill(
            &dir,
            "[skill]\nname = \"s\"\ndescription = \"d\"\n\n[execution]\nai_provider = \"gpt5-turbo\"\n",
            None,
        );
        let skill = load_skill_dir(&dir).unwrap();
        assert_eq!(skill.ai_provider, None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn home_expansion_only_touches_a_leading_tilde() {
        assert!(expand_home("~/.hive/skills").ends_with(".hive/skills"));
        assert_eq!(expand_home("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_home("rel/path"), PathBuf::from("rel/path"));
    }
}
