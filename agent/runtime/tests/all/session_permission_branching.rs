use std::sync::Arc;

use serde_json::json;
use tickloop_agent_protocol::{
    BranchRequest, EventKind, PermissionRequest, PermissionScope, RunId,
};
use tickloop_agent_runtime::{Agent, AgentError};
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use uuid::Uuid;

async fn permission_for_run(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: RunId,
) -> PermissionRequest {
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => return request,
            EventKind::Failed(error) => panic!("run failed before requesting permission: {error}"),
            EventKind::Completed { .. } => panic!("run completed without requesting permission"),
            _ => {}
        }
    }
}

async fn completed_without_permission(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: RunId,
) {
    let mut tool_started = false;
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { .. } => {
                panic!("the inherited session allowance unexpectedly prompted")
            }
            EventKind::ToolStarted { .. } => tool_started = true,
            EventKind::Completed { .. } => {
                assert!(tool_started, "the expected tool never reached admission");
                return;
            }
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

async fn wait_for_completion(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: RunId,
) {
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

async fn wait_for_denial_interruption(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: RunId,
) {
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::Interrupted => return,
            EventKind::Failed(error) => {
                panic!("denied run failed instead of interrupting: {error}")
            }
            EventKind::Completed { .. } => panic!("denied run unexpectedly completed"),
            EventKind::ToolStarted { .. } => panic!("denied operation reached admission"),
            _ => {}
        }
    }
}

async fn denied_without_permission(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: RunId,
) {
    let mut denial_returned = false;
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { .. } => {
                panic!("the inherited session denial unexpectedly prompted")
            }
            EventKind::ToolStarted { .. } => panic!("session-denied operation reached admission"),
            EventKind::ToolCompleted { result, .. } => denial_returned = result.is_error,
            EventKind::Completed { .. } => {
                assert!(
                    denial_returned,
                    "the session denial never reached the model"
                );
                return;
            }
            EventKind::Failed(error) => panic!("session-denied run failed: {error}"),
            _ => {}
        }
    }
}

fn read(path: &std::path::Path, call_id: &str) -> Vec<tickloop_agent_protocol::Content> {
    call(call_id, "Read", json!({ "file_path": path }))
}

#[tokio::test]
async fn session_allowance_follows_each_new_loop_in_the_child_chat() {
    let workspace = tempfile::tempdir().unwrap();
    let changed_workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source_path = external.path().join("source.txt");
    std::fs::write(&source_path, "branch permission fact").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        read(&source_path, "source-read"),
        text("source complete"),
        read(&source_path, "child-read"),
        text("child complete"),
        call(
            "changed-input-read",
            "Read",
            json!({ "file_path": &source_path, "offset": 1 }),
        ),
        read(&source_path, "independent-read"),
        text("independent complete"),
        read(&source_path, "changed-directory-read"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let source_chat_id = agent.create_chat(workspace.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source = agent
        .submit(source_chat_id, None, "read the external source".into())
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, source.run_id).await;
    agent
        .answer_permission_with_scope(
            source_chat_id,
            permission.id,
            true,
            PermissionScope::Session,
        )
        .await
        .unwrap();
    wait_for_completion(&mut events, source.run_id).await;
    assert!(
        agent
            .snapshot(source_chat_id)
            .await
            .unwrap()
            .policy
            .rules
            .is_empty(),
        "process-only session authority must not enter the Chat snapshot"
    );

    let child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            source_loop_id: Some(source.loop_id),
            name: Some("permission child".into()),
        })
        .await
        .unwrap();
    let continuation = agent
        .submit(
            child.child_chat_id,
            None,
            "continue the selected session".into(),
        )
        .await
        .unwrap();
    completed_without_permission(&mut events, continuation.run_id).await;

    let changed_input = agent
        .submit(
            child.child_chat_id,
            Some(continuation.loop_id),
            "read with a different normalized input".into(),
        )
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, changed_input.run_id).await;
    agent
        .answer_permission(child.child_chat_id, permission.id, false)
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, changed_input.run_id).await;

    let independent = agent
        .submit(child.child_chat_id, None, "start a separate loop".into())
        .await
        .unwrap();
    assert_ne!(independent.loop_id, continuation.loop_id);
    completed_without_permission(&mut events, independent.run_id).await;

    let changed_directory_child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            source_loop_id: Some(source.loop_id),
            name: Some("changed directory child".into()),
        })
        .await
        .unwrap();
    agent
        .set_directories(
            changed_directory_child.child_chat_id,
            changed_workspace.path().into(),
            Vec::new(),
        )
        .await
        .unwrap();
    let changed_directory = agent
        .submit(
            changed_directory_child.child_chat_id,
            None,
            "read after changing workspace roots".into(),
        )
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, changed_directory.run_id).await;
    agent
        .answer_permission(changed_directory_child.child_chat_id, permission.id, false)
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, changed_directory.run_id).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn missing_source_selection_and_restart_do_not_recreate_session_authority() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source_path = external.path().join("source.txt");
    std::fs::write(&source_path, "ephemeral permission fact").unwrap();
    let state = workspace.path().join("state");
    let agent = Agent::open_embedded(
        state.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            read(&source_path, "source-read"),
            text("source complete"),
            read(&source_path, "unselected-read"),
            text("unselected denied"),
        ])),
    )
    .unwrap();
    let source_chat_id = agent.create_chat(workspace.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source = agent
        .submit(source_chat_id, None, "read the external source".into())
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, source.run_id).await;
    agent
        .answer_permission_with_scope(
            source_chat_id,
            permission.id,
            true,
            PermissionScope::Session,
        )
        .await
        .unwrap();
    wait_for_completion(&mut events, source.run_id).await;

    let unselected = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            source_loop_id: None,
            name: Some("unselected".into()),
        })
        .await
        .unwrap();
    let unselected_run = agent
        .submit(
            unselected.child_chat_id,
            None,
            "try without a selected loop".into(),
        )
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, unselected_run.run_id).await;
    agent
        .answer_permission(unselected.child_chat_id, permission.id, false)
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, unselected_run.run_id).await;

    let selected = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            source_loop_id: Some(source.loop_id),
            name: Some("restart child".into()),
        })
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(
        state,
        config(),
        Arc::new(ScriptedModel::new(vec![
            read(&source_path, "restarted-read"),
            text("restart denied"),
        ])),
    )
    .unwrap();
    reopened.resume_chat(selected.child_chat_id).await.unwrap();
    let mut events = reopened.subscribe();
    let restarted = reopened
        .submit(
            selected.child_chat_id,
            None,
            "continue after process restart".into(),
        )
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, restarted.run_id).await;
    reopened
        .answer_permission(selected.child_chat_id, permission.id, false)
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, restarted.run_id).await;
    reopened.shutdown().await;
}

