//! SSH onboarding for machines added in Settings → Fleet: Hive's key, a
//! connection check, host-key trust and a one-time password key install.
//!
//! Every check runs the system `ssh` with the options Hive's transport uses, so
//! "test connection" tests what delegation will do. Machines the user's own
//! `~/.ssh/config` doesn't configure get Hive's key through a managed
//! `~/.ssh/config.d/hive.conf`, which every SSH path (transport, sessions,
//! terminals) reads.

use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::chat::AgentHandle;

const KEY_NAME: &str = "hive_worker_ed25519";
const MANAGED_HEADER: &str = "# Managed by Hive for machines added in Settings → Fleet.\n\
# Regenerated on every change; edit ~/.ssh/config instead.\n";

/// Where Hive keeps its SSH state. Tests point this at a scratch directory.
#[derive(Clone)]
pub struct SshPaths {
    pub dir: PathBuf,
}

impl SshPaths {
    pub fn from_env() -> Option<Self> {
        std::env::var_os("HOME").map(|home| Self {
            dir: Path::new(&home).join(".ssh"),
        })
    }
    pub fn key(&self) -> PathBuf {
        self.dir.join(KEY_NAME)
    }
    fn public_key(&self) -> PathBuf {
        self.dir.join(format!("{KEY_NAME}.pub"))
    }
    fn managed_config(&self) -> PathBuf {
        self.dir.join("config.d").join("hive.conf")
    }
    /// Whether `~/.ssh/config` reads `config.d/*.conf`, without which the
    /// managed file has no effect.
    fn config_includes_managed(&self) -> bool {
        std::fs::read_to_string(self.dir.join("config")).is_ok_and(|config| {
            config.lines().any(|line| {
                let line = line.trim();
                line.len() > 8
                    && line[..8].eq_ignore_ascii_case("include ")
                    && line.contains("config.d/")
            })
        })
    }
}

// ------------------------------------------------------------------ input

/// A host becomes one `ssh` argument and a `Host` line in the managed config,
/// so it must not look like an option, a pattern or a second directive.
pub fn validate_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() || host.len() > 253 {
        return Err("Host is required and must be at most 253 characters");
    }
    if host.starts_with('-') {
        return Err("Host can't start with '-'");
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err("Host may contain only letters, digits and . - _ : [ ]");
    }
    Ok(())
}

pub fn validate_user(user: &str) -> Result<(), &'static str> {
    if user.is_empty() || user.len() > 64 {
        return Err("SSH user is required and must be at most 64 characters");
    }
    if user.starts_with('-') {
        return Err("SSH user can't start with '-'");
    }
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err("SSH user may contain only letters, digits and . - _");
    }
    Ok(())
}

pub fn validate_port(port: Option<u16>) -> Result<(), &'static str> {
    match port {
        Some(0) => Err("Port must be between 1 and 65535"),
        _ => Ok(()),
    }
}

#[derive(Deserialize, Clone)]
pub struct Target {
    pub host: String,
    pub user: String,
    #[serde(default)]
    pub port: Option<u16>,
}

impl Target {
    fn cleaned(self) -> Result<Self, Response> {
        let target = Self {
            host: self.host.trim().to_string(),
            user: self.user.trim().to_string(),
            port: self.port,
        };
        validate_host(&target.host)
            .and(validate_user(&target.user))
            .and(validate_port(target.port))
            .map_err(bad_request)?;
        Ok(target)
    }
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_string()).into_response()
}

fn server_error(error: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
}

// ------------------------------------------------------------------ outcome

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ok,
    HostKeyUnknown,
    HostKeyChanged,
    NotAuthorized,
    Unresolvable,
    Unreachable,
    Error,
}

#[derive(Serialize, Debug)]
pub struct TestResult {
    pub status: Status,
    pub message: String,
    /// Authentication methods the server offered, when it refused Hive's key.
    pub methods: Vec<String>,
    /// Whether the server accepts a password, so Hive can install its key.
    pub password_possible: bool,
}

