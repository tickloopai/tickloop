use std::os::unix::fs::PermissionsExt;

use crate::{
    ToolError, Tools, Workspace,
    file_change::{FileChangeLineKind, load_file_change_review},
};
use serde_json::json;
use tickloop_agent_protocol::FileChangeAction;
use tokio_util::sync::CancellationToken;

fn workspace(path: &std::path::Path) -> Workspace {
    Workspace {
        folder: Some(path.to_path_buf()),
        additional_directories: Vec::new(),
    }
}

async fn read(tools: &Tools, directory: &std::path::Path, file_path: &str) {
    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory),
            "Read",
            &json!({"file_path": file_path}),
        )
        .await
        .unwrap();
    tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn create_prepare_persists_a_complete_review_before_execution() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"new.txt","content":"one\n二\n"}),
        )
        .await
        .unwrap();
    let change = prepared.file_change.clone().expect("typed file review");
    let review = load_file_change_review(&change.artifact).unwrap();

    assert_eq!(change.summary, review.summary());
    assert_eq!(review.action, FileChangeAction::Create);
    assert!(!review.before.exists);
    assert!(review.before.identity.is_none());
    assert!(review.after.exists);
    assert!(review.after.identity.is_none());
    assert_eq!(review.after.line_count, 2);
    assert_eq!(change.summary.added_lines, 2);
    assert_eq!(change.summary.removed_lines, 0);
    assert_eq!(review.hunks[0].old_start, 0);
    assert_eq!(review.hunks[0].old_len, 0);
    assert_eq!(review.hunks[0].new_start, 1);
    assert_eq!(
        std::fs::metadata(&change.artifact.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!directory.path().join("new.txt").exists());

    tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("new.txt")).unwrap(),
        "one\n二\n".as_bytes()
    );

    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"another.txt","content":"created by write"}),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.file_change.unwrap().summary.action,
        FileChangeAction::Create
    );
}

#[tokio::test]
async fn overwrite_review_reaches_changes_after_the_old_preview_limit() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.txt");
    let mut before = "same line\n".repeat(7_000);
    before.push_str("old marker beyond sixty four kibibytes\n");
    let after = before.replace(
        "old marker beyond sixty four kibibytes\n",
        "new marker beyond sixty four kibibytes",
    );
    assert!(before.len() > 64 * 1024);
    std::fs::write(&path, &before).unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    read(&tools, directory.path(), "large.txt").await;
    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"large.txt","content":after}),
        )
        .await
        .unwrap();
    let change = prepared.file_change.clone().unwrap();
    let review = load_file_change_review(&change.artifact).unwrap();

    assert_eq!(review.action, FileChangeAction::Overwrite);
    assert!(review.before.ends_with_newline);
    assert!(!review.after.ends_with_newline);
    assert_eq!(change.summary.added_lines, 1);
    assert_eq!(change.summary.removed_lines, 1);
    let changed_text = review
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind != FileChangeLineKind::Context)
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        changed_text,
        [
            "old marker beyond sixty four kibibytes",
            "new marker beyond sixty four kibibytes"
        ]
    );

    tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), after);
}

#[tokio::test]
async fn edit_and_replace_all_are_distinct_and_execute_the_reviewed_result() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "old middle old\n").unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    read(&tools, directory.path(), "notes.txt").await;
    let prepared = tools
        .prepare(&tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Edit",
            &json!({"file_path":"notes.txt","old_string":"old","new_string":"new","replace_all":true}),
        )
        .await
        .unwrap();
    let change = prepared.file_change.clone().unwrap();
    let review = load_file_change_review(&change.artifact).unwrap();

    assert_eq!(review.action, FileChangeAction::ReplaceAll);
    assert_eq!(change.summary.added_lines, 1);
    assert_eq!(change.summary.removed_lines, 1);
    assert_eq!(review.after.sha256, change.summary.after.sha256);
    tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new middle new\n");

    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Edit",
            &json!({"file_path":"notes.txt","old_string":"middle","new_string":"center"}),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.file_change.as_ref().unwrap().summary.action,
        FileChangeAction::Edit
    );
}

#[tokio::test]
async fn changed_identity_rejects_execution_without_changing_the_prepared_review() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "before\n").unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    read(&tools, directory.path(), "notes.txt").await;
    let prepared = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"notes.txt","content":"approved\n"}),
        )
        .await
        .unwrap();
    let change = prepared.file_change.clone().unwrap();
    let approved_review = load_file_change_review(&change.artifact).unwrap();

    std::fs::write(&path, "human change\n").unwrap();
    let error = tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Conflict(_)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "human change\n");
    assert_eq!(
        load_file_change_review(&change.artifact).unwrap(),
        approved_review
    );
}

#[tokio::test]
async fn identical_bound_content_and_identity_produce_identical_typed_reviews() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "before\n").unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    read(&tools, directory.path(), "notes.txt").await;
    let input = json!({"file_path":"notes.txt","content":"after\n"});
    let first = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &input,
        )
        .await
        .unwrap()
        .file_change
        .unwrap();
    let second = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &input,
        )
        .await
        .unwrap()
        .file_change
        .unwrap();

    assert_eq!(first.summary, second.summary);
    assert_eq!(first.artifact.sha256, second.artifact.sha256);
    assert_ne!(first.artifact.path, second.artifact.path);
}

#[tokio::test]
async fn oversized_existing_file_fails_before_a_review_is_created() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("large.txt"),
        vec![b'x'; 8 * 1024 * 1024 + 1],
    )
    .unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    let error = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"large.txt","content":"small"}),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, ToolError::InvalidInput(message) if message == "existing file exceeds 8 MiB review limit")
    );
    assert!(!output.path().join("chat-output/file-changes").exists());
}

#[tokio::test]
async fn artifact_failure_aborts_preparation_before_the_target_is_changed() {
    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let output_dir = output.path().join("chat-output");
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::write(
        output_dir.join(crate::file_change::ARTIFACT_DIRECTORY),
        "occupied",
    )
    .unwrap();
    let tools = Tools::new(output_dir);
    let error = tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"new.txt","content":"never written"}),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ToolError::FileChange(_)));
    assert!(!directory.path().join("new.txt").exists());
}
