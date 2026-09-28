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

/// Hive's agent base image, built on each Docker machine from this file.
const DOCKERFILE: &str = include_str!("runner/Dockerfile.agent");

/// Login folders shared with containers Hive creates, relative to `$HOME`.
/// Folders, never single files: token refreshes replace files atomically,
/// which a single-file bind mount can't follow. `~/.claude.json` stays out:
/// the machine's own Claude rewrites it constantly.
const LOGINS: [&str; 3] = [".claude", ".codex", ".local/share/opencode"];

/// The base image's tag, named by its content so an edit builds a new image.
pub fn image_tag() -> String {
    use sha2::{Digest, Sha256};
    let hash = format!("{:x}", Sha256::digest(DOCKERFILE.as_bytes()));
    format!("hive-agent:{}", &hash[..12])
}

/// What `create` learned about the machine before starting the container.
struct MachineHome {
    home: String,
    user: String,
    logins: Vec<String>,
}

fn parse_home(out: &str) -> anyhow::Result<MachineHome> {
    let field = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Could not read {key} on the machine"))
    };
    let user = field("user=")?;
    anyhow::ensure!(
        user.split(':').count() == 2
            && user.split(':').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "Unexpected uid:gid {user}"
    );
    Ok(MachineHome {
        home: field("home=")?,
        user,
        logins: out
            .lines()
            .filter_map(|l| l.strip_prefix("login="))
            .filter(|l| LOGINS.contains(l))
            .map(str::to_string)
            .collect(),
    })
}

