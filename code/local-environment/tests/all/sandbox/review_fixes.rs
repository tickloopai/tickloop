use super::*;

#[tokio::test]
async fn sandbox_external_cwd_requires_approval_and_cannot_expand_write_access() {
    for relative in [false, true] {
        for allow in [false, true] {
            let fixture =
                SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
            let cwd = if relative {
                "../".to_owned()
            } else {
                fixture.root.path().display().to_string()
            };
            let (agent, chat) = fixture
                .agent(
                    serde_json::json!({"command":"printf forbidden > outside", "cwd":cwd}),
                    ExecutionMode::AcceptEdits,
                )
                .await;
            let (asked, result) = fixture.run_with_human_approval(&agent, chat, allow).await;
            assert!(asked, "external cwd must use normal approval: {result:?}");
            assert!(result.is_error, "{}", result.content);
            if allow {
                assert!(result.content.contains("被沙箱拦截"), "{}", result.content);
            }
            assert!(!fixture.root.path().join("outside").exists());
        }
    }
}

#[tokio::test]
async fn sandbox_external_cwd_is_reviewed_in_auto() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let (agent, chat) = fixture
        .agent(
            serde_json::json!({"command":"mkdir outside", "cwd":"../"}),
            ExecutionMode::Auto,
        )
        .await;
    let reviewer = blocking_reviewer();
    agent.set_auto_reviewer(reviewer.clone()).await;
    let (asked, result) = fixture.run_with_human_approval(&agent, chat, false).await;
    assert!(!asked);
    assert!(!reviewer.requests.lock().unwrap().is_empty());
    assert!(result.is_error, "{}", result.content);
    assert!(!fixture.root.path().join("outside").exists());
}

#[tokio::test]
async fn sandbox_subdirectory_cwd_keeps_folder_writable_and_config_paths_folder_relative() {
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "filesystem":{"allowWrite":["../cache"],"denyWrite":["protected"]}
    }));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    std::fs::create_dir(folder.join("subdir")).unwrap();
    std::fs::create_dir(fixture.root.path().join("cache")).unwrap();
    let (agent, chat) = fixture.agent(
        serde_json::json!({
            "command":"printf folder > ../note; printf cache > ../../cache/note; printf child > protected; printf forbidden > ../protected",
            "cwd":"subdir"
        }),
        ExecutionMode::AcceptEdits,
    ).await;
    let (asked, result) = fixture.run_with_human_approval(&agent, chat, false).await;
    assert!(!asked, "subdirectory cwd retains sandbox admission");
    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("被沙箱拦截"), "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(folder.join("note")).unwrap(),
        "folder"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("cache/note")).unwrap(),
        "cache"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("subdir/protected")).unwrap(),
        "child"
    );
    assert!(!folder.join("protected").exists());
}

#[tokio::test]
async fn sandbox_additional_directory_cwd_keeps_folder_write_access() {
    let mut fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let additional = fixture.root.path().join("additional");
    std::fs::create_dir(&additional).unwrap();
    fixture
        .workspace
        .additional_directories
        .push(additional.clone());
    let operation = tickloop_ids::generate();
    let prepared = fixture.tools.prepare_for_operation(
        &tickloop_ids::IdContext::new(), &fixture.workspace, "Bash",
        &serde_json::json!({"command":"printf folder > ../folder/note; printf additional > note", "cwd":additional}),
        operation,
    ).await.unwrap();
    assert!(!prepared.approval_facts().requires_approval.unwrap_or(false));
    let result = fixture.run((prepared, operation)).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.folder.as_ref().unwrap().join("note")).unwrap(),
        "folder"
    );
    assert_eq!(
        std::fs::read_to_string(additional.join("note")).unwrap(),
        "additional"
    );
}

#[tokio::test]
async fn sandbox_settings_failure_refuses_the_next_shell_preparation() {
    for read_failure in [false, true] {
        let fixture = SandboxFixture::new(serde_json::json!({
            "enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":false
        }));
        let before = fixture.run(fixture.prepare("printf before").await).await;
        assert!(!before.is_error, "{}", before.content);
        let expected_error = if read_failure {
            std::fs::remove_file(fixture.settings.file_path()).unwrap();
            std::fs::create_dir(fixture.settings.file_path()).unwrap();
            "cannot read settings file"
        } else {
            std::fs::write(fixture.settings.file_path(), "{invalid json").unwrap();
            "not valid JSON"
        };
        let error = fixture
            .tools
            .prepare_for_operation(
                &tickloop_ids::IdContext::new(),
                &fixture.workspace,
                "Bash",
                &serde_json::json!({"command":"mkdir should-not-run"}),
                tickloop_ids::generate(),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("SandboxConfiguration"), "{message}");
        assert!(message.contains("settings.json"), "{message}");
        assert!(message.contains(expected_error), "{message}");
        assert!(
            !fixture
                .workspace
                .folder
                .as_ref()
                .unwrap()
                .join("should-not-run")
                .exists()
        );
    }
}

#[tokio::test]
async fn valid_settings_without_sandbox_or_with_it_disabled_keep_unsandboxed_shells() {
    for settings in [
        serde_json::json!({}),
        serde_json::json!({"sandbox":{"enabled":false}}),
    ] {
        let fixture =
            SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
        std::fs::write(
            fixture.settings.file_path(),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        let prepared = fixture.prepare("printf outside > ../outside").await;
        assert_eq!(
            prepared.0.approval_facts().isolation,
            Some(ExecutionIsolation::default())
        );
        let result = fixture.run(prepared).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(fixture.root.path().join("outside")).unwrap(),
            "outside"
        );
    }
}

#[tokio::test]
async fn sandbox_linked_worktree_cwd_requires_approval_but_keeps_trusted_writable_roots() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    let sibling = fixture.root.path().join("linked");
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet", "--template="])
            .arg(folder)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(["worktree", "add", "--quiet", "--orphan"])
            .arg(&sibling)
            .status()
            .unwrap()
            .success()
    );
    let (agent, chat) = fixture.agent(
        serde_json::json!({"command":"printf linked > note; printf folder > ../folder/note", "cwd":sibling}),
        ExecutionMode::AcceptEdits,
    ).await;
    let (asked, result) = fixture.run_with_human_approval(&agent, chat, true).await;
    assert!(
        asked,
        "a linked cwd outside Folder/additional directories needs ordinary approval"
    );
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(sibling.join("note")).unwrap(),
        "linked"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("note")).unwrap(),
        "folder"
    );
}
