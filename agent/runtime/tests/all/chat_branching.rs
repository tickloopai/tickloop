use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    BranchPublicItem, BranchRequest, ChatSnapshot, CommandState, Content, DirectedSummaryRequest,
    EventKind, InputContext, Message, ModelIdentity, Protocol, SummaryDirection, Usage,
    UserShellState,
};
use tickloop_agent_runtime::{Agent, AgentError};
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn wait_for_run(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: Uuid,
) {
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } | EventKind::Interrupted => return,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

async fn wait_for_failed_run(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run_id: Uuid,
) {
    loop {
        let event = next_event(events).await;
        if event.run_id == run_id
            && matches!(event.kind, EventKind::Failed(_) | EventKind::Interrupted)
        {
            return;
        }
    }
}

fn tool_use_ids(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn branch_is_insert_only_idempotent_and_continues_with_fresh_identity() {
    let directory = tempfile::tempdir().unwrap();
    let additional = tempfile::tempdir().unwrap();
    let archive = directory.path().join("state");
    let attachment_path = directory.path().join("source-context.txt");
    let attachment_content = "stored attachment survives source file removal";
    std::fs::write(&attachment_path, attachment_content).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("source answer"),
        text("child answer"),
    ]));
    let agent = Agent::open_embedded(archive.clone(), config(), model.clone()).unwrap();
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_directories(
            source_chat_id,
            directory.path().into(),
            vec![additional.path().into()],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let source_receipt = agent
        .submit_with_file_references(
            source_chat_id,
            "source prompt".into(),
            vec![attachment_path.clone()],
        )
        .await
        .unwrap();
    wait_for_run(&mut events, source_receipt.run_id).await;
    let source_before = agent.snapshot(source_chat_id).await.unwrap();
    let stored_context = source_before.message_contexts[&0][0].clone();
    assert!(matches!(
        &stored_context,
        InputContext::File(file) if file.content == attachment_content
    ));
    std::fs::remove_file(&attachment_path).unwrap();
    let source_json = serde_json::to_value(&source_before).unwrap();
    let requests_before_branch = model.requests.lock().unwrap().len();
    let request = BranchRequest {
        request_id: Uuid::new_v4(),
        source_chat_id,
        name: Some("研究分支".into()),
    };

    let outcome = agent.branch_chat(request.clone()).await.unwrap();

    assert_ne!(outcome.child_chat_id, source_chat_id);
    assert_eq!(outcome.name, "研究分支 (Branch)");
    assert_eq!(outcome.source_chat_id, source_chat_id);
    assert_eq!(outcome.snapshot.id, outcome.child_chat_id);
    assert_eq!(outcome.snapshot.branch_name(), Some("研究分支 (Branch)"));
    let origin = outcome.snapshot.branch_origin.as_ref().unwrap();
    assert_eq!(origin.source_chat_id, source_chat_id);
    assert_eq!(origin.name, "研究分支 (Branch)");
    let origin = origin.creation.as_ref().unwrap();
    assert_eq!(origin.source_loop_id, source_receipt.loop_id);
    assert_eq!(origin.request_id, request.request_id);
    assert_eq!(origin.requested_name.as_deref(), Some("研究分支"));
    assert_eq!(origin.source_raw_boundary, source_before.messages.len());
    assert_eq!(origin.source_public_boundary, origin.public_seed.len());
    assert!(origin.public_seed.iter().any(|item| matches!(
        item,
        BranchPublicItem::Message { contexts, .. }
            if contexts == std::slice::from_ref(&stored_context)
    )));
    assert_eq!(outcome.snapshot.model, source_before.model);
    assert_eq!(outcome.snapshot.folder, source_before.folder);
    assert_eq!(
        outcome.snapshot.additional_directories,
        source_before.additional_directories
    );
    assert_eq!(
        outcome.snapshot.conversation_messages().unwrap(),
        source_before.conversation_messages().unwrap()
    );
    assert_eq!(
        outcome.snapshot.public_conversation_items().unwrap(),
        source_before.public_conversation_items().unwrap()
    );
    // 源 Chat 的 Loop 复制到子 Chat。
    assert_eq!(outcome.snapshot.chat_loop.id, source_receipt.loop_id);
    assert!(outcome.snapshot.request_usage.entries().is_empty());
    assert_eq!(
        outcome.snapshot.rewind_points(),
        source_before.rewind_points()
    );
    assert!(outcome.snapshot.operations.is_empty());
    assert!(outcome.snapshot.pending_inputs.is_empty());
    assert!(outcome.snapshot.pending_commands.is_empty());
    assert!(outcome.snapshot.permissions.is_empty());
    assert!(outcome.snapshot.questions.is_empty());
    assert!(outcome.snapshot.commands.is_empty());
    assert!(outcome.snapshot.user_shells.is_empty());
    assert!(outcome.snapshot.events.is_empty());
    assert!(outcome.snapshot.pending_model.is_none());
    assert_eq!(model.requests.lock().unwrap().len(), requests_before_branch);
    assert_eq!(
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
        source_json
    );

    let retry = agent.branch_chat(request.clone()).await.unwrap();
    assert_eq!(retry.child_chat_id, outcome.child_chat_id);
    assert_eq!(agent.list_chats().await.len(), 2);
    assert!(matches!(
        agent
            .branch_chat(BranchRequest {
                name: Some("different".into()),
                ..request.clone()
            })
            .await,
        Err(AgentError::Invalid(message)) if message.contains("different input")
    ));

    let child_receipt = agent
        .submit(outcome.child_chat_id, "continue in child".into())
        .await
        .unwrap();
    assert_eq!(child_receipt.loop_id, source_receipt.loop_id);
    assert_ne!(child_receipt.run_id, source_receipt.run_id);
    wait_for_run(&mut events, child_receipt.run_id).await;
    let child = agent.snapshot(outcome.child_chat_id).await.unwrap();
    assert_eq!(child.chat_loop.run_id, Some(child_receipt.run_id));
    assert_eq!(
        agent
            .snapshot(source_chat_id)
            .await
            .unwrap()
            .chat_loop
            .run_id,
        Some(source_receipt.run_id)
    );
    assert_eq!(child.request_usage.entries().len(), 1);
    assert_eq!(child.rewind_points().len(), 2);
    {
        let requests = model.requests.lock().unwrap();
        let child_request_text = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(child_request_text.contains("source-context.txt"));
        assert!(child_request_text.contains(attachment_content));
    }
    let child_public = child.public_conversation_items().unwrap();
    let grandchild = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: outcome.child_chat_id,
            name: Some("nested branch".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        grandchild.snapshot.public_conversation_items().unwrap(),
        child_public
    );
    assert_eq!(
        grandchild.snapshot.conversation_messages().unwrap(),
        child.conversation_messages().unwrap()
    );
    let provenance = grandchild
        .snapshot
        .branch_origin
        .as_ref()
        .and_then(|origin| origin.creation.as_ref())
        .unwrap()
        .public_seed
        .iter()
        .filter_map(|item| match item {
            BranchPublicItem::Message { source, .. } if source.run_id.is_some() => {
                Some(source.chat_id)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    assert!(provenance.contains(&source_chat_id));
    assert!(provenance.contains(&outcome.child_chat_id));
    assert_eq!(grandchild.snapshot.chat_loop.id, source_receipt.loop_id);
    agent.shutdown().await;
    drop(agent);

    // 请求标识只在创建 Branch 的进程内有效；重启后 Branch 来源与名称由 Transcript 元数据恢复。
    let reopened = Agent::open_embedded(archive, config(), model).unwrap();
    reopened.resume_chat(source_chat_id).await.unwrap();
    let restored = reopened.resume_chat(outcome.child_chat_id).await.unwrap();
    let restored_origin = restored.branch_origin.as_ref().unwrap();
    assert_eq!(restored_origin.source_chat_id, source_chat_id);
    assert_eq!(restored_origin.name, outcome.name);
    assert!(restored_origin.creation.is_none());
    assert_eq!(reopened.list_chats().await.len(), 2);
    reopened.shutdown().await;
}

#[tokio::test]
async fn concurrent_branch_requests_are_serialized_by_request_identity() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("source answer")]));
    let agent = Agent::new_embedded(config(), model);
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source_receipt = agent
        .submit(source_chat_id, "source prompt".into())
        .await
        .unwrap();
    wait_for_run(&mut events, source_receipt.run_id).await;
    let source_before =
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap();
    let idempotent_request = BranchRequest {
        request_id: Uuid::new_v4(),
        source_chat_id,
        name: Some("same request".into()),
    };

    let (first_retry, second_retry) = tokio::join!(
        agent.branch_chat(idempotent_request.clone()),
        agent.branch_chat(idempotent_request)
    );
    let first_retry = first_retry.unwrap();
    let second_retry = second_retry.unwrap();
    assert_eq!(first_retry.child_chat_id, second_retry.child_chat_id);
    assert_eq!(agent.list_chats().await.len(), 2);
    assert_eq!(
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
        source_before
    );

    let first_request = BranchRequest {
        request_id: Uuid::new_v4(),
        source_chat_id,
        name: Some("parallel one".into()),
    };
    let second_request = BranchRequest {
        request_id: Uuid::new_v4(),
        source_chat_id,
        name: Some("parallel two".into()),
    };
    let (first_child, second_child) = tokio::join!(
        agent.branch_chat(first_request),
        agent.branch_chat(second_request)
    );
    let first_child = first_child.unwrap();
    let second_child = second_child.unwrap();
    assert_ne!(first_child.child_chat_id, second_child.child_chat_id);
    assert_ne!(first_child.child_chat_id, source_chat_id);
    assert_ne!(second_child.child_chat_id, source_chat_id);
    assert_eq!(agent.list_chats().await.len(), 4);
    assert_eq!(
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
        source_before
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn branch_model_seed_and_first_child_request_exclude_private_model_content() {
    let directory = tempfile::tempdir().unwrap();
    let private_thinking = "private reasoning from the source";
    let private_protocol_data = "private provider state from the source";
    let public_answer = "public source answer";
    let model = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::Thinking {
                thinking: private_thinking.into(),
                signature: Some("private-signature".into()),
            },
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value: serde_json::json!({"encrypted_content": private_protocol_data}),
            },
            Content::Text {
                text: public_answer.into(),
            },
        ],
        text("child answer"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source_receipt = agent
        .submit(source_chat_id, "source prompt".into())
        .await
        .unwrap();
    wait_for_run(&mut events, source_receipt.run_id).await;
    let source_before = agent.snapshot(source_chat_id).await.unwrap();
    assert!(
        source_before
            .conversation_messages()
            .unwrap()
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::Thinking { .. }))
    );
    assert!(
        source_before
            .conversation_messages()
            .unwrap()
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ProtocolData { .. }))
    );

    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: Some("public child".into()),
        })
        .await
        .unwrap();
    assert!(
        branch
            .snapshot
            .conversation_messages()
            .unwrap()
            .iter()
            .flat_map(|message| &message.content)
            .all(|content| !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ))
    );
    assert!(
        format!("{:?}", branch.snapshot.conversation_messages().unwrap()).contains(public_answer)
    );

    let child_receipt = agent
        .submit(branch.child_chat_id, "continue in child".into())
        .await
        .unwrap();
    wait_for_run(&mut events, child_receipt.run_id).await;
    {
        let requests = model.requests.lock().unwrap();
        let child_request = requests.last().unwrap();
        assert!(
            child_request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .all(|content| !matches!(
                    content,
                    Content::Thinking { .. } | Content::ProtocolData { .. }
                ))
        );
        let child_request_debug = format!("{:?}", child_request.messages);
        assert!(child_request_debug.contains(public_answer));
        assert!(!child_request_debug.contains(private_thinking));
        assert!(!child_request_debug.contains(private_protocol_data));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn branch_rewrites_duplicate_cross_run_call_ids_and_keeps_future_child_calls_unique() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("duplicate-call.txt");
    std::fs::write(&file, "duplicate call evidence").unwrap();
    let duplicate_call = || {
        call(
            "duplicate-call",
            "Read",
            serde_json::json!({"file_path": file, "offset": 0, "limit": 1024}),
        )
    };
    let model = Arc::new(ScriptedModel::new(vec![
        duplicate_call(),
        text("first source answer"),
        duplicate_call(),
        text("second source answer"),
        duplicate_call(),
        text("child answer"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(source_chat_id, "first source prompt".into())
        .await
        .unwrap();
    wait_for_run(&mut events, first.run_id).await;
    let second = agent
        .submit(source_chat_id, "second source prompt".into())
        .await
        .unwrap();
    wait_for_run(&mut events, second.run_id).await;
    assert_ne!(first.run_id, second.run_id);
    let source_before = agent.snapshot(source_chat_id).await.unwrap();
    let source_json = serde_json::to_value(&source_before).unwrap();

    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: Some("duplicate call branch".into()),
        })
        .await
        .unwrap();

    let origin = branch
        .snapshot
        .branch_origin
        .as_ref()
        .and_then(|origin| origin.creation.as_ref())
        .unwrap();
    let mut public_calls = Vec::new();
    let mut public_results = Vec::new();
    for item in &origin.public_seed {
        let BranchPublicItem::Message {
            source, message, ..
        } = item
        else {
            continue;
        };
        for content in &message.content {
            match content {
                Content::ToolUse { id, .. } => public_calls.push((source.run_id, id.clone())),
                Content::ToolResult { tool_use_id, .. } => {
                    public_results.push((source.run_id, tool_use_id.clone()));
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        public_calls,
        vec![
            (Some(first.run_id), "duplicate-call".into()),
            (Some(second.run_id), "duplicate-call".into()),
        ]
    );
    assert_eq!(public_results, public_calls);

    let inherited_messages = branch.snapshot.conversation_messages().unwrap();
    let inherited_ids = tool_use_ids(&inherited_messages);
    assert_eq!(
        inherited_ids,
        vec![
            "duplicate-call".into(),
            format!("tl_{}_1", second.run_id.simple()),
        ]
    );
    assert_eq!(tool_result_ids(&inherited_messages), inherited_ids);
    assert_eq!(
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
        source_json
    );

    let child = agent
        .submit(branch.child_chat_id, "continue in child".into())
        .await
        .unwrap();
    wait_for_run(&mut events, child.run_id).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(tool_use_ids(&requests[4].messages), inherited_ids);
        assert_eq!(tool_result_ids(&requests[4].messages), inherited_ids);
    }
    let child_messages = agent
        .snapshot(branch.child_chat_id)
        .await
        .unwrap()
        .conversation_messages()
        .unwrap();
    let child_ids = tool_use_ids(&child_messages);
    assert_eq!(
        child_ids,
        vec![
            "duplicate-call".into(),
            format!("tl_{}_1", second.run_id.simple()),
            format!("tl_{}_2", child.run_id.simple()),
        ]
    );
    assert_eq!(tool_result_ids(&child_messages), child_ids);
    assert_eq!(
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
        source_json
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn branch_materializes_summary_tool_and_user_shell_public_types() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("fact.txt");
    std::fs::write(&file, "fact").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read-fact",
            "Read",
            serde_json::json!({"file_path": file, "offset": 0, "limit": 1024}),
        ),
        text("first answer"),
        text("second answer"),
        text("summary of the selected suffix"),
        text("child answer after the inherited seed"),
        text("summary of the child suffix"),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    agent.install_own_tickhost();
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(source_chat_id, "inspect the fact".into())
        .await
        .unwrap();
    wait_for_run(&mut events, first.run_id).await;

    let mut shell_events = agent.subscribe_user_shells();
    let shell = agent
        .submit_user_shell(source_chat_id, "printf shell-fact".into(), false)
        .await
        .unwrap();
    loop {
        let record = tokio::time::timeout(std::time::Duration::from_secs(5), shell_events.recv())
            .await
            .unwrap()
            .unwrap();
        if record.id == shell.command_id
            && matches!(
                record.state,
                UserShellState::Completed | UserShellState::Failed
            )
        {
            assert_eq!(record.state, UserShellState::Completed);
            break;
        }
    }

    let second = agent
        .submit(source_chat_id, "second prompt".into())
        .await
        .unwrap();
    wait_for_run(&mut events, second.run_id).await;
    let before_summary = agent.snapshot(source_chat_id).await.unwrap();
    let checkpoint = before_summary.rewind_points()[1].clone();
    agent
        .summarize_checkpoint(
            source_chat_id,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    let source = agent.snapshot(source_chat_id).await.unwrap();
    let source_public = source.public_conversation_items().unwrap();
    assert!(source_public.iter().any(|item| matches!(
        item,
        BranchPublicItem::Message { message, .. }
            if message.content.iter().any(|content| matches!(content, Content::ToolUse { .. }))
    )));
    assert!(has_public_user_text(
        &source_public,
        "<bash-input>printf shell-fact</bash-input>"
    ));
    assert!(has_public_user_text(
        &source_public,
        "<bash-stdout>shell-fact</bash-stdout><bash-stderr></bash-stderr>"
    ));
    assert!(
        source_public
            .iter()
            .any(|item| matches!(item, BranchPublicItem::Summary { .. }))
    );

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(
        outcome.snapshot.public_conversation_items().unwrap(),
        source_public
    );
    assert_eq!(
        outcome.snapshot.conversation_messages().unwrap(),
        source.conversation_messages().unwrap()
    );
    assert!(outcome.snapshot.user_shells.is_empty());
    assert!(
        (0..outcome.snapshot.messages.len())
            .all(|index| !outcome.snapshot.is_compact_boundary(index))
    );
    assert!(outcome.snapshot.message_runs.is_empty());

    let child_run = agent
        .submit(outcome.child_chat_id, "continue after the branch".into())
        .await
        .unwrap();
    wait_for_run(&mut events, child_run.run_id).await;
    let before_child_summary = agent.snapshot(outcome.child_chat_id).await.unwrap();
    let child_checkpoint = before_child_summary.rewind_points().pop().unwrap();
    agent
        .summarize_checkpoint(
            outcome.child_chat_id,
            DirectedSummaryRequest {
                message_id: child_checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    let after_child_summary = agent.snapshot(outcome.child_chat_id).await.unwrap();
    let child_public = after_child_summary.public_conversation_items().unwrap();
    assert_eq!(
        &child_public[..source_public.len()],
        source_public.as_slice()
    );
    assert!(child_public.iter().any(|item| matches!(
        item,
        BranchPublicItem::Summary { summary, .. }
            if summary == "summary of the child suffix"
    )));
    assert!(after_child_summary.conversation_messages().is_ok());
    assert_eq!(
        serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
        serde_json::to_value(source).unwrap()
    );
    agent.shutdown().await;
}

enum ModelStep {
    PartialFailure(&'static str),
    Success(&'static str),
}

struct PartialModel {
    steps: Mutex<VecDeque<ModelStep>>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for PartialModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let step = self.steps.lock().unwrap().pop_front().unwrap();
        match step {
            ModelStep::PartialFailure(partial) => {
                let _ = events.send(ModelDelta::Text(partial.into()));
                tokio::task::yield_now().await;
                Err(ModelError::StreamInterrupted)
            }
            ModelStep::Success(answer) => Ok(ModelOutput {
                private_source: None,
                content: vec![Content::Text {
                    text: answer.into(),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            }),
        }
    }
}

#[tokio::test]
async fn failed_partial_reply_is_not_inherited_by_a_branch() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(PartialModel {
        steps: Mutex::new(
            vec![
                ModelStep::PartialFailure("partial private-looking reply"),
                ModelStep::Success("child answer"),
            ]
            .into(),
        ),
        requests: Mutex::new(Vec::new()),
    });
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let failed = agent
        .submit(source_chat_id, "produce a partial".into())
        .await
        .unwrap();
    wait_for_failed_run(&mut events, failed.run_id).await;
    let source = agent.snapshot(source_chat_id).await.unwrap();
    assert!(
        !format!("{:?}", source.conversation_messages().unwrap())
            .contains("partial private-looking reply")
    );

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: None,
        })
        .await
        .unwrap();
    assert!(
        !format!(
            "{:?}",
            outcome.snapshot.public_conversation_items().unwrap()
        )
        .contains("partial private-looking reply")
    );
    assert!(
        !format!("{:?}", outcome.snapshot.conversation_messages().unwrap())
            .contains("partial private-looking reply")
    );

    let continued = agent
        .submit(outcome.child_chat_id, "continue".into())
        .await
        .unwrap();
    wait_for_run(&mut events, continued.run_id).await;
    {
        let requests = model.requests.lock().unwrap();
        let child_request = requests.last().unwrap();
        assert!(!format!("{:?}", child_request.messages).contains("partial private-looking reply"));
    }
    agent.shutdown().await;
}

struct BlockingModel {
    entered: Notify,
}

#[async_trait]
impl ModelClient for BlockingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.entered.notify_one();
        cancel.cancelled().await;
        Err(ModelError::Cancelled)
    }
}

#[tokio::test]
async fn invalid_empty_and_busy_sources_have_no_branch_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let blocking = Arc::new(BlockingModel {
        entered: Notify::new(),
    });
    let agent = Agent::new_embedded(config(), blocking.clone());
    let empty = agent.create_chat(directory.path().into()).await.unwrap();
    let empty_count = agent.list_chats().await.len();
    assert!(matches!(
        agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id: empty,
                name: None,
            })
            .await,
        Err(AgentError::Invalid(message)) if message.contains("no conversation")
    ));
    assert_eq!(agent.list_chats().await.len(), empty_count);

    let mut events = agent.subscribe();
    let receipt = agent.submit(empty, "running prompt".into()).await.unwrap();
    blocking.entered.notified().await;
    let source_before = serde_json::to_value(agent.snapshot(empty).await.unwrap()).unwrap();
    assert!(matches!(
        agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id: empty,
                name: None,
            })
            .await,
        Err(AgentError::Busy)
    ));
    assert_eq!(agent.list_chats().await.len(), empty_count);
    assert_eq!(
        serde_json::to_value(agent.snapshot(empty).await.unwrap()).unwrap(),
        source_before
    );
    agent.interrupt(empty).await.unwrap();
    wait_for_failed_run(&mut events, receipt.run_id).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn automatic_names_are_unique_and_explicit_names_are_validated() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("answer")]));
    let agent = Agent::new_embedded(config(), model);
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(source_chat_id, "  first   prompt  ".into())
        .await
        .unwrap();
    wait_for_run(&mut events, receipt.run_id).await;
    let first = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: None,
        })
        .await
        .unwrap();
    let second = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(first.name, "first prompt (Branch)");
    assert_eq!(second.name, "first prompt (Branch 2)");
    assert!(matches!(
        agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id,
                name: Some("bad\nname".into()),
            })
            .await,
        Err(AgentError::Invalid(message)) if message.contains("control")
    ));
    agent.shutdown().await;
}

