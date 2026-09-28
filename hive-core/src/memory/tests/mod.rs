//! Behavioural tests for local machine probe and login shell PATH resolution.

use super::*;

#[tokio::test]
#[cfg(unix)]
async fn local_probe_reports_tools_from_login_shell_profile() {
    use std::os::unix::fs::PermissionsExt;

    struct TempDir {
        path: std::path::PathBuf,
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    let temp_dir = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hive-probe-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    };

    let bin_dir = temp_dir.path.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");

    // Stub cargo binary (probed tool)
    let stub_cargo = bin_dir.join("cargo");
    std::fs::write(&stub_cargo, "#!/bin/sh\necho cargo 1.80.0\n").expect("write stub");
    std::fs::set_permissions(&stub_cargo, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Fake shell profile that adds the stub bin directory to PATH
    let profile = temp_dir.path.join(".profile");
    std::fs::write(
        &profile,
        format!("export PATH=\"{}:$PATH\"\n", bin_dir.display()),
    )
    .expect("profile");

    // Fake SHELL script that sources ~/.profile when run as a login shell
    let fake_shell = temp_dir.path.join("fake_shell.sh");
    let fake_shell_content = r#"#!/bin/sh
if [ -f "$HOME/.profile" ]; then
    . "$HOME/.profile"
fi
exec /bin/sh "$@"
"#;
    std::fs::write(&fake_shell, fake_shell_content).expect("fake shell");
    std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    // Never mutate process-wide environment variables in tests!
    // Test the resolver and probe directly using explicit shell and HOME parameters.
    let resolved_path =
        resolve_login_path_with(&fake_shell, &temp_dir.path, "/usr/bin:/bin").await;
    assert!(
        resolved_path.contains(&bin_dir.display().to_string()),
        "resolved PATH must include stub tool dir from profile: {resolved_path}"
    );

    let facts =
        probe_local_with("test-master", &fake_shell, &temp_dir.path, "/usr/bin:/bin").await;

    assert!(
        facts.reachable,
        "probe must succeed with fake login shell and HOME"
    );
    assert!(
        facts.tools.contains(&"cargo".to_string()),
        "probe must report stub tool found on login shell PATH: {:?}",
        facts.tools
    );
}

#[tokio::test]
#[cfg(not(unix))]
async fn local_probe_reports_tools_from_login_shell_profile() {
    // Cleanly skipped on non-unix platforms
}
