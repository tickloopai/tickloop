mod support;

use std::{collections::HashSet, sync::Arc};
use support::{ScriptedModel, call, config, next_event, text};
use tickloop_agent::{
    model::{Content, ModelRequest, Protocol, Role, cards::model_for_persistent_selection},
    permissions::{Decision, PermissionRule},
    runtime::{Agent, AgentError, EventKind, ModelChange, Receipt},
};
use uuid::Uuid;

async fn require_workspace_write_approval(agent: &Agent, chat: Uuid) {
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
}

async fn completed(
    agent: &Agent,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
    receipt: &Receipt,
) {
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => return,
            EventKind::Failed(error) => panic!(
                "{error}; snapshot: {:?}",
                agent.snapshot(receipt.chat_id).await.unwrap().loops
            ),
            _ => {}
        }
    }
}

fn assert_paired_history(request: &ModelRequest) {
    let mut seen = HashSet::new();
    for (index, message) in request.messages.iter().enumerate() {
        let calls = message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolUse { id, .. } => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            continue;
        }
        assert_eq!(message.role, Role::Assistant);
        assert!(
            calls.iter().all(|id| seen.insert((*id).clone())),
            "different runs need unique wire identities"
        );
        let following = &request.messages[index + 1];
        assert_eq!(following.role, Role::User);
        let results = following
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls, results,
            "each assistant call group needs immediately paired results"
        );
    }
}

#[tokio::test]
async fn raw_legacy_model_name_change_keeps_same_source_private_history() {
    let directory = tempfile::tempdir().unwrap();
    let raw = config();
    assert!(raw.card.is_none());
    let private = Content::Thinking {
        thinking: "private reasoning from the first name".into(),
        signature: Some("signed-private-state".into()),
    };
    let first_client = Arc::new(ScriptedModel::new(vec![vec![
        private.clone(),
        Content::Text {
            text: "First answer".into(),
        },
    ]]));
    let agent = Agent::new_embedded(raw.clone(), first_client);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "First prompt".into())
        .await
        .unwrap();
    completed(&agent, &mut events, &first).await;
    let initial = agent.snapshot(chat).await.unwrap();
    assert!(initial.model.card.is_some());
    assert!(!initial.model.provider_id.is_empty());

    let mut renamed = raw;
    renamed.name = "renamed fixture".into();
    let second_client = Arc::new(ScriptedModel::new(vec![text("Second answer")]));
    assert_eq!(
        agent
            .set_model(chat, renamed, second_client.clone())
            .await
            .unwrap(),
        ModelChange::Applied
    );
    let selected = agent.snapshot(chat).await.unwrap();
    assert_eq!(selected.model.name, "renamed fixture");
    assert_eq!(selected.model.provider_id, initial.model.provider_id);
    assert!(
        selected
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| *content == private)
    );

    let next = agent
        .submit(
            chat,
            Some(first.loop_id),
            "Continue after the name change".into(),
        )
        .await
        .unwrap();
    completed(&agent, &mut events, &next).await;
    assert!(
        second_client.requests.lock().unwrap()[0]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| *content == private)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn waiting_goals_project_paired_history_without_changing_raw_tool_identity_or_private_state()
{
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.txt"), "shared source value").unwrap();
        let private = match protocol {
            Protocol::Responses => Content::ProtocolData {
                protocol,
                value: serde_json::json!({"type":"reasoning","id":"reasoning-a","summary":[],"encrypted_content":"opaque-state-must-stay"}),
            },
            _ => Content::Thinking {
                thinking: "private state retained verbatim".into(),
                signature: (protocol == Protocol::Messages).then(|| "original-signature".into()),
            },
        };
        let mut first = vec![private.clone()];
        first.extend(call(
            "shared-id",
            "Write",
            serde_json::json!({"file_path":"a.txt","content":"A result"}),
        ));
        first.extend(call(
            "second-a",
            "Read",
            serde_json::json!({"file_path":"source.txt"}),
        ));
        let model = Arc::new(ScriptedModel::new(vec![
            first,
            call(
                "shared-id",
                "Read",
                serde_json::json!({"file_path":"source.txt"}),
            ),
            text("B finished"),
            text("A finished"),
        ]));
        let mut profile = config();
        profile.protocol = protocol;
        profile = model_for_persistent_selection(profile);
        let agent = Agent::new_embedded(profile.clone(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        require_workspace_write_approval(&agent, chat).await;
        let mut events = agent.subscribe();
        let a = agent
            .submit(chat, None, "Write A then read source".into())
            .await
            .unwrap();
        let permission = loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        let b = agent
            .submit(chat, None, "Independently read source for B".into())
            .await
            .unwrap();
        completed(&agent, &mut events, &b).await;
        assert!(!directory.path().join("a.txt").exists());
        assert!(matches!(agent.compact(chat).await, Err(AgentError::Busy)));
        let pending_requests = model.requests.lock().unwrap().clone();
        assert_eq!(pending_requests.len(), 3);
        for request in &pending_requests {
            assert_paired_history(request);
        }
        assert!(pending_requests[1].messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::ToolResult{content,..} if content.text().contains("pending"))));
        assert!(
            pending_requests[1]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| *content == private)
        );
        let a_alias = pending_requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolUse { id, name, .. } if name == "Write" => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        let replacement = Arc::new(ScriptedModel::new(vec![
            text("Summary after both interleaved goals completed"),
            text("New selection is now active"),
        ]));
        profile.name = "next-profile".into();
        profile.card.as_mut().unwrap().display_name = profile.name.clone();
        assert_eq!(
            agent
                .set_model(chat, profile.clone(), replacement.clone())
                .await
                .unwrap(),
            ModelChange::Pending
        );
        assert_ne!(agent.snapshot(chat).await.unwrap().model.name, profile.name);
        agent
            .answer_permission(chat, permission.id, true)
            .await
            .unwrap();
        completed(&agent, &mut events, &a).await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "A result"
        );
        assert!(replacement.requests.lock().unwrap().is_empty());
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 4);
        assert_paired_history(&requests[3]);
        assert!(requests[3].messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::ToolUse{id,name,..} if name=="Write"&&id==&a_alias)));
        assert!(requests[3].messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::Text{text} if text.contains("became available")&&text.contains(&a_alias))));
        let raw = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            raw.messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|content| matches!(content,Content::ToolUse{id,..} if id=="shared-id"))
                .count(),
            2
        );
        assert!(
            raw.messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| *content == private)
        );
        let outcome = agent.compact(chat).await.unwrap();
        assert_paired_history(&ModelRequest {
            messages: outcome.snapshot.conversation_messages().unwrap(),
            tools: vec![],
            structured_output: None,
            temperature: None,
            reasoning_effort: None,
        });
        // 全量压缩在原始对话之后追加边界与摘要。
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .messages
                .starts_with(&raw.messages)
        );
        let next = agent
            .submit(chat, Some(a.loop_id), "Continue".into())
            .await
            .unwrap();
        completed(&agent, &mut events, &next).await;
        assert_eq!(replacement.requests.lock().unwrap().len(), 2);
        agent.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "isolated restart fixture invoked by the paired waiting-goal recovery test"]
