mod support;

use std::{path::Path, process::Command, sync::Arc};

use serde_json::json;
use support::{ScriptedModel, call, config, next_event, text};
use tickloop_agent::{
    permissions::{Decision, ExecutionMode, PermissionPolicy},
    runtime::{Agent, EventKind},
    tools::{Workspace, prepare},
};
use uuid::Uuid;

const PIPED_GIT_DIFF: &str = "git diff --cached --stat | head -30";

fn workspace(directory: &Path) -> Workspace {
    Workspace {
        workdir: directory.to_path_buf(),
        additional_directories: Vec::new(),
    }
}

fn decide(workspace: &Workspace, mode: ExecutionMode, command: &str) -> Decision {
    let prepared = prepare(workspace, "Bash", &json!({"command": command})).unwrap();
    PermissionPolicy {
        mode,
        ..PermissionPolicy::default()
    }
    .decide(&prepared, workspace, Uuid::new_v4())
}

#[test]
fn claude_code_read_only_commands_need_no_approval_in_accept_edits_or_auto() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "alpha\n").unwrap();
    let workspace = workspace(directory.path());
    for command in [
        PIPED_GIT_DIFF,
        "cat notes.txt | grep alpha",
        "ls -la && git status",
        "grep -rn \"alpha\" . 2>/dev/null",
        "find . -name '*.txt' | wc -l",
    ] {
        let prepared = prepare(&workspace, "Bash", &json!({"command": command})).unwrap();
        assert!(prepared.is_read_only, "{command}");
        assert!(!prepared.requires_permission, "{command}");
        assert_eq!(
            decide(&workspace, ExecutionMode::Auto, command),
            Decision::Allow,
            "{command}"
        );
        assert_eq!(
            decide(&workspace, ExecutionMode::AcceptEdits, command),
            Decision::Allow,
            "{command}"
        );
    }
}

#[test]
fn read_only_sed_asks_only_in_accept_edits_like_claude_code() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "alpha\n").unwrap();
    let workspace = workspace(directory.path());
    let command = "sed -n '1p' notes.txt";
    let prepared = prepare(&workspace, "Bash", &json!({"command": command})).unwrap();
    assert!(prepared.is_read_only);
    assert!(!prepared.is_allowed_in_accept_edits(&workspace));
    assert!(prepared.accept_edits_supersedes_read_only());
    assert_eq!(
        decide(&workspace, ExecutionMode::AcceptEdits, command),
        Decision::Ask
    );
    assert_eq!(
        decide(&workspace, ExecutionMode::Auto, command),
        Decision::Allow
    );
}

#[test]
fn auto_admits_a_workspace_redirect_through_the_accept_edits_fast_path() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = workspace(directory.path());
    let command = "ls > listing.txt";
    assert!(
        prepare(&workspace, "Bash", &json!({"command": command}))
            .unwrap()
            .requires_permission
    );
    assert_eq!(
        decide(&workspace, ExecutionMode::Auto, command),
        Decision::Allow
    );
}

#[test]
fn commands_outside_claude_code_rules_keep_their_mode_decision() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = workspace(directory.path());
    for command in [
        "cat ../outside.txt",
        "cd src && git status",
        "echo $(git rev-parse HEAD)",
        "git branch release",
        "find . -delete",
    ] {
        assert!(
            prepare(&workspace, "Bash", &json!({"command": command}))
                .unwrap()
                .requires_permission,
            "{command}"
        );
        assert_eq!(
            decide(&workspace, ExecutionMode::Auto, command),
            Decision::Check,
            "{command}"
        );
    }
    assert_eq!(
        decide(
            &workspace,
            ExecutionMode::AcceptEdits,
            "touch notes.txt && rm -rf build"
        ),
        Decision::Allow
    );
    assert_eq!(
        decide(&workspace, ExecutionMode::AcceptEdits, "ls > listing.txt"),
        Decision::Allow
    );
    for command in ["cp -r src dst", "rm -rf *", "touch ../outside.txt"] {
        assert_eq!(
            decide(&workspace, ExecutionMode::AcceptEdits, command),
            Decision::Ask,
            "{command}"
        );
    }
}

#[tokio::test]
async fn piped_git_diff_runs_without_approval_in_accept_edits_and_auto() {
    for mode in [ExecutionMode::AcceptEdits, ExecutionMode::Auto] {
        let directory = tempfile::tempdir().unwrap();
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(initialized.success());
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("diff", "Bash", json!({"command": PIPED_GIT_DIFF})),
                text("Reviewed the staged diff."),
            ])),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.set_execution_mode(chat, mode).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, None, "Summarize the staged changes".into())
            .await
            .unwrap();
        let mut completed_tool = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    panic!("{mode:?} asked for approval of {}", request.input)
                }
                EventKind::ToolCompleted { result, .. } => {
                    assert!(!result.is_error, "{}", result.content);
                    completed_tool = true;
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(completed_tool, "{mode:?} did not run the command");
        agent.shutdown().await;
    }
}
