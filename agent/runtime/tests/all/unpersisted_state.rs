use crate::support;

use std::sync::Arc;

use serde_json::json;
use tickloop_agent_protocol::{ChatId, EventKind, ExecutionMode, Receipt};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, single_question, text};
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    chat: ChatId,
    matches: impl Fn(&EventKind) -> bool,
) {
    loop {
        let event = next_event(events).await;
        if event.chat_id != chat {
            continue;
        }
        if let EventKind::Failed(error) = &event.kind {
            panic!("run failed: {error}");
        }
        if matches(&event.kind) {
            return;
        }
    }
}

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    wait_for(&mut events, chat, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    receipt
}

/// 照 Claude Code，重启恢复只还原对话：权限模式回到默认，挂起的权限请求、提问、排队输入与命令、
/// 会话级规则、附加目录、操作记录、用量账本和事件都不再出现。
#[tokio::test]
async fn restart_keeps_the_conversation_but_forgets_session_only_state() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    let extra = directory.path().join("extra");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("The first answer."),
            call(
                "run-shell",
                "Bash",
                json!({"command":"env sh -c 'printf hi'"}),
            ),
            call(
                "clarify",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which report?"),
            ),
        ])),
    )
    .unwrap();
    let chat = first.create_chat(workdir.clone()).await.unwrap();
    complete_turn(&first, chat, "Summarize the notes").await;

    first
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    first
        .set_directories(chat, workdir.clone(), vec![extra.clone()])
        .await
        .unwrap();
    let mut session_rules = first.snapshot(chat).await.unwrap().policy.rules;
    session_rules.push(tickloop_agent_protocol::PermissionRule {
        id: uuid::Uuid::new_v4(),
        decision: tickloop_agent_protocol::Decision::Allow,
        tool: "Read".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    });
    first
        .set_permission_rules(chat, session_rules)
        .await
        .unwrap();

    let mut events = first.subscribe();
    let waiting = first
        .submit(chat, None, "Run a shell command".into())
        .await
        .unwrap();
    wait_for(&mut events, chat, |kind| {
        matches!(kind, EventKind::PermissionRequested { .. })
    })
    .await;
    first
        .submit(chat, Some(waiting.loop_id), "Then add a conclusion".into())
        .await
        .unwrap();
    first
        .queue_command(chat, None, "/status".into())
        .await
        .unwrap();

    let asking = first.create_chat(workdir.clone()).await.unwrap();
    first
        .submit(asking, None, "Write the report".into())
        .await
        .unwrap();
    wait_for(&mut events, asking, |kind| {
        matches!(kind, EventKind::QuestionAsked { .. })
    })
    .await;

    let before = first.snapshot(chat).await.unwrap();
    assert_eq!(before.permissions.len(), 1);
    assert!(
        before.queued_inputs().len() >= 2,
        "{:?}",
        before.queued_inputs()
    );
    assert!(!before.operations.is_empty());
    assert!(before.request_usage.summary().request_count > 0);
    assert!(!before.events.is_empty());
    assert_eq!(first.snapshot(asking).await.unwrap().questions.len(), 1);
    first.flush_transcripts().await;

    let second = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let restored = second.resume_chat(chat).await.unwrap();
    assert_eq!(restored.policy.mode, ExecutionMode::AcceptEdits);
    assert!(restored.policy.rules.is_empty(), "{:?}", restored.policy);
    assert!(restored.permissions.is_empty());
    assert!(restored.queued_inputs().is_empty());
    assert!(restored.pending_commands.is_empty());
    assert!(restored.additional_directories.is_empty());
    assert!(restored.operations.is_empty());
    assert_eq!(restored.request_usage.summary().request_count, 0);
    assert!(restored.events.is_empty());
    let texts = restored
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            tickloop_agent_protocol::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for expected in [
        "Summarize the notes",
        "The first answer.",
        "Run a shell command",
    ] {
        assert!(texts.contains(&expected), "{texts:?}");
    }

    let restored_question = second.resume_chat(asking).await.unwrap();
    assert!(restored_question.questions.is_empty());
    assert!(
        restored_question
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == "Write the report"))
    );
    first.shutdown().await;
}

/// 用量只统计本进程：恢复后的 Chat 从空账本开始，新一轮对话只记本进程的请求。
#[tokio::test]
async fn usage_after_restart_counts_only_requests_made_by_this_process() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("one"), text("two")])),
    )
    .unwrap();
    let chat = first.create_chat(workdir).await.unwrap();
    complete_turn(&first, chat, "First").await;
    complete_turn(&first, chat, "Second").await;
    assert_eq!(
        support::model_script_requests(&first.snapshot(chat).await.unwrap()).len(),
        2
    );
    first.shutdown().await;

    let second = Agent::open_embedded(
        root,
        config(),
        Arc::new(ScriptedModel::new(vec![text("three")])),
    )
    .unwrap();
    second.resume_chat(chat).await.unwrap();
    complete_turn(&second, chat, "Third").await;
    let summary = second.snapshot(chat).await.unwrap().request_usage.summary();
    assert_eq!(summary.request_count, 1);
    second.shutdown().await;
}