/// Turn an `ssh` exit into something a person can act on.
pub fn classify(success: bool, stderr: &str, target: &Target) -> TestResult {
    let who = format!("{}@{}", target.user, target.host);
    let result = |status, message: String| TestResult {
        status,
        message,
        methods: vec![],
        password_possible: false,
    };
    if success {
        return result(Status::Ok, format!("Connected to {who}."));
    }
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || stderr.contains("has changed and you have requested strict checking") {
        return result(
            Status::HostKeyChanged,
            format!("{}'s host key doesn't match the one Hive trusted before. That can mean the machine was reinstalled, or that something is intercepting the connection. Check with the machine's owner, then remove the old key with `ssh-keygen -R {}`.", target.host, target.host),
        );
    }
    if stderr.contains("Host key verification failed") || stderr.contains("host key is known") {
        return result(
            Status::HostKeyUnknown,
            format!("Hive hasn't seen {}'s host key yet. Check its fingerprint and trust it to continue.", target.host),
        );
    }
    if let Some(start) = stderr.find("Permission denied (") {
        let rest = &stderr[start + "Permission denied (".len()..];
        let methods: Vec<String> = rest
            .split(')')
            .next()
            .unwrap_or_default()
            .split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        let password_possible = methods
            .iter()
            .any(|m| m == "password" || m == "keyboard-interactive");
        return TestResult {
            status: Status::NotAuthorized,
            message: format!("{who} is reachable, but Hive's key isn't authorized there yet."),
            methods,
            password_possible,
        };
    }
    if stderr.contains("Could not resolve hostname") {
        return result(Status::Unresolvable, format!("{} doesn't resolve to an address. Check the name, or use an IP address.", target.host));
    }
    if ["Connection refused", "timed out", "No route to host", "Network is unreachable", "Connection closed", "Connection reset"]
        .iter()
        .any(|s| stderr.contains(s))
    {
        return result(Status::Unreachable, format!("Couldn't reach {}: {}", target.host, first_line(stderr)));
    }
    result(Status::Error, format!("SSH to {who} failed: {}", first_line(stderr)))
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
        .unwrap_or("no error output");
    line.chars().take(300).collect()
}

/// `ssh-keygen -lf` lines: "256 SHA256:abc… host (ED25519)".
pub fn parse_fingerprints(listing: &str) -> Vec<Fingerprint> {
    listing
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let _bits = parts.next()?;
            let fingerprint = parts.next()?.to_string();
            let kind = line
                .rsplit_once('(')
                .map(|(_, k)| k.trim_end_matches(')').to_string())
                .unwrap_or_default();
            fingerprint
                .starts_with("SHA256:")
                .then_some(Fingerprint { fingerprint, kind })
        })
        .collect()
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Fingerprint {
    pub fingerprint: String,
    pub kind: String,
}

/// The managed file: Hive's key for each machine that needs it.
pub fn render_managed_config(key: &Path, hosts: &[String]) -> String {
    let mut out = String::from(MANAGED_HEADER);
    for host in hosts {
        out.push_str(&format!(
            "\nHost {host}\n  IdentityFile {}\n  IdentitiesOnly yes\n",
            key.display()
        ));
    }
    out
}

fn managed_hosts(paths: &SshPaths) -> Vec<String> {
    std::fs::read_to_string(paths.managed_config())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("Host "))
        .map(|h| h.trim().to_string())
        .collect()
}

// ------------------------------------------------------------------ ssh

/// A private scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("hive-ssh-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn run(mut cmd: Command, stdin: Option<String>, seconds: u64) -> anyhow::Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut pipe = child.stdin.take().unwrap();
    let data = stdin.unwrap_or_default();
    let writer = tokio::spawn(async move {
        let _ = pipe.write_all(data.as_bytes()).await;
        let _ = pipe.shutdown().await;
    });
    let output = tokio::time::timeout(std::time::Duration::from_secs(seconds), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {seconds} seconds"))??;
    let _ = writer.await;
    Ok(Output {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Effective `ssh -G` settings for a host, optionally ignoring every config.
async fn effective(host: &str, ignore_config: bool) -> anyhow::Result<Vec<(String, String)>> {
    let mut cmd = Command::new("ssh");
    if ignore_config {
        cmd.args(["-F", "/dev/null"]);
    }
    cmd.args(["-G", host]);
    let out = run(cmd, None, 10).await?;
    anyhow::ensure!(out.success, "ssh -G {host}: {}", first_line(&out.stderr));
    Ok(out
        .stdout
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect())
}

/// Whether Hive's key must be supplied for `host`: true unless the user's ssh
/// config already says how to reach it.
async fn needs_hive_key(paths: &SshPaths, host: &str) -> anyhow::Result<bool> {
    if managed_hosts(paths).iter().any(|h| h == host) {
        return Ok(true);
    }
    let pick = |settings: &[(String, String)]| {
        let mut picked: Vec<_> = settings
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), "hostname" | "user" | "port" | "identityfile"))
            .cloned()
            .collect();
        picked.sort();
        picked
    };
    let with_config = pick(&effective(host, false).await?);
    let without = pick(&effective(host, true).await?);
    Ok(with_config == without)
}

