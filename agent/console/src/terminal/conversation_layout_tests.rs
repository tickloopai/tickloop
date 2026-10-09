//! The conversation laid out as Claude Code lays it out: one blank row above every block, each
//! result under its own call, reads and searches collapsed, and the same rows whether a View
//! follows a run's events or reopens its saved history.
use super::*;
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    Content, Decision, ExecutionRef, LocalMessage, ModelIdentity, ModelProvider, PermissionRule,
    PermissionScope, Protocol, Usage,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MARKDOWN_REPLY: &str =
    "I checked the build setup.\n\n- First item\n- Second item\n\n```sh\ncargo build\n```";

/// One scripted reply: thinking streamed before the content, which streams its text.
struct Reply {
    thinking: Option<&'static str>,
    content: Vec<Content>,
}

impl Reply {
    fn of(content: Vec<Vec<Content>>) -> Self {
        Self {
            thinking: None,
            content: content.concat(),
        }
    }
}

/// Answers each request with the next scripted reply, streaming it as a provider does.
struct StreamedScript {
    replies: Mutex<VecDeque<Reply>>,
}

#[async_trait]
impl ModelClient for StreamedScript {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        let mut content = Vec::new();
        if let Some(thinking) = reply.thinking {
            events.send(ModelDelta::Thinking(thinking.into())).unwrap();
            content.push(Content::Thinking {
                thinking: thinking.into(),
                signature: None,
            });
        }
        for block in &reply.content {
            if let Content::Text { text } = block {
                events.send(ModelDelta::Text(text.clone())).unwrap();
            }
        }
        content.extend(reply.content);
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                ..Usage::default()
            },
            stop_reason: "end_turn".into(),
        })
    }
}

fn rule(decision: Decision, tool: &str) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

