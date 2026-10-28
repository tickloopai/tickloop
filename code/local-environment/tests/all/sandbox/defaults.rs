use super::*;

// Acceptance paths are literal examples from ticket 10, independent of the production accessor.
const PROTECTED_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
    ".vscode/settings.json",
    ".idea/workspace.xml",
    ".git/hooks/pre-commit",
    ".git/config",
    ".tickloop/settings.json",
    ".tickloop/skills/example/SKILL.md",
    ".tickloop/hooks/run",
    ".tickloop/loop.md",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/skills/example/SKILL.md",
    ".claude/hooks/run",
    ".claude/commands/run.md",
    ".claude/agents/helper.md",
    ".claude/rules/rule.md",
    ".claude/CLAUDE.md",
    "AGENTS.md",
    "CLAUDE.md",
    "CLAUDE.local.md",
];

#[tokio::test]
async fn sandbox_default_paths_are_unwritable_at_any_depth_and_case() {
    let mut fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.workspace.folder.as_ref().unwrap().clone();
    let additional = fixture.root.path().join("additional");
    let linked = fixture.root.path().join("linked");
    std::fs::create_dir(&additional).unwrap();
    git(&folder, &["init", "--quiet", "--template="]);
    git(
        &folder,
        &[
            "worktree",
            "add",
            "--quiet",
            "--orphan",
            linked.to_str().unwrap(),
        ],
    );
    fixture
        .workspace
        .additional_directories
        .push(additional.clone());
    let mut command = String::new();
    let mut paths = Vec::new();
    for root in [&folder, &additional, &linked] {
        command.push_str(&format!(
            "printf ordinary > '{}'; ",
            root.join("note").display()
        ));
        for prefix in ["", "nested/deeper/"] {
            for name in PROTECTED_FILES {
                // A linked worktree's .git is a pointer file, so its Git directory cases live
                // in the nested repository; the common Git directory is covered by Folder.
                if root == &linked && prefix.is_empty() && name.starts_with(".git/") {
                    continue;
                }
                for relative in [
                    format!("{prefix}{name}"),
                    format!("{prefix}{}", name.to_ascii_uppercase()),
                ] {
                    let path = root.join(&relative);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    let original = std::fs::read(&path).unwrap_or_else(|_| b"original".to_vec());
                    std::fs::write(&path, &original).unwrap();
                    command.push_str(&format!(
                        "if printf forbidden > '{}'; then printf 'BYPASS:{}\n'; fi; ",
                        path.display(),
                        path.display()
                    ));
                    paths.push((path, original));
                }
            }
        }
    }
    command.push_str("exit 1");
    let result = fixture.run(fixture.prepare(&command).await).await;
    // The full matrix exceeds the inline output preview; inspect the public output reference.
    let output = std::fs::read_to_string(result.output_ref.as_ref().unwrap()).unwrap();
    assert!(!output.contains("BYPASS:"), "{output}");
    assert!(result.is_error && output.contains("被沙箱拦截"), "{output}");
    for (path, original) in paths {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "{}",
            path.display()
        );
    }
    for root in [&folder, &additional, &linked] {
        assert_eq!(
            std::fs::read_to_string(root.join("note")).unwrap(),
            "ordinary"
        );
    }
}

#[tokio::test]
async fn sandbox_allow_git_config_only_relaxes_git_config_and_is_frozen_at_prepare() {
    for allowed in [true, false] {
        let fixture = SandboxFixture::new(serde_json::json!({
            "enabled":true,"failIfUnavailable":true,"filesystem":{"allowGitConfig":allowed}
        }));
        let folder = fixture.workspace.folder.as_ref().unwrap();
        std::fs::create_dir_all(folder.join("nested/.GIT/hooks")).unwrap();
        let prepared = fixture.prepare("printf config > nested/.GIT/CONFIG").await;
        std::fs::write(fixture.settings.file_path(), serde_json::to_vec(&serde_json::json!({
            "sandbox":{"enabled":true,"failIfUnavailable":true,"filesystem":{"allowGitConfig":!allowed}}
        })).unwrap()).unwrap();
        let result = fixture.run(prepared).await;
        assert_eq!(result.is_error, !allowed, "{result:?}");
        assert_eq!(folder.join("nested/.GIT/CONFIG").exists(), allowed);
        let result = fixture
            .run(
                fixture
                    .prepare("printf forbidden > nested/.GIT/hooks/pre-commit")
                    .await,
            )
            .await;
        assert!(
            result.is_error && result.content.contains("被沙箱拦截"),
            "{result:?}"
        );
        assert!(!folder.join("nested/.GIT/hooks/pre-commit").exists());
    }
}

