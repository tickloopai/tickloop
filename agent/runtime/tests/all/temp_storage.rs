use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::Command,
    sync::Arc,
};

use serde_json::json;
use tickloop_agent_protocol::{EventKind, ExecutionMode, ToolResult};
use tickloop_agent_runtime::Agent;

const FIXTURE_ENV: &str = "TICKLOOP_TEMP_STORAGE_FIXTURE";
const OUTPUT: &str = "private-temp-shell-output";

#[tokio::test]
async fn bash_uses_private_storage_without_modifying_a_shared_legacy_root() {
    if std::env::var_os(FIXTURE_ENV).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        // SAFETY: getuid has no preconditions and cannot fail.
        let legacy_root = base.join(format!("tickloop-{}", unsafe { libc::getuid() }));
        std::fs::create_dir(&legacy_root).unwrap();
        std::fs::set_permissions(&legacy_root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let legacy_file = legacy_root.join("existing-output.txt");
        std::fs::write(&legacy_file, "preserve legacy output").unwrap();
        let other_temp = directory.path().join("other-system-temp");
        std::fs::create_dir(&other_temp).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                tickloop_agent_test_support::test_path!(
                    bash_uses_private_storage_without_modifying_a_shared_legacy_root
                )
                .as_str(),
                "--nocapture",
            ])
            .env(FIXTURE_ENV, "1")
            .env("TICKLOOP_TMPDIR", &base)
            .env("TMPDIR", &other_temp)
            .output()
            .unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&legacy_root).unwrap().mode() & 0o777,
            0o755,
            "the existing root's permissions remain unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(&legacy_file).unwrap(),
            "preserve legacy output"
        );
        assert!(
            output.status.success(),
            "Bash must work despite the old shared root:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let result = invoke_bash().await;
    assert!(!result.is_error, "Bash failed: {}", result.content);
    assert!(result.content.contains(OUTPUT), "{}", result.content);
    let output_path = result.output_ref.expect("Bash records its task output");
    let base = std::path::PathBuf::from(std::env::var_os("TICKLOOP_TMPDIR").unwrap())
        .canonicalize()
        .unwrap();
    let expected_uid = std::fs::metadata(&base).unwrap().uid();
    assert!(output_path.starts_with(&base), "{}", output_path.display());
    for parent in output_path
        .ancestors()
        .skip(1)
        .take_while(|path| *path != base)
    {
        let metadata = std::fs::symlink_metadata(parent).unwrap();
        assert_eq!(
            metadata.mode() & 0o777,
            0o700,
            "{} is private",
            parent.display()
        );
        assert_eq!(metadata.uid(), expected_uid, "{}", parent.display());
    }
    assert_eq!(
        std::fs::symlink_metadata(output_path).unwrap().mode() & 0o777,
        0o600
    );
}

async fn invoke_bash() -> ToolResult {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "private-temp-shell",
            "Bash",
            json!({"command": format!("printf {OUTPUT}; printf dispatched > dispatch-marker"), "description": "Read a fixture marker"}),
        ),
        tickloop_agent_test_support::text("The shell worked."),
    ]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model);
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Print the fixture marker".into())
        .await
        .unwrap();
    let result = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted { result, .. } => break result,
            EventKind::Failed(error) => panic!("the turn failed before Bash returned: {error}"),
            EventKind::Completed { .. } => panic!("the turn completed without a Bash result"),
            _ => {}
        }
    };
    agent.shutdown().await;
    assert_eq!(
        directory.path().join("dispatch-marker").exists(),
        !result.is_error,
        "an unsafe storage root is rejected before dispatch"
    );
    result
}

async fn assert_bash_uses_no_untrusted_root(
    test_name: &str,
    is_legacy_root: bool,
    is_symlink: bool,
) {
    if std::env::var_os(FIXTURE_ENV).is_some() {
        let result = invoke_bash().await;
        if is_legacy_root {
            assert!(!result.is_error, "{}", result.content);
            assert!(result.content.contains(OUTPUT));
        } else {
            assert!(result.is_error, "the unsafe root must be rejected");
            let reason = if is_symlink {
                "symbolic link"
            } else {
                "mode 0755"
            };
            assert!(result.content.contains(reason), "{}", result.content);
        }
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let prefix = if is_legacy_root {
        "tickloop-"
    } else {
        "tickloop-private-"
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let root = base.join(format!("{prefix}{}", unsafe { libc::getuid() }));
    let target = base.join("untouched-target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("marker"), "untouched").unwrap();
    if is_symlink {
        std::os::unix::fs::symlink(&target, &root).unwrap();
    } else {
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let before = std::fs::symlink_metadata(&root).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(FIXTURE_ENV, "1")
        .env("TICKLOOP_TMPDIR", &base)
        .output()
        .unwrap();
    let after = std::fs::symlink_metadata(&root).unwrap();
    assert_eq!(after.mode(), before.mode());
    assert_eq!(after.ino(), before.ino());
    assert_eq!(
        std::fs::read_to_string(target.join("marker")).unwrap(),
        "untouched"
    );
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !is_legacy_root {
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 2);
    }
}

#[tokio::test]
async fn bash_does_not_follow_a_legacy_root_symlink() {
    assert_bash_uses_no_untrusted_root(
        &tickloop_agent_test_support::test_path!(bash_does_not_follow_a_legacy_root_symlink),
        true,
        true,
    )
    .await;
}

#[tokio::test]
async fn bash_rejects_a_shared_private_root_without_falling_back() {
    assert_bash_uses_no_untrusted_root(
        &tickloop_agent_test_support::test_path!(
            bash_rejects_a_shared_private_root_without_falling_back
        ),
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn bash_rejects_a_private_root_symlink_without_falling_back() {
    assert_bash_uses_no_untrusted_root(
        &tickloop_agent_test_support::test_path!(
            bash_rejects_a_private_root_symlink_without_falling_back
        ),
        false,
        true,
    )
    .await;
}
