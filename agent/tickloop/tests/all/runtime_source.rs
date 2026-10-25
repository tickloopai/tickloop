use std::{
    fs::OpenOptions,
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_agent_client::LocalAgentClient;

use tickloop_agent_access::{AgentAccess, AgentCommand, RuntimeBinaryInfo};

use crate::graceful_upgrade::{PrivateRuntime, command, copy_bundle, version};

fn installation(directory: &Path, source: Value, product_version: &str) {
    std::fs::write(
        directory.join("installation.json"),
        serde_json::to_vec(&json!({
            "source": source,
            "build": {"version": product_version, "identity": "source-boundary-fixture"},
            "pinned": false,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn native_bundle(root: &Path, release: &str) -> PathBuf {
    std::fs::create_dir_all(root.join("versions")).unwrap();
    let directory = root.join("versions").join(release);
    let executable = copy_bundle(Path::new(env!("CARGO_BIN_EXE_tickloop")), &directory);
    installation(
        &directory,
        json!({"kind":"native","root":root.canonicalize().unwrap()}),
        release,
    );
    executable
}

fn selected(root: &Path, executable: &Path) {
    let current = root.join("current");
    if current.symlink_metadata().is_ok() {
        std::fs::remove_file(&current).unwrap();
    }
    symlink(executable.parent().unwrap(), current).unwrap();
}

async fn isolated_daemon(
    runtime: &PrivateRuntime,
    executable: &Path,
    home: &Path,
) -> LocalAgentClient {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(runtime.path().join("daemon.lock"))
        .unwrap();
    let descriptor = lock.as_raw_fd();
    assert_eq!(
        unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut process = tokio::process::Command::new(executable);
    process
        .args(["daemon", "run"])
        .arg(runtime.path())
        .arg(descriptor.to_string())
        .env("HOME", home)
        .env("TICKLOOP_CONFIG_DIR", runtime.config_root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        process.pre_exec(move || {
            if libc::fcntl(descriptor, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = process.spawn().unwrap();
    let client = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                process.try_wait().unwrap().is_none(),
                "private daemon exited during startup"
            );
            if let Ok(client) = LocalAgentClient::connect(runtime.path().join("daemon.sock")).await
            {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("private daemon did not become ready");
    drop(lock);
    client
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[tokio::test]
async fn managed_daemon_rejects_foreign_sources_before_running_the_candidate_probe() {
    let runtime = PrivateRuntime::new();
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let root = scratch.path().join("native");
    let executable = native_bundle(&root, env!("CARGO_PKG_VERSION"));
    selected(&root, &executable);
    let daemon = isolated_daemon(&runtime, &executable, &home).await;
    let epoch = daemon.daemon_epoch();
    let process = daemon.server_process_id();
    let cases = [
        (
            "other-native-root",
            Some(json!({"kind":"native","root":scratch.path().join("other-native")})),
        ),
        (
            "homebrew",
            Some(json!({"kind":"homebrew","executable":scratch.path().join("brew/bin/brew")})),
        ),
        (
            "cargo-git",
            Some(
                json!({"kind":"cargo","root":scratch.path().join("cargo"),"repository":"https://github.com/tickloopai/tickloop.git"}),
            ),
        ),
        (
            "cargo-prebuilt",
            Some(
                json!({"kind":"cargo_registry","root":scratch.path().join("cargo"),"launcher":scratch.path().join("cargo/bin/tickloop")}),
            ),
        ),
        ("unmanaged", None),
    ];
    for (label, source) in cases {
        let directory = scratch.path().join(label);
        std::fs::create_dir(&directory).unwrap();
        let candidate = directory.join("tickloop");
        let probe = directory.join("probe-executed");
        std::fs::write(
            &candidate,
            format!(
                "#!/bin/sh\nprintf probe > {}\nprintf 'not-runtime-metadata\\n'\n",
                quoted(&probe)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
        if let Some(source) = source {
            installation(&directory, source, env!("CARGO_PKG_VERSION"));
        }
        let receipt = daemon
            .execute(command(
                epoch,
                AgentCommand::UpgradeRuntime {
                    executable: candidate.canonicalize().unwrap(),
                },
            ))
            .await
            .unwrap();
        assert!(
            receipt.into_result().is_err(),
            "{label} must not take over the running installation"
        );
        assert!(
            !probe.exists(),
            "{label}: source rejection must happen before executing --runtime-info"
        );
        assert_eq!(daemon.server_process_id(), process);
        assert_eq!(daemon.daemon_epoch(), epoch);
        assert_eq!(version(&daemon).await.executable, executable);
    }
    daemon.shutdown_daemon().await.unwrap();
}

#[tokio::test]
async fn daemon_can_transfer_between_version_directories_of_its_native_source() {
    let runtime = PrivateRuntime::new();
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let root = scratch.path().join("native");
    let old = native_bundle(&root, env!("CARGO_PKG_VERSION"));
    let next_version = format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH").parse::<u32>().unwrap() + 1,
    );
    let successor = native_bundle(&root, &next_version);
    selected(&root, &old);
    let daemon = isolated_daemon(&runtime, &old, &home).await;
    let epoch = daemon.daemon_epoch();
    let original_session = daemon.session_id();
    let process = daemon.server_process_id();
    let metadata: RuntimeBinaryInfo = serde_json::from_slice(
        &std::process::Command::new(&successor)
            .arg("--runtime-info")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    daemon
        .execute(command(
            epoch,
            AgentCommand::UpgradeRuntime {
                executable: successor.clone(),
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if daemon.server_process_id() != process
                && version(&daemon).await.executable == successor
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the native source's successor did not take over");
    assert_eq!(daemon.daemon_epoch(), epoch);
    assert_eq!(daemon.session_id(), original_session);
    assert_eq!(version(&daemon).await.build_id, metadata.build_id);
    selected(&root, &successor);
    daemon.shutdown_daemon().await.unwrap();
}