async fn wait_for_snapshot(
    agent: &Agent,
    chat_id: Uuid,
    is_ready: impl Fn(&ChatSnapshot) -> bool,
) -> ChatSnapshot {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = agent.snapshot(chat_id).await.unwrap();
            if is_ready(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Chat snapshot did not reach the expected state")
}

fn gated_shell_command(gate: &std::path::Path) -> String {
    format!(
        "printf before; while [ ! -e '{}' ]; do sleep 0.05; done; printf after",
        gate.display()
    )
}

fn has_public_user_text(items: &[BranchPublicItem], expected: &str) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            BranchPublicItem::Message { message, .. }
                if message.content.iter().any(|content| matches!(content, Content::Text { text } if text == expected))
        )
    })
}

fn has_task_notification(snapshot: &ChatSnapshot, task_id: Uuid) -> bool {
    snapshot
        .conversation_messages()
        .unwrap()
        .iter()
        .any(|message| {
            message.content.iter().any(|content| {
                matches!(content, Content::Text { text }
                if text.starts_with("<task-notification>")
                    && text.contains(&format!("<task-id>{task_id}</task-id>")))
            })
        })
}

#[tokio::test]
async fn background_user_shell_output_after_branch_stays_in_the_source_chat() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("state");
    let gate = directory.path().join("release-user-shell");
    let agent = Agent::open_embedded(
        archive.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::<Vec<Content>>::new())),
    )
    .unwrap();
    agent.install_own_tickhost();
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let shell = agent
        .submit_user_shell(source_chat_id, gated_shell_command(&gate), true)
        .await
        .unwrap();
    wait_for_snapshot(&agent, source_chat_id, |snapshot| {
        snapshot.user_shells.iter().any(|record| {
            record.id == shell.command_id && record.state == UserShellState::Background
        })
    })
    .await;

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: Some("background branch".into()),
        })
        .await
        .unwrap();
    assert!(outcome.snapshot.user_shells.is_empty());
    assert_eq!(outcome.snapshot.chat_loop.run_id, None);
    assert!(outcome.snapshot.operations.is_empty());
    let child_items_at_branch = outcome.snapshot.public_conversation_items().unwrap();
    let child_messages_at_branch = outcome.snapshot.conversation_messages().unwrap();
    assert!(has_public_user_text(
        &child_items_at_branch,
        &format!("<bash-input>{}</bash-input>", gated_shell_command(&gate))
    ));

    std::fs::write(&gate, b"release").unwrap();
    wait_for_snapshot(&agent, source_chat_id, |snapshot| {
        snapshot.user_shells.iter().any(|record| {
            record.id == shell.command_id && record.state == UserShellState::Completed
        })
    })
    .await;
    let source_after = wait_for_snapshot(&agent, source_chat_id, |snapshot| {
        has_task_notification(snapshot, shell.command_id)
    })
    .await;
    assert!(has_task_notification(&source_after, shell.command_id));

    let child_after = agent.snapshot(outcome.child_chat_id).await.unwrap();
    assert!(child_after.user_shells.is_empty());
    assert_eq!(
        child_after.public_conversation_items().unwrap(),
        child_items_at_branch
    );
    assert_eq!(
        child_after.conversation_messages().unwrap(),
        child_messages_at_branch
    );
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(
        archive,
        config(),
        Arc::new(ScriptedModel::new(Vec::<Vec<Content>>::new())),
    )
    .unwrap();
    reopened.install_own_tickhost();
    let reopened_source = reopened.resume_chat(source_chat_id).await.unwrap();
    assert!(has_task_notification(&reopened_source, shell.command_id));
    let reopened_child = reopened.resume_chat(outcome.child_chat_id).await.unwrap();
    // 照 Claude Code：恢复时末尾是 user 消息，追加不请求回复的 assistant 哨兵。
    let sentinel = Message {
        role: tickloop_agent_protocol::Role::Assistant,
        content: vec![Content::Text {
            text: "No response requested.".into(),
        }],
    };
    // 复制种子的公开来源只在创建 Branch 的进程内有效；重启后公开对话按子 Chat 自身的消息投影。
    let without_source = |items: &[BranchPublicItem]| {
        items
            .iter()
            .map(|item| match item {
                BranchPublicItem::Message {
                    message, contexts, ..
                } => (Some((message.clone(), contexts.clone())), None),
                BranchPublicItem::Summary { summary, .. } => (None, Some(summary.clone())),
            })
            .collect::<Vec<_>>()
    };
    let reopened_items = reopened_child.public_conversation_items().unwrap();
    assert_eq!(
        without_source(&reopened_items[..child_items_at_branch.len()]),
        without_source(&child_items_at_branch)
    );
    assert!(matches!(
        &reopened_items[child_items_at_branch.len()..],
        [BranchPublicItem::Message { message, .. }] if *message == sentinel
    ));
    let mut expected_messages = child_messages_at_branch.clone();
    expected_messages.push(sentinel);
    assert_eq!(
        reopened_child.conversation_messages().unwrap(),
        expected_messages
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn background_agent_shell_output_after_branch_stays_in_the_source_chat() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release-agent-shell");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "background-shell",
            "Bash",
            serde_json::json!({"command": gated_shell_command(&gate), "run_in_background": true}),
        ),
        text("The source background task is running."),
    ]));
    let agent = Agent::open_embedded(directory.path().join("state"), config(), model).unwrap();
    agent.install_own_tickhost();
    let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let source = agent
        .submit(source_chat_id, "start the background task".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != source.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(source_chat_id, request.id, true)
                .await
                .unwrap(),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("source run failed: {error}"),
            _ => {}
        }
    }
    let source_command = wait_for_snapshot(&agent, source_chat_id, |snapshot| {
        snapshot.commands.first().is_some_and(|command| {
            command.state == CommandState::Background && command.output_bytes >= 6
        })
    })
    .await
    .commands[0]
        .clone();

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id,
            name: Some("agent shell branch".into()),
        })
        .await
        .unwrap();
    assert!(outcome.snapshot.commands.is_empty());
    assert!(outcome.snapshot.operations.is_empty());
    assert_eq!(outcome.snapshot.chat_loop.id, source.loop_id);
    let child_items_at_branch = outcome.snapshot.public_conversation_items().unwrap();
    let child_messages_at_branch = outcome.snapshot.conversation_messages().unwrap();

    std::fs::write(&gate, b"release").unwrap();
    let source_after = wait_for_snapshot(&agent, source_chat_id, |snapshot| {
        snapshot.commands.iter().any(|command| {
            command.operation_id == source_command.operation_id
                && command.state == CommandState::Completed
        })
    })
    .await;
    assert_eq!(source_after.commands[0].loop_id, source.loop_id);

    let child_after = agent.snapshot(outcome.child_chat_id).await.unwrap();
    assert!(child_after.commands.is_empty());
    assert_eq!(
        child_after.public_conversation_items().unwrap(),
        child_items_at_branch
    );
    assert_eq!(
        child_after.conversation_messages().unwrap(),
        child_messages_at_branch
    );
    agent.shutdown().await;
}