async fn overlapping_waits_process_fixture() {
    let Ok(directory) = std::env::var("TICKLOOP_OVERLAPPING_WAIT_DIRECTORY") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "same-provider-id",
            "Write",
            serde_json::json!({"file_path":"a.txt","content":"A"}),
        ),
        call(
            "same-provider-id",
            "Write",
            serde_json::json!({"file_path":"b.txt","content":"B"}),
        ),
    ]));
    let agent = Agent::open_embedded(directory.join("archive"), config(), model).unwrap();
    let chat = agent.create_chat(directory.clone()).await.unwrap();
    require_workspace_write_approval(&agent, chat).await;
    let mut events = agent.subscribe();
    let a = agent.submit(chat, None, "Write A".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let b = agent
        .submit(chat, None, "Independently write B".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } if request.loop_id == b.loop_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    std::fs::write(
        directory.join("receipts.json"),
        serde_json::to_vec(&[a, b]).unwrap(),
    )
    .unwrap();
    agent.flush_transcripts().await;
    std::process::exit(0);
}

#[tokio::test]
async fn reopening_two_waiting_runs_drops_their_unanswered_calls_without_replaying_either() {
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "overlapping_waits_process_fixture",
            "--nocapture",
        ])
        .env("TICKLOOP_OVERLAPPING_WAIT_DIRECTORY", directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let receipts: Vec<Receipt> =
        serde_json::from_slice(&std::fs::read(directory.path().join("receipts.json")).unwrap())
            .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text(
        "A resumed without repeating either command",
    )]));
    let agent =
        Agent::open_embedded(directory.path().join("archive"), config(), model.clone()).unwrap();
    let before = agent.resume_chat(receipts[0].chat_id).await.unwrap();
    assert!(before.permissions.is_empty());
    assert!(
        before
            .loops
            .iter()
            .all(|goal| goal.state == tickloop_agent::runtime::LoopState::Interrupted)
    );
    assert!(model.requests.lock().unwrap().is_empty());
    let mut events = agent.subscribe();
    let resumed = agent
        .submit(
            receipts[0].chat_id,
            Some(receipts[0].loop_id),
            "Continue A without old commands".into(),
        )
        .await
        .unwrap();
    completed(&agent, &mut events, &resumed).await;
    let request = model.requests.lock().unwrap()[0].clone();
    assert_paired_history(&request);
    // 照 Claude Code：等待权限时中断、没有结果的 tool_use 在恢复时从对话中过滤。
    assert!(
        !request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::ToolUse { .. } | Content::ToolResult { .. }
            ))
    );
    assert!(!directory.path().join("a.txt").exists());
    assert!(!directory.path().join("b.txt").exists());
    assert_eq!(
        agent
            .snapshot(receipts[0].chat_id)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == receipts[1].loop_id)
            .unwrap()
            .state,
        tickloop_agent::runtime::LoopState::Interrupted
    );
    agent.shutdown().await;
}