/// The options every check shares with Hive's transport, plus Hive's key
/// when the host needs it. No connection sharing, so a stale mux can't hide
/// the real result.
async fn ssh_command(paths: &SshPaths, target: &Target) -> anyhow::Result<Command> {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "ConnectTimeout=5", "-o", "ControlMaster=no", "-o", "ControlPath=none"]);
    if needs_hive_key(paths, &target.host).await? {
        cmd.arg("-i").arg(paths.key()).args(["-o", "IdentitiesOnly=yes"]);
    }
    if let Some(port) = target.port {
        cmd.args(["-p", &port.to_string()]);
    }
    Ok(cmd)
}

async fn test(paths: &SshPaths, target: &Target) -> anyhow::Result<TestResult> {
    let mut cmd = ssh_command(paths, target).await?;
    cmd.args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"])
        .arg(format!("{}@{}", target.user, target.host))
        .arg("true");
    let out = run(cmd, None, 20).await?;
    Ok(classify(out.success, &out.stderr, target))
}

/// Let ssh record the host key it is offered in a scratch file. ssh stores it
/// right after key exchange, before authentication, and in exactly the form
/// it will look up later (port, HostKeyAlias, hashing), so failed auth is fine.
async fn capture_host_key(paths: &SshPaths, target: &Target, scratch: &Scratch) -> anyhow::Result<(PathBuf, Vec<Fingerprint>)> {
    let file = scratch.0.join("known_hosts");
    let mut cmd = ssh_command(paths, target).await?;
    cmd.args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "GlobalKnownHostsFile=/dev/null"])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", file.display()))
        .arg(format!("{}@{}", target.user, target.host))
        .arg("true");
    let out = run(cmd, None, 20).await?;
    anyhow::ensure!(
        file.exists(),
        "{}",
        classify(out.success, &out.stderr, target).message
    );
    let mut list = Command::new("ssh-keygen");
    list.arg("-lf").arg(&file);
    let listing = run(list, None, 10).await?;
    let fingerprints = parse_fingerprints(&listing.stdout);
    anyhow::ensure!(!fingerprints.is_empty(), "ssh returned no host key for {}", target.host);
    Ok((file, fingerprints))
}

async fn known_hosts_file(paths: &SshPaths, host: &str) -> anyhow::Result<PathBuf> {
    let settings = effective(host, false).await?;
    let first = settings
        .iter()
        .find(|(k, _)| k == "userknownhostsfile")
        .and_then(|(_, v)| v.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "~/.ssh/known_hosts".into());
    Ok(match first.strip_prefix("~/") {
        Some(rest) => paths.dir.parent().unwrap_or(&paths.dir).join(rest),
        None => PathBuf::from(first),
    })
}

fn append_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// Rewrite the managed config atomically for the given hosts.
pub fn write_managed_config(paths: &SshPaths, hosts: &[String]) -> std::io::Result<()> {
    let path = paths.managed_config();
    let dir = path.parent().unwrap();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let temp = dir.join(format!(".hive.conf.{}", uuid::Uuid::new_v4().simple()));
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, render_managed_config(&paths.key(), hosts).as_bytes()))?;
    std::fs::rename(&temp, &path)
}