/// Waits for `chat`'s run to end, declining every permission it asks for.
async fn finish_run(
    agent: &Agent,
    chat: Uuid,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) {
    loop {
        match tickloop_agent_test_support::next_event(events).await.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission_with_comment(
                    chat,
                    request.id,
                    false,
                    PermissionScope::Once,
                    None,
                )
                .await
                .unwrap(),
            EventKind::Completed { .. } | EventKind::Interrupted => return,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// The conversation of `snapshot` as a View following its events draws it.
fn followed(chat: Uuid, workdir: &std::path::Path, snapshot: &ChatSnapshot) -> String {
    let console = Console::test_interactive_for_chat(chat);
    console.set_input_context(Some(chat), workdir, &[]).unwrap();
    let mut tools = ToolPresentation::new(false);
    render_restored_observation(
        &console,
        &mut tools,
        chat,
        0,
        &mut HashMap::new(),
        &mut MessageGroups::default(),
        ExecutionObservation {
            execution: ExecutionRef {
                daemon_epoch: Uuid::nil(),
                agent_id: chat,
                generation: 1,
            },
            events: snapshot.conversation_events(),
            snapshot: snapshot.clone(),
            reset_required: false,
        },
    )
    .unwrap();
    console.test_conversation_text()
}

/// The conversation of `snapshot` as a View reopening its history draws it.
fn reopened(chat: Uuid, snapshot: &ChatSnapshot) -> String {
    let console = Console::test_interactive_for_chat(chat);
    workflows::write_public_history(&console, snapshot).unwrap();
    console.test_conversation_text()
}

#[tokio::test]
async fn followed_and_reopened_conversations_share_one_claude_code_layout() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    std::fs::write(
        workdir.join("notes.txt"),
        "Build notes\nTODO: fix cache\nKeep the ui stage fast\n",
    )
    .unwrap();
    let path = |name: &str| workdir.join(name).display().to_string();
    let call = tickloop_agent_test_support::call;
    let text = tickloop_agent_test_support::text;
    let model = StreamedScript {
        replies: Mutex::new(
            [
                Reply {
                    thinking: Some("Look at the build first."),
                    content: [
                        text(MARKDOWN_REPLY),
                        call(
                            "bash",
                            "Bash",
                            serde_json::json!({
                                "command": "printf 'one\\ntwo\\n'",
                                "description": "Print two lines"
                            }),
                        ),
                    ]
                    .concat(),
                },
                Reply::of(vec![
                    call(
                        "read",
                        "Read",
                        serde_json::json!({"file_path": path("notes.txt")}),
                    ),
                    call(
                        "grep",
                        "Grep",
                        serde_json::json!({"pattern": "TODO", "path": path("")}),
                    ),
                ]),
                Reply::of(vec![call(
                    "missing",
                    "Read",
                    serde_json::json!({"file_path": path("missing.txt")}),
                )]),
                Reply::of(vec![
                    text("Cleaning up."),
                    call(
                        "write",
                        "Write",
                        serde_json::json!({"file_path": path("build.log"), "content": "stale\n"}),
                    ),
                ]),
                Reply::of(vec![call(
                    "edit",
                    "Edit",
                    serde_json::json!({
                        "file_path": path("notes.txt"),
                        "old_string": "TODO: fix cache",
                        "new_string": "DONE: fix cache"
                    }),
                )]),
                Reply::of(vec![text("All set.")]),
            ]
            .into(),
        ),
    };
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), Arc::new(model));
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![rule(Decision::Ask, "Write"), rule(Decision::Allow, "Bash")],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Audit the build".into())
        .await
        .unwrap();
    finish_run(&agent, chat, &mut events).await;
    agent
        .submit(chat, None, "Continue with the edit".into())
        .await
        .unwrap();
    finish_run(&agent, chat, &mut events).await;
    let snapshot = agent.snapshot(chat).await.unwrap();

    let followed = followed(chat, &workdir, &snapshot);
    assert_eq!(
        followed,
        reopened(chat, &snapshot),
        "a followed run and its reopened history draw the same rows"
    );
    assert_eq!(
        followed,
        [
            "",
            "❯ Audit the build",
            "",
            "∴ Thinking (ctrl+o to expand)",
            "",
            "⏺ I checked the build setup.",
            "",
            "  - First item",
            "  - Second item",
            "",
            "  cargo build",
            "",
            "⏺ Bash · Print two lines",
            "  ⎿  one",
            "     two",
            "",
            "  Searched for 1 pattern, read 2 files (ctrl+o to expand)",
            "",
            "⏺ Cleaning up.",
            "",
            "⏺ Write · build.log",
            "  ⎿  User rejected write to build.log",
            "",
            "❯ Continue with the edit",
            "",
            "⏺ Edit · notes.txt",
            "  ⎿  Added 1 line, removed 1 line",
            "      1  Build notes",
            "      2 -TODO: fix cache",
            "      2 +DONE: fix cache",
            "      3  Keep the ui stage fast",
            "",
            "⏺ All set.",
            "",
            "✻ Done · ↓ 40 tokens",
            "",
        ]
        .join("\n")
    );
    let transcript = presentation::transcript_document(
        &snapshot,
        DisplayProfile::default(),
        &Default::default(),
    )
    .plain_text(120);
    for expanded in [
        "⏺ Read(",
        "⏺ Grep(",
        "Error: file or directory does not exist",
    ] {
        assert!(
            transcript.contains(expanded),
            "Ctrl+O shows each collapsed call: {transcript}"
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn a_finished_user_shell_shows_the_result_the_agent_records() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shells = agent.subscribe_user_shells();
    for (command, state) in [
        ("printf 'one\\ntwo\\n'", UserShellState::Completed),
        ("printf 'boom\\n'; exit 3", UserShellState::Failed),
    ] {
        let receipt = agent
            .submit_user_shell_for_goal(chat, None, command.into(), false)
            .await
            .unwrap();
        let record = loop {
            let record = tokio::time::timeout(Duration::from_secs(10), shells.recv())
                .await
                .unwrap()
                .unwrap();
            if record.id == receipt.command_id && user_shell_is_terminal(&record) {
                break record;
            }
        };
        assert_eq!(record.state, state);
        let output = agent
            .read_user_shell_output(chat, record.id, 0, USER_SHELL_RESULT_BYTES)
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        let recorded = snapshot
            .messages
            .iter()
            .rev()
            .find_map(|message| {
                message.content.iter().find_map(|content| match content {
                    Content::Text { text } => match LocalMessage::parse(text) {
                        Some(output @ LocalMessage::BashOutput { .. }) => Some(output),
                        _ => None,
                    },
                    _ => None,
                })
            })
            .unwrap();
        assert_eq!(
            presentation::user_shell_result(&record, output["output"].as_str().unwrap()),
            recorded,
            "{command}"
        );
    }
    agent.shutdown().await;
}

fn notification(status: TaskNotificationStatus, summary: &str) -> TaskNotification {
    TaskNotification {
        chat_id: Uuid::new_v4(),
        task_id: Uuid::new_v4(),
        tool_use_id: None,
        output_file: "/tmp/task.log".into(),
        status,
        summary: summary.into(),
    }
}

#[test]
fn notices_warnings_and_compaction_are_blocks_of_their_own() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console.write("Earlier output\n").unwrap();
    write_warning(&console, "Settings could not be read").unwrap();
    write_compaction_notice(&console, chat, "summary").unwrap();
    console
        .write_roles(&task_notification_line(
            &notification(
                TaskNotificationStatus::Failed,
                "Background command \"make\" failed",
            ),
            DisplayProfile::default(),
        ))
        .unwrap();
    assert_eq!(
        console.test_conversation_text(),
        [
            "Earlier output",
            "",
            "! Settings could not be read",
            "",
            "✻ Conversation compacted (ctrl+o for history)",
            "",
            "⏺ Background command \"make\" failed",
            "",
        ]
        .join("\n")
    );
    let completed = task_notification_line(
        &notification(TaskNotificationStatus::Completed, "done"),
        DisplayProfile::default(),
    );
    assert_eq!(
        completed[1],
        ("⏺".into(), Some(Role::Success)),
        "the task's dot takes its outcome's colour"
    );
}

#[test]
fn a_write_opening_with_a_newline_is_a_block_and_newlines_alone_end_a_line() {
    let console = Console::test_interactive_for_chat(Uuid::new_v4());
    console.write("partial").unwrap();
    console.write("\n").unwrap();
    console.write("next line\n").unwrap();
    console.write("\n\n\nA notice\n").unwrap();
    console
        .write_roles(&[("\n".into(), None), ("Another notice\n".into(), None)])
        .unwrap();
    assert_eq!(
        console.test_conversation_text(),
        "partial\nnext line\n\nA notice\n\nAnother notice\n"
    );
}
