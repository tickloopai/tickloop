use crate::support;

use std::process::Command;

#[test]
fn update_help_and_version_do_not_start_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let daemon = support::cli_runtime::adopt(&root.path().join("config"));
    for arguments in [
        vec!["update", "--help"],
        vec!["--version"],
        vec!["--build-identity"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(arguments)
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
    assert!(!daemon.exists());
    assert!(!root.path().join("config").exists());
}

#[test]
fn update_rejects_unknown_flags_and_unmanaged_development_builds() {
    let root = tempfile::tempdir().unwrap();
    let daemon = support::cli_runtime::adopt(&root.path().join("config"));
    for arguments in [
        vec!["update", "--unknown"],
        vec!["update", "--check"],
        vec!["update", "--background-startup"],
        vec!["update", "--background"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(&arguments)
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
    assert!(!daemon.exists());
    assert!(!root.path().join("config").exists());
}

#[test]
fn cargo_installer_activates_the_runtime_without_replacing_cargo_owned_entries() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let daemon = support::cli_runtime::adopt(&root.path().join("config"));
    let source = root.path().join("downloaded");
    let cargo_root = root.path().join("Cargo install root");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir_all(cargo_root.join("bin")).unwrap();
    let payload = source.join("tickloop");
    std::fs::copy(env!("CARGO_BIN_EXE_tickloop"), &payload).unwrap();
    let identity = Command::new(&payload)
        .arg("--build-identity")
        .output()
        .unwrap();
    assert!(identity.status.success());
    let identity = String::from_utf8(identity.stdout).unwrap();
    let identity = identity.trim();
    let version = env!("CARGO_PKG_VERSION");
    let tickhost = source.join("tickhost");
    std::fs::write(&tickhost, format!("#!/bin/sh\ncase \"$1\" in\n--version) echo 'tickhost {version}';;\n--build-identity) echo '{identity}';;\n*) exit 2;;\nesac\n")).unwrap();
    std::fs::set_permissions(tickhost, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launcher = cargo_root.join("bin/tickloop");
    std::fs::write(&launcher, b"Cargo owns the launcher").unwrap();
    let metadata = serde_json::json!({"installs": {format!("tickloop {version} (registry+https://github.com/rust-lang/crates.io-index)"): {"bins":["tickloop","tl","tickhost"]}}});
    let metadata = serde_json::to_vec(&metadata).unwrap();
    std::fs::write(cargo_root.join(".crates2.json"), &metadata).unwrap();
    let output = Command::new(&payload)
        .args(["update", "--install-cargo"])
        .arg(&launcher)
        .env("TICKLOOP_CONFIG_DIR", root.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&launcher).unwrap(),
        b"Cargo owns the launcher"
    );
    assert_eq!(
        std::fs::read(cargo_root.join(".crates2.json")).unwrap(),
        metadata
    );
    let runtime = cargo_root.join(".tickloop/current");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(runtime.join("installation.json")).unwrap()).unwrap();
    assert_eq!(record["source"]["kind"], "cargo_registry");
    assert_eq!(record["build"]["identity"], identity);
    for name in ["tickloop", "tickhost"] {
        let output = Command::new(runtime.join(name))
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("{name} {version}")
        );
    }
    assert!(!root.path().join("config").exists());
    assert!(!daemon.exists());
}
