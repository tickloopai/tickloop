use std::process::Command;

#[test]
fn update_help_and_version_do_not_start_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["update", "--help"],
        vec!["--version"],
        vec!["--build-identity"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(arguments)
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", root.path().join("daemon"))
            .env("TICKLOOP_CONFIG_DIR", root.path().join("config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
    }
    assert!(!root.path().join("daemon").exists());
    assert!(!root.path().join("config").exists());
}

#[test]
fn update_rejects_unknown_flags_and_unmanaged_development_builds() {
    let root = tempfile::tempdir().unwrap();
    for arguments in [vec!["update", "--unknown"], vec!["update", "--check"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(&arguments)
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", root.path().join("daemon"))
            .env("TICKLOOP_CONFIG_DIR", root.path().join("config"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(if arguments[1] == "--unknown" {
                "Usage: tickloop update"
            } else {
                "unmanaged/development build"
            }),
            "{stderr}"
        );
    }
    assert!(!root.path().join("daemon").exists());
    assert!(!root.path().join("config").exists());
}
