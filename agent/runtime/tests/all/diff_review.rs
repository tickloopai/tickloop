use std::sync::Arc;
use tickloop_agent_settings::local_agent::LocalAgent;

use serde_json::json;
use tickloop_agent_protocol::{
    ChatSnapshot, Content, Decision, EventKind, FileChangeAction, OperationState,
    PermissionRequest, PermissionRule, PreparedFileChange,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tickloop_agent_tools::file_change::{
    FileChangeArtifactError, FileChangeLineKind, load_file_change_review,
};
use uuid::Uuid;

async fn require_file_change_approval(agent: &Agent, chat: Uuid, tool: &str) {
    agent
        .set_permission_rules(
            tickloop_ids::Id::from(chat),
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: tool.into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
}

async fn next_permission(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) -> PermissionRequest {
    loop {
        match next_event(events).await.kind {
            EventKind::PermissionRequested { request } => return request,
            EventKind::Failed(error) => panic!("run failed before requesting permission: {error}"),
            EventKind::Completed { .. } => panic!("run completed without requesting permission"),
            _ => {}
        }
    }
}

async fn wait_for_tool_error(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    expected_call_id: &str,
) -> String {
    loop {
        match next_event(events).await.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == expected_call_id => {
                assert!(result.is_error, "failed file change succeeded");
                return result.content;
            }
            EventKind::Failed(error) => panic!("run failed before the tool result: {error}"),
            EventKind::Completed { .. } => panic!("run completed without the tool result"),
            _ => {}
        }
    }
}

async fn wait_for_completion(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) {
    loop {
        match next_event(events).await.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("file change failed: {error}"),
            _ => {}
        }
    }
}

fn completed_file_changes(snapshot: &ChatSnapshot) -> Vec<&PreparedFileChange> {
    snapshot
        .operations
        .iter()
        .filter(|operation| operation.state == OperationState::Completed)
        .filter_map(|operation| operation.file_change.as_ref())
        .collect()
}