#[tokio::test]
async fn once_answer_does_not_follow_a_selected_loop_into_its_child() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source_path = external.path().join("source.txt");
    std::fs::write(&source_path, "one-time permission fact").unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            read(&source_path, "source-once"),
            text("source complete"),
            read(&source_path, "child-must-ask"),
        ])),
    );
    let source_chat_id = agent.create_chat(workspace.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source = agent
        .submit(source_chat_id, None, "read once".into())
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, source.run_id).await;
    agent
        .answer_permission(source_chat_id, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&mut events, source.run_id).await;

    let child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            source_loop_id: Some(source.loop_id),
            name: Some("once child".into()),
        })
        .await
        .unwrap();
    let child_run = agent
        .submit(child.child_chat_id, None, "read again in child".into())
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, child_run.run_id).await;
    agent
        .answer_permission(child.child_chat_id, permission.id, false)
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, child_run.run_id).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn pending_permission_cannot_be_branched_as_session_authority() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source_path = external.path().join("source.txt");
    std::fs::write(&source_path, "pending permission fact").unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![read(&source_path, "pending-read")])),
    );
    let source_chat_id = agent.create_chat(workspace.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source = agent
        .submit(source_chat_id, None, "wait for permission".into())
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, source.run_id).await;

    assert!(matches!(
        agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id,
                source_loop_id: Some(source.loop_id),
                name: Some("pending child".into()),
            })
            .await,
        Err(AgentError::Busy)
    ));
    assert_eq!(agent.list_chats().await.len(), 1);
    agent
        .answer_permission(source_chat_id, permission.id, false)
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, source.run_id).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn session_deny_is_process_only_and_follows_the_selected_owner() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source_path = external.path().join("source.txt");
    std::fs::write(&source_path, "denied permission fact").unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            read(&source_path, "initial-deny"),
            read(&source_path, "same-owner-deny"),
            text("same owner denied"),
            read(&source_path, "child-deny"),
            text("child denied"),
        ])),
    );
    let source_chat_id = agent.create_chat(workspace.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source = agent
        .submit(source_chat_id, None, "request denied read".into())
        .await
        .unwrap();
    let permission = permission_for_run(&mut events, source.run_id).await;
    agent
        .answer_permission_with_scope(
            source_chat_id,
            permission.id,
            false,
            PermissionScope::Session,
        )
        .await
        .unwrap();
    wait_for_denial_interruption(&mut events, source.run_id).await;
    assert!(
        agent
            .snapshot(source_chat_id)
            .await
            .unwrap()
            .policy
            .rules
            .is_empty()
    );

    let same_owner = agent
        .submit(
            source_chat_id,
            Some(source.loop_id),
            "repeat the denied read".into(),
        )
        .await
        .unwrap();
    denied_without_permission(&mut events, same_owner.run_id).await;

    let child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            source_loop_id: Some(source.loop_id),
            name: Some("denied child".into()),
        })
        .await
        .unwrap();
    let child_run = agent
        .submit(
            child.child_chat_id,
            None,
            "repeat denied read in child".into(),
        )
        .await
        .unwrap();
    denied_without_permission(&mut events, child_run.run_id).await;
    agent.shutdown().await;
}
