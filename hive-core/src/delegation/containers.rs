//! Docker containers used as delegation devices.
//!
//! A container is reached through the machine that runs it: every command is
//! wrapped in `docker exec` there, over the same SSH (or local shell) path the
//! machine itself uses. Only delegation, sessions and terminals know about
//! containers; the SSH worker pool, chat-plan execution and incidents never
//! receive one, so nothing can silently run on the host instead.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Container {
    /// The device name agents and people use for it.
    pub name: String,
    /// The fleet machine (or the coordinator) whose Docker runs it.
    pub host: String,
    /// Docker's name for the container on that machine.
    pub container: String,
    /// Hive created it and may remove it; otherwise Hive only unregisters it.
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub image: Option<String>,
}

fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HIVE_CONTAINERS_FILE") {
        return Some(path.into());
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".hive/containers.json"))
}

/// Registered containers. A missing or unreadable file means none.
pub fn load() -> Vec<Container> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(all: &[Container]) -> anyhow::Result<()> {
    let path = path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    std::fs::create_dir_all(path.parent().expect("nested path"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_string_pretty(all)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

/// Device and Docker names go into shell commands and tmux targets.
pub fn valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Register a container. `taken` names every existing device, so a container
/// can never shadow a machine (the fleet wins every lookup).
pub fn add(entry: Container, taken: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(valid_name(&entry.name), "Use letters, numbers, '-', '_' or '.' for the name");
    anyhow::ensure!(valid_name(&entry.container), "Invalid container name");
    anyhow::ensure!(
        !taken.iter().any(|t| t == &entry.name),
        "A machine named {} already exists",
        entry.name
    );
    let mut all = load();
    anyhow::ensure!(
        !all.iter().any(|c| c.name == entry.name),
        "A container named {} is already added",
        entry.name
    );
    anyhow::ensure!(
        !all.iter().any(|c| c.host == entry.host && c.container == entry.container),
        "{} on {} is already added",
        entry.container,
        entry.host
    );
    all.push(entry);
    save(&all)
}

/// Unregister a container, returning it. Never touches Docker.
pub fn remove(name: &str) -> anyhow::Result<Container> {
    let mut all = load();
    let index = all
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| anyhow::anyhow!("No container named {name}"))?;
    let removed = all.remove(index);
    save(&all)?;
    Ok(removed)
}

/// Create a Hive-managed container named `name` on `machine` (a fleet machine
/// or the coordinator, never a container) from Hive's agent base image, with
/// the machine's agent logins mounted in, and register it with `managed: true`.
/// `taken` is every existing device name. Only Hive's own planner calls this.
pub async fn create(
    machine: &hive_common::protocol::WorkerInfo,
    name: &str,
    taken: &[String],
) -> anyhow::Result<Container> {
    let _ = (machine, name, taken);
    anyhow::bail!("Creating containers is not implemented yet")
}

/// Run `command` inside the container, as a login shell so the image's PATH
/// applies. `-i` keeps stdin: the runner reads its JSON input from it.
pub fn exec(container: &str, command: &str, tty: bool) -> String {
    format!(
        "docker exec {} {} sh -lc {}",
        if tty { "-it" } else { "-i" },
        super::transport::quote(container),
        super::transport::quote(command)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_reach_a_shell_are_plain() {
        assert!(valid_name("dev-box_1.test"));
        for bad in ["", "-flag", "a b", "a;rm", "a$(x)", "a'b", &"x".repeat(64)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn exec_quotes_the_command_as_one_argument() {
        let line = exec("box", "echo 'hi'; tmux ls", false);
        assert_eq!(line, r#"docker exec -i 'box' sh -lc 'echo '"'"'hi'"'"'; tmux ls'"#);
        assert!(exec("box", "x", true).starts_with("docker exec -it "));
    }
}
