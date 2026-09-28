//! Tests for machines memory module and local/remote probe commands.

use super::*;

#[test]
fn test_local_and_remote_probe_commands_use_login_shell() {
    let cmd = local_probe_command();
    let program = cmd.as_std().get_program().to_string_lossy();
    assert!(
        program.contains("bash") || program.contains("sh"),
        "local probe program should be a shell: {program}"
    );
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.iter().any(|arg| arg == "-l" || arg == "-lc" || arg.contains("-l")),
        "local probe must run with login shell flag (-l): {args:?}"
    );
    let remote_cmd = remote_probe_command();
    assert!(
        remote_cmd.contains("bash -lc") || remote_cmd.contains("bash -l -c"),
        "remote probe must use login shell: {remote_cmd}"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn test_local_probe_reports_tools_from_login_shell_profile() {
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

    let stub_cargo = bin_dir.join("cargo");
    std::fs::write(&stub_cargo, "#!/bin/sh\necho cargo 1.80.0\n").expect("write stub");
    std::fs::set_permissions(&stub_cargo, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let profile_content = format!("export PATH=\"{}:$PATH\"\n", bin_dir.display());
    std::fs::write(temp_dir.path.join(".profile"), &profile_content).expect("write .profile");
    std::fs::write(temp_dir.path.join(".bash_profile"), &profile_content).expect("write .bash_profile");

    struct EnvGuard {
        home: Option<std::ffi::OsString>,
        path: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(ref h) = self.home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
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
        path: std::env::var_os("PATH"),
    };

    std::env::set_var("HOME", &temp_dir.path);
    std::env::set_var("PATH", "/usr/bin:/bin");

    let facts = probe_local("test-master").await;

    assert!(
        facts.reachable,
        "probe must succeed with fake HOME"
    );
    assert!(
        facts.tools.contains(&"cargo".to_string()),
        "probe must report cargo added to PATH by login shell profile, got tools: {:?}",
        facts.tools
    );
}

#[tokio::test]
#[cfg(not(unix))]
async fn test_local_probe_reports_tools_from_login_shell_profile() {
    // Cleanly skipped on non-unix platforms
}