/// `docker run` for a managed container. Every argument is Hive's own: the
/// planner names the container and nothing else.
fn run_command(docker_name: &str, tag: &str, machine: &MachineHome) -> String {
    use super::transport::quote;
    let mut args = vec![
        "docker run -d".to_string(),
        format!("--name {}", quote(docker_name)),
        "--restart unless-stopped".into(),
        format!("--user {}", quote(&machine.user)),
        "--label hive.managed=1".into(),
    ];
    for login in &machine.logins {
        args.push(format!(
            "-v {}",
            quote(&format!("{}/{login}:/home/hive/{login}", machine.home))
        ));
    }
    args.push(quote(tag));
    // Register the uid (see Dockerfile.agent), give Claude its own config file
    // (it reads its login from the mounted ~/.claude, but won't start without
    // ~/.claude.json), then idle; agents come in through `docker exec`.
    args.push(format!(
        "sh -c {}",
        quote(r#"grep -q "^[^:]*:x:$(id -u):" /etc/passwd || echo "hive:x:$(id -u):$(id -g)::/home/hive:/bin/sh" >> /etc/passwd; [ -e ~/.claude.json ] || echo '{"hasCompletedOnboarding":true}' > ~/.claude.json; exec sleep infinity"#)
    ));
    args.join(" ")
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
    use super::transport::{quote, ssh_timeout};
    anyhow::ensure!(machine.container.is_none(), "Containers can't hold containers");
    let docker_name = format!("hive-{name}");
    anyhow::ensure!(
        valid_name(name) && valid_name(&docker_name),
        "Use up to 58 letters, numbers, '-', '_' or '.' for a container name"
    );
    anyhow::ensure!(!taken.iter().any(|t| t == name), "A machine named {name} already exists");
    anyhow::ensure!(
        !load().iter().any(|c| c.name == name),
        "A container named {name} is already added"
    );
    let tag = image_tag();
    // Building takes minutes the first time on each machine, then never again.
    ssh_timeout(
        machine,
        &format!(
            // A failed build's cause is at the end of its output; errors are
            // truncated from the front, so keep only the tail.
            "docker image inspect {tag} >/dev/null 2>&1 || {{ out=$(printf '%s' {} | docker build -q -t {tag} - 2>&1) || {{ printf '%s\\n' \"$out\" | tail -n 25 >&2; exit 1; }}; }}",
            quote(DOCKERFILE),
            tag = quote(&tag)
        ),
        None,
        900,
    )
    .await?;
    let probe = LOGINS
        .iter()
        .map(|l| format!("[ -d \"$HOME/{l}\" ] && echo login={l}"))
        .collect::<Vec<_>>()
        .join("; ");
    let home = parse_home(
        &ssh_timeout(
            machine,
            &format!("echo \"home=$HOME\"; echo \"user=$(id -u):$(id -g)\"; {probe}; exit 0"),
            None,
            30,
        )
        .await?,
    )?;
    ssh_timeout(machine, &run_command(&docker_name, &tag, &home), None, 120).await?;
    let entry = Container {
        name: name.to_string(),
        host: machine.name.clone(),
        container: docker_name,
        managed: true,
        image: Some(tag),
    };
    add(entry.clone(), taken)?;
    Ok(entry)
}

/// Delete a container Hive created, then forget it. Refuses anything else.
pub async fn delete(
    machine: &hive_common::protocol::WorkerInfo,
    entry: &Container,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        entry.managed,
        "{} wasn't created by Hive; it can only be removed from the list",
        entry.name
    );
    anyhow::ensure!(machine.container.is_none(), "Containers can't hold containers");
    super::transport::ssh_timeout(
        machine,
        &format!("docker rm -f {}", super::transport::quote(&entry.container)),
        None,
        60,
    )
    .await?;
    remove(&entry.name)?;
    Ok(())
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
    fn run_mounts_only_existing_login_folders_under_hives_home() {
        let home = parse_home(
            "home=/Users/me x\nuser=501:20\nlogin=.codex\nlogin=.local/share/opencode\nlogin=../etc\n",
        )
        .unwrap();
        let line = run_command("hive-dev", "hive-agent:abc", &home);
        assert!(line.starts_with(
            "docker run -d --name 'hive-dev' --restart unless-stopped --user '501:20'"
        ));
        assert!(line.contains("-v '/Users/me x/.codex:/home/hive/.codex'"));
        assert!(line.contains(
            "-v '/Users/me x/.local/share/opencode:/home/hive/.local/share/opencode'"
        ));
        // Only folders Hive asked about, and never the shared ~/.claude.json.
        assert!(!line.contains("/.claude:") && !line.contains("../etc") && !line.contains(".claude.json:"));
        assert!(line.contains(" 'hive-agent:abc' sh -c "));
        for bad in ["home=/h\nuser=0;rm:0\n", "home=/h\nuser=0\n", "user=1:1\n"] {
            assert!(parse_home(bad).is_err(), "{bad}");
        }
    }

    /// Builds the image and starts a real container on this machine's Docker.
    /// `cargo test -p hive-core live_create -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_create_runs_as_the_machine_user_with_shared_logins() {
        let registry = std::env::temp_dir().join(format!("hive-containers-{}.json", uuid::Uuid::new_v4()));
        std::env::set_var("HIVE_CONTAINERS_FILE", &registry);
        let machine = hive_common::protocol::WorkerInfo {
            name: "live-machine".into(),
            host: "localhost".into(),
            user: std::env::var("USER").unwrap_or_default(),
            port: None,
            tags: vec![],
            local: true,
            container: None,
        };
        let entry = create(&machine, "phase-b-probe", &[]).await.expect("create");
        assert!(entry.managed && entry.container == "hive-phase-b-probe");
        let mut inside = machine.clone();
        inside.container = Some(entry.container.clone());
        let report = super::super::transport::ssh_timeout(
            &inside,
            r#"echo "whoami=$(whoami) home=$HOME uid=$(id -u)"
python3 --version; tmux -V; node --version
for t in claude codex opencode; do echo "$t: $($t --version 2>&1 | head -1)"; done
ls -a ~/.codex >/dev/null 2>&1 && echo codex_dir=readable
f=~/.codex/.hive-write-test-$$; touch "$f" 2>/dev/null && rm -f "$f" && echo codex_dir=writable
codex login status 2>&1 | head -2 | sed 's/^/codex login: /'
claude auth status 2>&1 | grep -m1 loggedIn | sed 's/^/claude auth: /'
ls ~/.local/share/opencode/auth.json >/dev/null 2>&1 && echo opencode_auth=present
opencode auth list 2>&1 | head -5 | sed 's/^/opencode: /'
exit 0"#,
            None,
            120,
        )
        .await;
        println!("{}", report.as_deref().unwrap_or_else(|e| panic!("inside: {e}")));
        delete(&machine, &entry).await.expect("delete");
        assert!(load().is_empty());
        std::fs::remove_file(registry).ok();
    }

    #[test]
    fn the_image_tag_follows_the_dockerfile() {
        let tag = image_tag();
        assert!(tag.starts_with("hive-agent:") && tag.len() == "hive-agent:".len() + 12);
        assert!(DOCKERFILE.contains("FROM python:3.12-slim"));
    }

    #[test]
    fn exec_quotes_the_command_as_one_argument() {
        let line = exec("box", "echo 'hi'; tmux ls", false);
        assert_eq!(line, r#"docker exec -i 'box' sh -lc 'echo '"'"'hi'"'"'; tmux ls'"#);
        assert!(exec("box", "x", true).starts_with("docker exec -it "));
    }
}
