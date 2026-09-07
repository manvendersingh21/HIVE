//! Which stock CLI runs a role, and how to start it non-interactively.
//!
//! Every invocation here was measured, not guessed. They come from the Phase S smoke
//! and the live runs recorded in `docs/findings/adapter-edge.md`, and each flag is
//! there because leaving it out produced a specific, observed failure:
//!
//! * **`codex` needs `--sandbox workspace-write`.** Its default sandbox is read-only,
//!   and a blocked run still exits 0 having written nothing (finding 1–2). It also
//!   needs `--skip-git-repo-check` because a run workspace is a scratch directory, not
//!   a repository.
//! * **`agy` takes its flags before `--print`.** `--print` consumes the next token as
//!   its prompt, so a flag placed after it is swallowed as part of the task.
//! * **Permission bypass is required on all of them.** A non-interactive session has no
//!   one to answer a prompt; without the flag the CLI blocks until the timeout.
//!
//! The mapping from an agent URN to the tool behind it lives here and in the run
//! record — never on the wire. Spec §3 forbids a peer learning which vendor runs a
//! role, and [`crate::runtime::brief`] has a test that greps every rendered brief for
//! every name in this table.

use std::path::Path;

/// One stock agentic CLI, and the argv that puts it in non-interactive mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCli {
    /// The program name as installed on PATH.
    pub name: &'static str,
}

/// Every CLI this runtime knows how to drive.
pub const KNOWN: &[&str] = &["claude", "codex", "agy", "opencode"];