/// Keep the managed config in step with the UI-added fleet: a machine keeps
/// its block while it's in the fleet, and gets one when added only if the
/// user's own ssh config doesn't already cover its host.
pub async fn sync_managed_config(paths: &SshPaths, fleet_hosts: &[String]) -> anyhow::Result<()> {
    let mut hosts = Vec::new();
    for host in fleet_hosts {
        if !hosts.contains(host) && needs_hive_key(paths, host).await? {
            hosts.push(host.clone());
        }
    }
    if hosts.is_empty() && !paths.managed_config().exists() {
        return Ok(());
    }
    write_managed_config(paths, &hosts)?;
    Ok(())
}

// ------------------------------------------------------------------ api

fn paths() -> Result<SshPaths, Response> {
    SshPaths::from_env().ok_or_else(|| server_error("HOME is not set"))
}

#[derive(Serialize)]
pub struct KeyInfo {
    pub public_key: Option<String>,
    pub path: String,
    /// False when ~/.ssh/config doesn't include config.d, so machines added
    /// here won't use Hive's key until it does.
    pub config_included: bool,
}

fn key_info(paths: &SshPaths) -> KeyInfo {
    KeyInfo {
        public_key: std::fs::read_to_string(paths.public_key())
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty()),
        path: paths.key().display().to_string(),
        config_included: paths.config_includes_managed(),
    }
}

pub async fn get_key() -> Result<Json<KeyInfo>, Response> {
    Ok(Json(key_info(&paths()?)))
}

/// Create Hive's key if there is none. An existing key is never replaced.
pub async fn create_key(State(h): State<AgentHandle>) -> Result<Json<KeyInfo>, Response> {
    let paths = paths()?;
    if !paths.key().exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&paths.dir)
            .map_err(server_error)?;
        let mut cmd = Command::new("ssh-keygen");
        cmd.args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(format!("hive@{}", h.master_name))
            .arg("-f")
            .arg(paths.key());
        let out = run(cmd, None, 20).await.map_err(server_error)?;
        if !out.success {
            return Err(server_error(first_line(&out.stderr)));
        }
    }
    Ok(Json(key_info(&paths)))
}

pub async fn test_connection(Json(target): Json<Target>) -> Result<Json<TestResult>, Response> {
    let target = target.cleaned()?;
    test(&paths()?, &target).await.map(Json).map_err(server_error)
}

#[derive(Serialize)]
pub struct HostKey {
    pub fingerprints: Vec<Fingerprint>,
}

pub async fn scan_host_key(Json(target): Json<Target>) -> Result<Json<HostKey>, Response> {
    let target = target.cleaned()?;
    let paths = paths()?;
    let scratch = Scratch::new().map_err(server_error)?;
    let (_, fingerprints) = capture_host_key(&paths, &target, &scratch)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())?;
    Ok(Json(HostKey { fingerprints }))
}

#[derive(Deserialize)]
pub struct TrustRequest {
    #[serde(flatten)]
    pub target: Target,
    /// Exactly what the person compared and approved.
    pub fingerprints: Vec<Fingerprint>,
}

/// Trust the host key the person approved. The key is captured again and must
/// match what they saw; a key that replaces a trusted one is never accepted here.
pub async fn trust_host_key(Json(req): Json<TrustRequest>) -> Result<Json<TestResult>, Response> {
    let target = req.target.cleaned()?;
    let paths = paths()?;
    let before = test(&paths, &target).await.map_err(server_error)?;
    match before.status {
        Status::HostKeyUnknown => {}
        Status::HostKeyChanged => return Err((StatusCode::CONFLICT, before.message).into_response()),
        _ => return Ok(Json(before)),
    }
    let scratch = Scratch::new().map_err(server_error)?;
    let (file, fingerprints) = capture_host_key(&paths, &target, &scratch)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())?;
    if fingerprints != req.fingerprints {
        return Err((
            StatusCode::CONFLICT,
            "The machine now offers a different host key than the one you approved. Nothing was trusted; check the fingerprint again.",
        )
            .into_response());
    }
    let lines = std::fs::read_to_string(&file).map_err(server_error)?;
    let known_hosts = known_hosts_file(&paths, &target.host).await.map_err(server_error)?;
    append_private(&known_hosts, &lines).map_err(server_error)?;
    tracing::info!(host = %target.host, "host key trusted from settings UI");
    test(&paths, &target).await.map(Json).map_err(server_error)
}

#[derive(Deserialize)]
pub struct InstallRequest {
    #[serde(flatten)]
    pub target: Target,
    pub password: String,
}