#[tokio::test]
async fn permission_and_operation_share_the_immutable_prepared_review() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.txt");
    std::fs::write(&path, "Delivery Monday\n").unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            [
                call("read-report", "Read", json!({"file_path":"report.txt"})),
                call(
                    "edit-report",
                    "Edit",
                    json!({"file_path":"report.txt","old_string":"Monday","new_string":"Friday"}),
                ),
            ]
            .concat(),
            text("done"),
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_file_change_approval(&agent, chat.uuid(), "Edit").await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Correct the delivery day".into())
        .await
        .unwrap();

    let permission = next_permission(&mut events).await;
    let operation = agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .into_iter()
        .find(|operation| operation.call_id == permission.execution.call_key())
        .unwrap();
    assert_eq!(
        permission.file_change.as_deref(),
        operation.file_change.as_ref()
    );
    let prepared = permission.file_change.as_deref().unwrap();
    let approved_review = load_file_change_review(&prepared.artifact).unwrap();
    assert_eq!(prepared.summary, approved_review.summary());
    assert_eq!(approved_review.action, FileChangeAction::Edit);
    assert!(
        approved_review
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| line.kind == FileChangeLineKind::Removed && line.text == "Delivery Monday")
    );
    assert!(
        approved_review
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| line.kind == FileChangeLineKind::Added && line.text == "Delivery Friday")
    );

    std::fs::write(&path, "A human changed this document\n").unwrap();
    assert_eq!(
        load_file_change_review(&prepared.artifact).unwrap(),
        approved_review
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    let error = wait_for_tool_error(&mut events, "edit-report").await;
    assert!(error.contains("file changed"), "{error}");
    wait_for_completion(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "A human changed this document\n"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let edit = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "edit-report")
        .unwrap();
    assert_eq!(edit.state, OperationState::Failed);
    assert_eq!(edit.file_change.as_ref(), Some(prepared));
    assert!(completed_file_changes(&snapshot).is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn change_journal_contains_only_successful_file_operations() {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first.txt");
    let second_path = directory.path().join("second.txt");
    std::fs::write(&first_path, "first before\n").unwrap();
    std::fs::write(&second_path, "second before\n").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::ToolUse {
                id: "read-only".into(),
                name: "Read".into(),
                input: json!({"file_path":"first.txt"}),
            },
            Content::ToolUse {
                id: "read-second".into(),
                name: "Read".into(),
                input: json!({"file_path":"second.txt"}),
            },
            Content::ToolUse {
                id: "successful-edit".into(),
                name: "Edit".into(),
                input: json!({"file_path":"first.txt","old_string":"before","new_string":"after"}),
            },
            Content::ToolUse {
                id: "conflicting-edit".into(),
                name: "Edit".into(),
                input: json!({"file_path":"second.txt","old_string":"before","new_string":"after"}),
            },
        ],
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_file_change_approval(&agent, chat.uuid(), "Edit").await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read and update both files".into())
        .await
        .unwrap();

    let first_permission = next_permission(&mut events).await;
    assert_eq!(first_permission.execution.call_key(), "successful-edit");
    agent
        .answer_permission(chat, first_permission.id, true)
        .await
        .unwrap();
    let second_permission = next_permission(&mut events).await;
    assert_eq!(second_permission.execution.call_key(), "conflicting-edit");
    std::fs::write(&second_path, "external replacement\n").unwrap();
    agent
        .answer_permission(chat, second_permission.id, true)
        .await
        .unwrap();
    let error = wait_for_tool_error(&mut events, "conflicting-edit").await;
    assert!(error.contains("file changed"), "{error}");
    wait_for_completion(&mut events).await;

    let snapshot = agent.snapshot(chat).await.unwrap();
    let read = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "read-only")
        .unwrap();
    assert_eq!(read.state, OperationState::Completed);
    assert!(read.file_change.is_none());
    let successful = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "successful-edit")
        .unwrap();
    assert_eq!(successful.state, OperationState::Completed);
    assert!(successful.file_change.is_some());
    let conflicting = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "conflicting-edit")
        .unwrap();
    assert_eq!(conflicting.state, OperationState::Failed);
    assert!(conflicting.file_change.is_some());
    let journal = completed_file_changes(&snapshot);
    assert_eq!(journal, vec![successful.file_change.as_ref().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(first_path).unwrap(),
        "first after\n"
    );
    assert_eq!(
        std::fs::read_to_string(second_path).unwrap(),
        "external replacement\n"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn saved_reviews_report_missing_or_corrupt_sidecars_and_are_not_listed_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let agent = Agent::open_embedded(
        state.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "create-report",
                "Write",
                json!({"file_path":"created.txt","content":"created content\n"}),
            ),
            text("created"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_file_change_approval(&agent, chat.uuid(), "Write").await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Create the report".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let original_reference = agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .into_iter()
        .find(|operation| operation.call_id == "create-report")
        .unwrap()
        .file_change
        .unwrap();
    let original_review = load_file_change_review(&original_reference.artifact).unwrap();
    assert_eq!(original_review.action, FileChangeAction::Create);
    agent.shutdown().await;
    drop(agent);

    let artifact_path = original_reference.artifact.path.clone();
    let original_bytes = std::fs::read(&artifact_path).unwrap();
    std::fs::remove_file(&artifact_path).unwrap();
    let missing = load_file_change_review(&original_reference.artifact).unwrap_err();
    assert!(
        matches!(missing, FileChangeArtifactError::Missing(ref path) if path == &artifact_path)
    );
    assert!(missing.to_string().contains("artifact is missing"));

    let mut corrupt_bytes = original_bytes;
    corrupt_bytes[0] ^= 1;
    std::fs::write(&artifact_path, corrupt_bytes).unwrap();
    let corrupt = load_file_change_review(&original_reference.artifact).unwrap_err();
    assert!(
        matches!(corrupt, FileChangeArtifactError::Corrupt { ref path, .. } if path == &artifact_path)
    );
    assert!(corrupt.to_string().contains("artifact is corrupt"));

    // Following Claude Code, operation records live only in this process's memory: after a restart,
    // `/diff` no longer lists previously saved Agent changes.
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let restored = Agent::open_embedded(state, config(), model.clone()).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(completed_file_changes(&snapshot).is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    restored.shutdown().await;
}

#[tokio::test]
async fn missing_review_artifact_cannot_be_approved_but_can_still_be_denied() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("unreviewed.txt");
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![call(
            "unreviewed-create",
            "Write",
            json!({"file_path":"unreviewed.txt","content":"must be reviewed\n"}),
        )])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_file_change_approval(&agent, chat.uuid(), "Write").await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Create a reviewed file".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    let artifact = permission
        .file_change
        .as_ref()
        .unwrap()
        .artifact
        .path
        .clone();
    std::fs::remove_file(&artifact).unwrap();

    let error = agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("review is unavailable"), "{error}");
    assert!(error.contains("artifact is missing"), "{error}");
    assert!(!target.exists());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .permissions
            .iter()
            .any(|request| request.id == permission.id)
    );
    assert_eq!(snapshot.operations[0].state, OperationState::Proposed);

    agent
        .answer_permission(chat, permission.id, false)
        .await
        .unwrap();
    let error = wait_for_tool_error(&mut events, "unreviewed-create").await;
    assert!(error.contains("denied"), "{error}");
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("denial failed the run: {error}"),
            EventKind::Completed { .. } => panic!("denial without a comment continued the run"),
            _ => {}
        }
    }
    assert!(!target.exists());
    agent.shutdown().await;
}