impl AgentCli {
    /// OpenCode scopes completion events to the conversation's original project
    /// directory. Moving its client into a repair subdirectory can lose those
    /// events even though the model writes its verdict. Output paths remain
    /// explicit in the brief; they need not equal the client's project directory.
    pub fn working_directory<'a>(&self, output: &'a Path, conversation_root: &'a Path,
        conversation: Option<&str>) -> &'a Path {
        if self.name == "opencode" && conversation.is_some() { conversation_root } else { output }
    }

    /// Look up a CLI by name. An unknown name is an error rather than a guess: an
    /// invented invocation would fail inside the pane, where it reads exactly like an
    /// agent that produced no output.
    pub fn resolve(name: &str) -> anyhow::Result<Self> {
        match KNOWN.iter().find(|k| **k == name) {
            Some(k) => Ok(Self { name: k }),
            None => anyhow::bail!(
                "unknown agent CLI '{name}'; this runtime knows: {}",
                KNOWN.join(", ")
            ),
        }
    }

    /// The arguments that precede the brief. The brief itself is passed as the final
    /// positional argument by [`crate::collab::SessionSpec`], shell-quoted by the
    /// session host — which is why none of these end in a flag that would eat it.
    pub fn args(&self, cwd: &Path) -> Vec<String> {
        let cwd = cwd.to_string_lossy().to_string();
        match self.name {
            "codex" => vec![
                "exec".into(),
                "--skip-git-repo-check".into(),
                "--sandbox".into(),
                "workspace-write".into(),
                "-C".into(),
                cwd,
            ],
            "agy" => vec![
                "--add-dir".into(),
                cwd,
                "--dangerously-skip-permissions".into(),
                "--print".into(),
            ],
            "claude" => vec!["-p".into(), "--dangerously-skip-permissions".into()],
            "opencode" => vec!["run".into()],
            // `resolve` is the only constructor and it refuses anything else.
            other => unreachable!("unregistered CLI '{other}' reached args()"),
        }
    }

    pub fn args_with_model(&self, cwd: &Path, model: Option<&str>) -> anyhow::Result<Vec<String>> {
        let mut args = self.args(cwd);
        if let Some(model) = model {
            anyhow::ensure!(!model.trim().is_empty() && !model.contains(['\n', '\r', '\0']), "invalid model identifier");
            // AGY's --print consumes the next token, which must remain the prompt.
            let at = if self.name == "agy" { args.len()-1 } else { args.len() };
            args.splice(at..at, ["--model".into(), model.into()]);
        }
        Ok(args)
    }

    pub fn call_args(&self, cwd: &Path, model: Option<&str>, conversation: Option<&str>) -> anyhow::Result<Vec<String>> {
        let mut args = self.args_with_model(cwd, model)?;
        let mut extra = Vec::new();
        match self.name {
            "opencode" => {
                extra.extend(["--format".into(), "json".into(), "--auto".into()]);
                if let Some(id) = conversation { extra.extend(["--session".into(), id.into()]); }
            }
            "agy" => {
                extra.extend(["--output-format".into(), "json".into()]);
                if let Some(id) = conversation { extra.extend(["--conversation".into(), id.into()]); }
            }
            _ => anyhow::ensure!(conversation.is_none(), "conversation recovery is not supported for this CLI"),
        }
        let at = if self.name == "agy" { args.len()-1 } else { args.len() };
        args.splice(at..at, extra);
        Ok(args)
    }

    /// Only vendor metadata at the top level of a structured CLI event counts.
    /// Never scrape IDs out of an agent's prose or a tool-result string.
    pub fn conversation_from_log(&self, log: &str) -> anyhow::Result<Option<String>> {
        let key = match self.name { "opencode" => "sessionID", "agy" => "conversation_id", _ => return Ok(None) };
        let mut values = Vec::new();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(log) { values.push(value); }
        else {
            values.extend(log.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for value in values {
            if let Some(id) = value[key].as_str() {
                let valid = if self.name == "opencode" {
                    id.starts_with("ses_") && id.len() > 8 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                } else { uuid::Uuid::parse_str(id).is_ok() };
                anyhow::ensure!(valid, "invalid conversation metadata returned by CLI");
                ids.insert(id.to_string());
            }
        }
        anyhow::ensure!(ids.len() <= 1, "CLI log names conflicting conversation identities");
        Ok(ids.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_project_stays_stable_while_attempt_outputs_move() {
        let root = Path::new("/tmp/run/sup");
        let repair = root.join("attempt-2");
        let cli = AgentCli::resolve("opencode").unwrap();
        assert_eq!(cli.working_directory(&repair, root, Some("ses_example")), root);
        assert_eq!(cli.working_directory(&repair, root, None), repair);
        assert_eq!(AgentCli::resolve("agy").unwrap().working_directory(&repair, root, Some("id")), repair);
    }
    use std::path::PathBuf;

    #[test]
    fn explicit_model_is_retained_without_consuming_the_prompt() {
        for name in KNOWN {
            let args = AgentCli::resolve(name).unwrap().args_with_model(Path::new("/tmp/work"), Some("exact/model")).unwrap();
            assert!(args.windows(2).any(|a| a == ["--model", "exact/model"]));
            if *name == "agy" { assert_eq!(args.last().unwrap(), "--print"); }
        }
    }

    #[test]
    fn continuation_uses_explicit_metadata_never_last_session_or_embedded_prose() {
        let cli = AgentCli::resolve("opencode").unwrap();
        assert_eq!(cli.conversation_from_log("{\"sessionID\":\"ses_abcdef123\",\"type\":\"text\"}\n").unwrap().as_deref(), Some("ses_abcdef123"));
        assert!(cli.conversation_from_log("{\"text\":\"sessionID: ses_fake123\"}").unwrap().is_none());
        assert!(cli.conversation_from_log("{\"sessionID\":\"ses_abcdef123\"}\n{\"sessionID\":\"ses_other123\"}").is_err());
        let args = cli.call_args(Path::new("/tmp/work"), None, Some("ses_abcdef123")).unwrap();
        assert!(args.windows(2).any(|a| a == ["--session", "ses_abcdef123"]));
        assert!(!args.contains(&"--continue".into()));
        let cli = AgentCli::resolve("agy").unwrap();
        let id = "e54cb477-bc27-4ab6-9880-94f096ba6e45";
        assert_eq!(cli.conversation_from_log(&format!("{{\"conversation_id\":\"{id}\"}}")).unwrap().as_deref(), Some(id));
        let args = cli.call_args(Path::new("/tmp/work"), None, Some(id)).unwrap();
        assert!(args.windows(2).any(|a| a == ["--conversation", id]));
        assert_eq!(args.last().unwrap(), "--print");
    }

    #[test]
    fn an_unknown_cli_is_refused_rather_than_guessed() {
        let e = AgentCli::resolve("gpt-cli").unwrap_err().to_string();
        assert!(e.contains("unknown agent CLI"), "{e}");
        assert!(e.contains("claude"), "the error should list what is known: {e}");
    }

    #[test]
    fn every_known_cli_resolves_and_yields_args() {
        for name in KNOWN {
            let cli = AgentCli::resolve(name).unwrap();
            let args = cli.args(&PathBuf::from("/tmp/ws"));
            assert!(!args.is_empty(), "{name} produced no arguments");
        }
    }

    #[test]
    fn codex_gets_a_writable_sandbox() {
        // Finding 1: the default sandbox is read-only and a blocked run exits 0.
        // Without this flag the whole file edge silently produces nothing.
        let args = AgentCli::resolve("codex").unwrap().args(&PathBuf::from("/tmp/ws"));
        let joined = args.join(" ");
        assert!(joined.contains("--sandbox workspace-write"), "{joined}");
        assert!(joined.contains("--skip-git-repo-check"), "{joined}");
    }

    #[test]
    fn the_brief_lands_as_the_prompt_on_every_cli() {
        // The session host appends the brief as the final positional argument. What
        // must not happen is a trailing flag that takes a *value*, which would swallow
        // it and hand the agent an empty task. There is no generic way to detect that
        // from a string, so the terminal token of each invocation is pinned by name,
        // with why it is safe:
        let expected: &[(&str, &str)] = &[
            // a boolean flag; the next token is the prompt
            ("claude", "--dangerously-skip-permissions"),
            // `-C <dir>` is complete, so the next token is the prompt
            ("codex", "/tmp/ws"),
            // `--print` deliberately takes the prompt itself
            ("agy", "--print"),
            // `run <prompt>`
            ("opencode", "run"),
        ];
        for (name, last) in expected {
            let args = AgentCli::resolve(name).unwrap().args(&PathBuf::from("/tmp/ws"));
            assert_eq!(
                args.last().map(String::as_str),
                Some(*last),
                "{name}'s invocation changed shape; re-check that the brief still lands \
                 as the prompt before updating this"
            );
        }
        // And every registered CLI is covered, so adding one forces this decision.
        assert_eq!(expected.len(), KNOWN.len());
    }

    #[test]
    fn the_workspace_is_named_where_the_cli_needs_it() {
        let ws = PathBuf::from("/tmp/run/wrk");
        for name in ["codex", "agy"] {
            let args = AgentCli::resolve(name).unwrap().args(&ws);
            assert!(
                args.iter().any(|a| a == "/tmp/run/wrk"),
                "{name} was not told where to work: {args:?}"
            );
        }
    }
}