#[tokio::test]
async fn sandbox_default_paths_cannot_be_retargeted_through_parent_aliases() {
    for parent in [".git", ".claude", "nested/.GIT", "nested/.CLAUDE"] {
        let fixture =
            SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
        let folder = fixture.workspace.folder.as_ref().unwrap();
        std::fs::create_dir_all(folder.join("ordinary")).unwrap();
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        let name = if parent.to_ascii_lowercase().ends_with(".git") {
            "config"
        } else {
            "settings.json"
        };
        let target = folder.join("ordinary").join(name);
        std::fs::write(&target, "original").unwrap();
        let result = fixture
            .run(
                fixture
                    .prepare(&format!(
                        "ln -s '{}' '{parent}'; printf forbidden > '{parent}/{name}'",
                        folder.join("ordinary").display()
                    ))
                    .await,
            )
            .await;
        assert!(
            result.is_error && result.content.contains("被沙箱拦截"),
            "{parent}: {result:?}"
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "original");
    }
}

fn git(folder: &std::path::Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn sandbox_keeps_git_commit_data_writable_and_config_deny_overrides_allow() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    git(folder, &["init", "--quiet", "--template="]);
    let result = fixture.run(fixture.prepare(
        "printf ordinary > note && git add note && git -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -qm fixture && git show HEAD:note"
    ).await).await;
    assert!(
        !result.is_error && result.content.contains("ordinary"),
        "{result:?}"
    );
    std::fs::write(
        fixture.settings.file_path(),
        serde_json::to_vec(&serde_json::json!({
            "sandbox":{"enabled":true,"failIfUnavailable":true,
                "filesystem":{"allowGitConfig":true,"denyWrite":[".git/config"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    let before = std::fs::read(folder.join(".git/config")).unwrap();
    let result = fixture
        .run(fixture.prepare("printf forbidden > .git/config").await)
        .await;
    assert!(
        result.is_error && result.content.contains("被沙箱拦截"),
        "{result:?}"
    );
    assert_eq!(std::fs::read(folder.join(".git/config")).unwrap(), before);
}

#[tokio::test]
async fn sandbox_keeps_home_startup_files_outside_the_writable_roots() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let home = fixture.root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    for name in [
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".zprofile",
        ".profile",
        ".gitconfig",
        "ordinary",
    ] {
        let path = home.join(name);
        std::fs::write(&path, "original").unwrap();
        let result = fixture
            .run(
                fixture
                    .prepare(&format!("printf forbidden > '{}'", path.display()))
                    .await,
            )
            .await;
        assert!(
            result.is_error && result.content.contains("被沙箱拦截"),
            "{name}: {result:?}"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }
}

#[tokio::test]
async fn sandbox_protected_config_still_runs_without_review_and_is_blocked_by_the_os() {
    for mode in [ExecutionMode::AcceptEdits, ExecutionMode::Auto] {
        let fixture =
            SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
        let folder = fixture.workspace.folder.as_ref().unwrap();
        std::fs::create_dir(folder.join(".claude")).unwrap();
        let (agent, chat) = fixture
            .agent(
                serde_json::json!({"command":"printf forbidden > .claude/settings.json"}),
                mode,
            )
            .await;
        let reviewer = Arc::new(ScriptedModel::new(vec![]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        let (asked, result) = fixture.run_with_human_approval(&agent, chat, false).await;
        assert!(!asked && reviewer.requests.lock().unwrap().is_empty());
        assert!(
            result.is_error && result.content.contains("被沙箱拦截"),
            "{result:?}"
        );
        assert!(!folder.join(".claude/settings.json").exists());
    }
}

#[tokio::test]
async fn sandbox_protected_containers_cannot_be_replaced_or_removed() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    for parent in [".git", ".claude", "nested/.GIT", "nested/.CLAUDE"] {
        std::fs::create_dir_all(folder.join(parent)).unwrap();
        let result = fixture
            .run(
                fixture
                    .prepare(&format!("mv '{parent}' '{parent}-moved'"))
                    .await,
            )
            .await;
        assert!(
            result.is_error && result.content.contains("被沙箱拦截"),
            "{parent}: {result:?}"
        );
        assert!(folder.join(parent).exists());
        assert!(!folder.join(format!("{parent}-moved")).exists());
    }
}
