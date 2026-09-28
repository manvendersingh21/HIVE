//! Behavioural tests for local machine probe and login shell PATH resolution.

use super::*;

#[tokio::test]
#[cfg(unix)]
async fn local_probe_reports_tools_from_login_shell_profile() {
    use std::os::unix::fs::PermissionsExt;

    // Mutex to avoid concurrency interference with environment variables and cache
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _lock = TEST_LOCK.lock().await;

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

    struct EnvGuard {
        home: Option<std::ffi::OsString>,
        shell: Option<std::ffi::OsString>,
        path: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(ref h) = self.home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
            if let Some(ref s) = self.shell {
                std::env::set_var("SHELL", s);
            } else {
                std::env::remove_var("SHELL");
            }
            if let Some(ref p) = self.path {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    let _guard = EnvGuard {
        home: std::env::var_os("HOME"),
        shell: std::env::var_os("SHELL"),
        path: std::env::var_os("PATH"),
    };

    std::env::set_var("HOME", &temp_dir.path);
    std::env::set_var("SHELL", &fake_shell);
    std::env::set_var("PATH", "/usr/bin:/bin");

    reset_cached_login_path().await;

    let facts = probe_local("test-master").await;

    reset_cached_login_path().await;

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