/// Remote side of the install: the key arrives on stdin, never in the command.
const INSTALL_SCRIPT: &str = "sh -c 'umask 077; mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && IFS= read -r key && { grep -qxF \"$key\" ~/.ssh/authorized_keys || printf \"%s\\n\" \"$key\" >> ~/.ssh/authorized_keys; }'";

/// Sign in once with the machine's password and add Hive's public key to its
/// authorized_keys. The password lives only in this request and the askpass
/// helper's environment; it is never stored or logged.
pub async fn install_key(Json(req): Json<InstallRequest>) -> Result<Json<TestResult>, Response> {
    let target = req.target.cleaned()?;
    if req.password.is_empty() {
        return Err(bad_request("Password is required"));
    }
    let paths = paths()?;
    let public_key = key_info(&paths)
        .public_key
        .ok_or_else(|| bad_request("Create Hive's key first"))?;
    // The password goes only to a machine whose identity is already trusted.
    let before = test(&paths, &target).await.map_err(server_error)?;
    match before.status {
        Status::NotAuthorized => {}
        Status::Ok => return Ok(Json(before)),
        Status::HostKeyUnknown | Status::HostKeyChanged => {
            return Err((StatusCode::CONFLICT, before.message).into_response())
        }
        _ => return Err((StatusCode::BAD_GATEWAY, before.message).into_response()),
    }
    let scratch = Scratch::new().map_err(server_error)?;
    let askpass = scratch.0.join("askpass");
    std::fs::write(&askpass, "#!/bin/sh\nprintf '%s\\n' \"$HIVE_SSH_SETUP_PASSWORD\"\n").map_err(server_error)?;
    std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).map_err(server_error)?;
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o", "ConnectTimeout=10",
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "BatchMode=no",
        "-o", "StrictHostKeyChecking=yes",
        "-o", "PubkeyAuthentication=no",
        "-o", "PreferredAuthentications=keyboard-interactive,password",
        "-o", "NumberOfPasswordPrompts=1",
    ]);
    if let Some(port) = target.port {
        cmd.args(["-p", &port.to_string()]);
    }
    cmd.arg(format!("{}@{}", target.user, target.host))
        .arg(INSTALL_SCRIPT)
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("DISPLAY", ":0")
        .env("HIVE_SSH_SETUP_PASSWORD", &req.password);
    let out = run(cmd, Some(format!("{public_key}\n")), 45).await.map_err(server_error)?;
    drop(scratch);
    if !out.success {
        let failed = classify(false, &out.stderr, &target);
        let message = if failed.status == Status::NotAuthorized {
            format!("{}@{} didn't accept that password. Nothing was changed.", target.user, target.host)
        } else {
            failed.message
        };
        return Err((StatusCode::BAD_GATEWAY, message).into_response());
    }
    tracing::info!(host = %target.host, user = %target.user, "Hive key installed from settings UI");
    test(&paths, &target).await.map(Json).map_err(server_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target { host: "gpu-node".into(), user: "me".into(), port: None }
    }

    #[test]
    fn hosts_and_users_that_could_become_options_or_directives_are_rejected() {
        for ok in ["gpu-node", "10.0.0.5", "box.tailnet.ts.net", "[fe80::1]", "fe80::1", "host_1"] {
            assert!(validate_host(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-oProxyCommand=evil", "a b", "a\nIdentityFile x", "*", "gpu?", "!x", "me@host", "a/b", "a'b"] {
            assert!(validate_host(bad).is_err(), "{bad:?}");
        }
        assert!(validate_user("tur38767").is_ok());
        for bad in ["", "-l", "a b", "a@b", "a\n", "root;id"] {
            assert!(validate_user(bad).is_err(), "{bad:?}");
        }
        assert!(validate_port(Some(0)).is_err());
        assert!(validate_port(Some(22)).is_ok() && validate_port(None).is_ok());
    }

    #[test]
    fn ssh_failures_become_actionable_outcomes() {
        let t = target();
        assert_eq!(classify(true, "", &t).status, Status::Ok);
        assert_eq!(classify(false, "Host key verification failed.\r\n", &t).status, Status::HostKeyUnknown);
        assert_eq!(classify(false, "No ED25519 host key is known for gpu-node and you have requested strict checking.\nHost key verification failed.", &t).status, Status::HostKeyUnknown);
        let changed = classify(false, "@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n", &t);
        assert_eq!(changed.status, Status::HostKeyChanged);
        assert!(changed.message.contains("ssh-keygen -R gpu-node"));
        let denied = classify(false, "me@gpu-node: Permission denied (publickey,password,keyboard-interactive).\n", &t);
        assert_eq!(denied.status, Status::NotAuthorized);
        assert_eq!(denied.methods, ["publickey", "password", "keyboard-interactive"]);
        assert!(denied.password_possible);
        let key_only = classify(false, "me@gpu-node: Permission denied (publickey).", &t);
        assert!(!key_only.password_possible);
        assert_eq!(classify(false, "ssh: Could not resolve hostname gpu-node: nodename nor servname provided", &t).status, Status::Unresolvable);
        let down = classify(false, "ssh: connect to host gpu-node port 22: Operation timed out", &t);
        assert_eq!(down.status, Status::Unreachable);
        assert!(down.message.contains("Operation timed out"));
        assert_eq!(classify(false, "kex_exchange_identification: Connection reset by peer", &t).status, Status::Unreachable);
        assert_eq!(classify(false, "something new", &t).status, Status::Error);
    }

    #[test]
    fn fingerprints_are_read_from_ssh_keygen_listings() {
        let listing = "256 SHA256:AbC/12+x gpu-node (ED25519)\n3072 SHA256:ZZZ [gpu-node]:2222 (RSA)\nnot a key\n";
        assert_eq!(
            parse_fingerprints(listing),
            [
                Fingerprint { fingerprint: "SHA256:AbC/12+x".into(), kind: "ED25519".into() },
                Fingerprint { fingerprint: "SHA256:ZZZ".into(), kind: "RSA".into() },
            ]
        );
    }

    #[test]
    fn the_managed_config_gives_listed_hosts_hives_key_and_is_private() {
        let dir = std::env::temp_dir().join(format!("hive-ssh-test-{}", uuid::Uuid::new_v4().simple()));
        let paths = SshPaths { dir: dir.clone() };
        write_managed_config(&paths, &["gpu-node".into(), "10.0.0.5".into()]).unwrap();
        let text = std::fs::read_to_string(paths.managed_config()).unwrap();
        assert!(text.starts_with("# Managed by Hive"));
        assert!(text.contains(&format!("Host gpu-node\n  IdentityFile {}\n  IdentitiesOnly yes\n", paths.key().display())));
        assert_eq!(managed_hosts(&paths), ["gpu-node", "10.0.0.5"]);
        let mode = std::fs::metadata(paths.managed_config()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        write_managed_config(&paths, &[]).unwrap();
        assert!(managed_hosts(&paths).is_empty());
        // No leftover temp files from the atomic rewrite.
        assert_eq!(std::fs::read_dir(dir.join("config.d")).unwrap().count(), 1);
        std::fs::write(dir.join("config"), "Host x\n  User y\n").unwrap();
        assert!(!paths.config_includes_managed());
        std::fs::write(dir.join("config"), "Include ~/.ssh/config.d/*.conf\n").unwrap();
        assert!(paths.config_includes_managed());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn known_hosts_lines_are_appended_on_their_own_line_privately() {
        let dir = std::env::temp_dir().join(format!("hive-kh-test-{}", uuid::Uuid::new_v4().simple()));
        let file = dir.join("known_hosts");
        append_private(&file, "a ssh-ed25519 AAA\n").unwrap();
        std::fs::write(&file, "a ssh-ed25519 AAA").unwrap();
        append_private(&file, "b ssh-ed25519 BBB").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a ssh-ed25519 AAA\nb ssh-ed25519 BBB\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_install_script_reads_the_key_from_stdin_only() {
        assert!(INSTALL_SCRIPT.contains("read -r key"));
        assert!(!INSTALL_SCRIPT.contains("ssh-ed25519"));
        assert!(INSTALL_SCRIPT.contains("chmod 700 ~/.ssh") && INSTALL_SCRIPT.contains("chmod 600 ~/.ssh/authorized_keys"));
    }
}
