mod support;

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use support::{
    ScriptedModel, call, config, is_tool_result_line, next_event, submit_plan, text,
    transcript_file, truncate_transcript_before,
};
use tickloop_agent::{
    model::{
        Content, Message, ModelClient, ModelDelta, ModelError, ModelIdentity, ModelOutput,
        ModelRequest, Protocol, Role, ToolResultBlock, ToolResultContent, Usage,
    },
    permissions::{Decision, ExecutionMode, PermissionRule},
    runtime::{
        Agent, AgentHostOptions, BranchRequest, ChatId, EventKind, LoopId, LoopState, PromptPolicy,
        Receipt, RewindMode, Trigger,
    },
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CONTINUE_FROM_INTERRUPTION: &str = "Continue from where you left off.";
const NO_RESPONSE_REQUESTED: &str = "No response requested.";

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

/// 按顺序回复的模型；`None` 表示这次请求一直等待，直到运行被取消。
struct SequenceModel {
    requests: Mutex<Vec<ModelRequest>>,
    replies: Mutex<VecDeque<Option<Vec<Content>>>>,
}

impl SequenceModel {
    fn new(replies: Vec<Option<Vec<Content>>>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        })
    }
}

#[async_trait]
impl ModelClient for SequenceModel {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent::model::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        let Some(content) = reply else {
            cancel.cancelled().await;
            return Err(ModelError::Cancelled);
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

async fn complete_turn(agent: &Agent, chat: ChatId, goal: Option<LoopId>, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, goal, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

fn request_text(request: &ModelRequest) -> String {
    serde_json::to_string(&request.messages).unwrap()
}

fn conversation(request: &ModelRequest) -> Vec<Message> {
    request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .cloned()
        .collect()
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![Content::Text { text: text.into() }],
    }
}

#[tokio::test]
async fn two_agents_on_one_root_finish_turns_concurrently() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer from first host")])),
    )
    .unwrap();
    let second = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer from second host")])),
    )
    .unwrap();
    let first_chat = first.create_chat(directory.path().into()).await.unwrap();
    let second_chat = second.create_chat(directory.path().into()).await.unwrap();
    tokio::join!(
        complete_turn(&first, first_chat, None, "prompt for first host"),
        complete_turn(&second, second_chat, None, "prompt for second host"),
    );
    first.shutdown().await;
    second.shutdown().await;
    assert!(first.transcript_write_error().is_none());
    assert!(second.transcript_write_error().is_none());

    let model = Arc::new(ScriptedModel::new(vec![
        text("first again"),
        text("second again"),
    ]));
    let third = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let saved = third.saved_chats(directory.path()).await.unwrap();
    let mut saved_ids = saved.iter().map(|chat| chat.id).collect::<Vec<_>>();
    saved_ids.sort();
    let mut expected_ids = vec![first_chat, second_chat];
    expected_ids.sort();
    assert_eq!(saved_ids, expected_ids);
    for (chat, own, other) in [
        (
            first_chat,
            "prompt for first host",
            "prompt for second host",
        ),
        (
            second_chat,
            "prompt for second host",
            "prompt for first host",
        ),
    ] {
        third.resume_chat(chat).await.unwrap();
        complete_turn(&third, chat, None, "continue").await;
        let requests = model.requests.lock().unwrap();
        let sent = request_text(requests.last().unwrap());
        assert!(sent.contains(own), "{sent}");
        assert!(!sent.contains(other), "{sent}");
    }
    third.shutdown().await;
}

#[tokio::test]
async fn two_resumes_of_one_chat_fork_and_the_latest_leaf_wins() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let origin = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("shared answer")])),
    )
    .unwrap();
    let chat = origin.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&origin, chat, None, "shared prompt").await;
    origin.shutdown().await;
    drop(origin);

    let left = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("left answer")])),
    )
    .unwrap();
    let right = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("right answer")])),
    )
    .unwrap();
    left.resume_chat(chat).await.unwrap();
    right.resume_chat(chat).await.unwrap();
    complete_turn(&left, chat, None, "from the left host").await;
    left.flush_transcripts().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    complete_turn(&right, chat, None, "from the right host").await;
    left.shutdown().await;
    right.shutdown().await;

    let model = Arc::new(ScriptedModel::new(vec![text("after the fork")]));
    let latest = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    latest.resume_chat(chat).await.unwrap();
    complete_turn(&latest, chat, None, "which branch").await;
    let sent = request_text(&model.requests.lock().unwrap()[0]);
    assert!(sent.contains("shared prompt"), "{sent}");
    assert!(sent.contains("from the right host"), "{sent}");
    assert!(!sent.contains("from the left host"), "{sent}");
    latest.shutdown().await;
    let file = std::fs::read_to_string(transcript_file(&root, chat)).unwrap();
    assert!(file.contains("from the left host"));
}

#[tokio::test]
async fn corrupt_and_truncated_lines_are_skipped_on_resume() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("first answer"),
            text("second answer"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, chat, None, "first prompt").await;
    complete_turn(&agent, chat, Some(first.loop_id), "second prompt").await;
    agent.shutdown().await;
    drop(agent);

    let path = transcript_file(&root, chat);
    let text_before = std::fs::read_to_string(&path).unwrap();
    let mut lines = text_before.lines().map(str::to_owned).collect::<Vec<_>>();
    let metadata_line = lines
        .iter()
        .position(|line| line.starts_with("{\"type\":\"last-prompt\""))
        .unwrap();
    lines[metadata_line] = "{broken".into();
    let last_message = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("{\"type\":\"assistant\""))
        .unwrap()
        .clone();
    let mut damaged = lines.join("\n");
    damaged.push('\n');
    damaged.push_str(&last_message[..last_message.len() / 2]);
    std::fs::write(&path, damaged).unwrap();

    let model = Arc::new(ScriptedModel::new(vec![text("third answer")]));
    let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
    restored.resume_chat(chat).await.unwrap();
    complete_turn(&restored, chat, Some(first.loop_id), "third prompt").await;
    assert_eq!(
        conversation(&model.requests.lock().unwrap()[0]),
        vec![
            user("first prompt"),
            assistant("first answer"),
            user("second prompt"),
            assistant("second answer"),
            user("third prompt"),
        ]
    );
    restored.shutdown().await;
}

#[tokio::test]
#[ignore = "subprocess fixture invoked by killed_process_resumes_from_its_flushed_transcript"]
async fn killed_process_fixture() {
    let Some(directory) = std::env::var_os("TICKLOOP_TRANSCRIPT_KILL_DIRECTORY") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        call(
            "fatal",
            "Bash",
            json!({"command": format!("sleep 0.3; kill -KILL {}", std::process::id())}),
        ),
    ]));
    let agent = Agent::open_with_options(
        directory.join("root"),
        config(),
        model,
        AgentHostOptions {
            prompt_policy: PromptPolicy::Unrestricted,
            ..AgentHostOptions::embedded_defaults()
        },
    )
    .unwrap();
    let chat = agent.create_chat(directory.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    std::fs::write(directory.join("chat"), chat.to_string()).unwrap();
    let first = complete_turn(&agent, chat, None, "first prompt").await;
    agent
        .submit(chat, Some(first.loop_id), "prompt before the kill".into())
        .await
        .unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn killed_process_resumes_from_its_flushed_transcript() {
    use std::os::unix::process::ExitStatusExt;

    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "killed_process_fixture",
            "--nocapture",
        ])
        .env("TICKLOOP_TRANSCRIPT_KILL_DIRECTORY", directory.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let chat: ChatId = std::fs::read_to_string(directory.path().join("chat"))
        .unwrap()
        .parse()
        .unwrap();

    let model = Arc::new(ScriptedModel::new(vec![text("resumed answer")]));
    let restored =
        Agent::open_embedded(directory.path().join("root"), config(), model.clone()).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(
        snapshot
            .loops
            .iter()
            .all(|goal| goal.state != LoopState::Running)
    );
    complete_turn(&restored, chat, None, "after the kill").await;
    assert_eq!(
        conversation(&model.requests.lock().unwrap()[0]),
        vec![
            user("first prompt"),
            assistant("first answer"),
            user("prompt before the kill"),
            assistant(NO_RESPONSE_REQUESTED),
            user("after the kill"),
        ]
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn resume_post_processing_repairs_tool_pairs_and_preserves_private_history() {
    // 末尾是工具结果：先追加"从中断处继续"的 meta 消息，再追加哨兵。
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        std::fs::write(directory.path().join("notes.txt"), "note body").unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", "Read", json!({"file_path":"notes.txt"})),
                text("final answer that was lost"),
            ])),
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&agent, chat, None, "read the notes").await;
        agent.shutdown().await;
        drop(agent);
        truncate_transcript_before(&transcript_file(&root, chat), |line| {
            line.contains("final answer that was lost")
        });

        let model = Arc::new(ScriptedModel::new(vec![text("continued")]));
        let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
        restored.resume_chat(chat).await.unwrap();
        complete_turn(&restored, chat, None, "next prompt").await;
        let sent = conversation(&model.requests.lock().unwrap()[0]);
        assert!(matches!(
            &sent[1].content[..],
            [Content::ToolUse { name, .. }] if name == "Read"
        ));
        assert!(matches!(
            &sent[2].content[..],
            [Content::ToolResult { content, .. }] if content.text().contains("note body")
        ));
        assert_eq!(
            sent[3..],
            [
                user(CONTINUE_FROM_INTERRUPTION),
                assistant(NO_RESPONSE_REQUESTED),
                user("next prompt"),
            ]
        );
        restored.shutdown().await;
    }
    // 工具结果没有写入：所有 tool_use 都没有结果的 assistant 被过滤，提示后接哨兵。
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        std::fs::write(directory.path().join("notes.txt"), "note body").unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", "Read", json!({"file_path":"notes.txt"})),
                text("never recorded"),
            ])),
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&agent, chat, None, "read the notes").await;
        agent.shutdown().await;
        drop(agent);
        truncate_transcript_before(&transcript_file(&root, chat), |line| {
            is_tool_result_line(line, "read")
        });

        let model = Arc::new(ScriptedModel::new(vec![text("continued")]));
        let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
        restored.resume_chat(chat).await.unwrap();
        complete_turn(&restored, chat, None, "next prompt").await;
        assert_eq!(
            conversation(&model.requests.lock().unwrap()[0]),
            vec![
                user("read the notes"),
                assistant(NO_RESPONSE_REQUESTED),
                user("next prompt"),
            ]
        );
        restored.shutdown().await;
    }
    // Private-only history survives restore for compatible-source replay.
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let agent = Agent::open_embedded(
            root.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![
                vec![Content::Thinking {
                    thinking: "orphaned private thought".into(),
                    signature: None,
                }],
                text("visible answer"),
            ])),
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let first = complete_turn(&agent, chat, None, "think only").await;
        complete_turn(&agent, chat, Some(first.loop_id), "answer now").await;
        agent.shutdown().await;
        drop(agent);

        let model = Arc::new(ScriptedModel::new(vec![text("continued")]));
        let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
        restored.resume_chat(chat).await.unwrap();
        complete_turn(&restored, chat, Some(first.loop_id), "next prompt").await;
        assert_eq!(
            conversation(&model.requests.lock().unwrap()[0]),
            vec![
                user("think only"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Thinking {
                        thinking: "orphaned private thought".into(),
                        signature: None,
                    }]
                },
                user("answer now"),
                assistant("visible answer"),
                user("next prompt"),
            ]
        );
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn restored_loops_are_interrupted_and_triggers_disarmed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = SequenceModel::new(vec![Some(text("scheduled")), None]);
    let original = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let scheduled = complete_turn(&original, chat, None, "schedule a check").await;
    original
        .configure_trigger(
            chat,
            scheduled.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "periodic check".into(),
                interval_seconds: Some(3_600),
                expires_at_ms: None,
                accepts_events: false,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let running = original
        .submit(chat, None, "long running goal".into())
        .await
        .unwrap();
    let live = original.snapshot(chat).await.unwrap();
    let state_of = |loops: &[tickloop_agent::runtime::LoopSnapshot], id: LoopId| {
        loops.iter().find(|goal| goal.id == id).unwrap().clone()
    };
    assert_eq!(
        state_of(&live.loops, running.loop_id).state,
        LoopState::Running
    );
    assert_eq!(
        state_of(&live.loops, scheduled.loop_id).state,
        LoopState::WaitingTrigger
    );
    original.flush_transcripts().await;

    // 原进程仍在运行时，另一个实例恢复同一 Chat。
    let resumed_model = Arc::new(ScriptedModel::new(vec![text("schedule resumed")]));
    let other = Agent::open_embedded(root, config(), resumed_model.clone()).unwrap();
    let restored = other.resume_chat(chat).await.unwrap();
    assert_eq!(
        state_of(&restored.loops, running.loop_id).state,
        LoopState::Interrupted
    );
    let restored_schedule = state_of(&restored.loops, scheduled.loop_id);
    assert_eq!(restored_schedule.state, LoopState::Interrupted);
    assert!(!restored_schedule.trigger.unwrap().armed);
    assert!(restored.resume_triggers.contains(&scheduled.loop_id));
    assert!(resumed_model.requests.lock().unwrap().is_empty());

    complete_turn(&other, chat, Some(scheduled.loop_id), "resume the schedule").await;
    let rearmed = other.snapshot(chat).await.unwrap();
    let schedule = state_of(&rearmed.loops, scheduled.loop_id);
    assert!(schedule.trigger.unwrap().armed);
    assert_eq!(schedule.state, LoopState::WaitingTrigger);
    other.shutdown().await;
    original.shutdown().await;
}

#[tokio::test]
async fn branch_copies_only_the_visible_conversation_and_records_its_source() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("kept answer"),
            text("answer to ABANDONED-MARKER"),
        ])),
    )
    .unwrap();
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, source, None, "kept prompt").await;
    complete_turn(
        &agent,
        source,
        Some(first.loop_id),
        "ABANDONED-MARKER prompt",
    )
    .await;
    let checkpoint = agent.snapshot(source).await.unwrap().rewind_points()[1].clone();
    agent
        .rewind(source, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: None,
            name: Some("clean branch".into()),
        })
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let model = Arc::new(ScriptedModel::new(vec![text("branch answer")]));
    let reopened = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let saved = reopened.saved_chats(directory.path()).await.unwrap();
    let child = saved
        .iter()
        .find(|chat| chat.id == branch.child_chat_id)
        .expect("the branch must be listed");
    assert_eq!(child.branch_source, Some(source));
    assert_eq!(child.title.as_deref(), Some("clean branch (Branch)"));
    reopened.resume_chat(branch.child_chat_id).await.unwrap();
    complete_turn(&reopened, branch.child_chat_id, None, "continue the branch").await;
    let sent = request_text(&model.requests.lock().unwrap()[0]);
    assert!(sent.contains("kept prompt"), "{sent}");
    assert!(!sent.contains("ABANDONED-MARKER"), "{sent}");
    reopened.shutdown().await;
    let child_file = std::fs::read_to_string(transcript_file(&root, branch.child_chat_id)).unwrap();
    assert!(!child_file.contains("ABANDONED-MARKER"));
}

fn tool_results(request: &ModelRequest) -> Vec<(String, ToolResultContent)> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

/// 读出或改写 Transcript 中某个工具结果条目的 `content` 原始 JSON。
fn edit_stored_tool_result(path: &Path, call_id: &str, edit: impl Fn(&mut Value)) -> Value {
    let mut stored = None;
    let lines = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            if !is_tool_result_line(line, call_id) {
                return line.to_owned();
            }
            let mut entry: Value = serde_json::from_str(line).unwrap();
            let result = entry["message"]["content"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|block| block["tool_use_id"] == call_id)
                .unwrap();
            edit(&mut result["content"]);
            stored = Some(result["content"].clone());
            entry.to_string()
        })
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::write(path, lines).unwrap();
    stored.unwrap_or_else(|| panic!("Transcript has no result for {call_id}"))
}

/// 从已加载的来源 Chat 分支，关闭后以 `model` 重新打开并恢复子 Chat，使其只能从 Transcript 读回。
async fn branch_and_reopen(
    agent: Agent,
    root: &Path,
    source: ChatId,
    model: Arc<ScriptedModel>,
) -> (Agent, ChatId) {
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    let reopened = Agent::open_embedded(root.into(), config(), model).unwrap();
    reopened.resume_chat(branch.child_chat_id).await.unwrap();
    (reopened, branch.child_chat_id)
}

#[tokio::test]
async fn string_tool_results_from_earlier_transcripts_resume_rewind_and_branch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::write(directory.path().join("notes.txt"), "DELIVERY-FRIDAY\n").unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "read-1",
                "Read",
                json!({"file_path": directory.path().join("notes.txt")}),
            ),
            text("read answer"),
            text("answer to ABANDONED-MARKER"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, chat, None, "read the notes").await;
    complete_turn(&agent, chat, Some(first.loop_id), "ABANDONED-MARKER prompt").await;
    agent.shutdown().await;
    drop(agent);
    let stored = edit_stored_tool_result(&transcript_file(&root, chat), "read-1", |_| {});
    let stored = stored
        .as_str()
        .expect("results are stored as plain strings")
        .to_owned();
    assert!(stored.contains("DELIVERY-FRIDAY"), "{stored}");

    let resumed = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    resumed.resume_chat(chat).await.unwrap();
    let checkpoint = resumed.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    resumed
        .rewind(chat, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();

    let model = Arc::new(ScriptedModel::new(vec![text("branch answer")]));
    let (branched, child) = branch_and_reopen(resumed, &root, chat, model.clone()).await;
    complete_turn(&branched, child, None, "continue the branch").await;
    let request = model.requests.lock().unwrap()[0].clone();
    assert_eq!(
        tool_results(&request),
        vec![("read-1".into(), ToolResultContent::Text(stored))]
    );
    assert!(!request_text(&request).contains("ABANDONED-MARKER"));
    branched.shutdown().await;
}

#[tokio::test]
async fn block_array_tool_results_round_trip_through_resume_and_branch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::write(directory.path().join("notes.txt"), "DELIVERY-FRIDAY\n").unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "read-1",
                "Read",
                json!({"file_path": directory.path().join("notes.txt")}),
            ),
            text("read answer"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, None, "read the notes").await;
    agent.shutdown().await;
    drop(agent);
    let blocks = json!([
        {"type":"text","text":"BLOCK-ONE"},
        {"type":"text","text":"BLOCK-TWO"}
    ]);
    edit_stored_tool_result(&transcript_file(&root, chat), "read-1", |content| {
        *content = blocks.clone();
    });

    let resumed = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    resumed.resume_chat(chat).await.unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("branch answer")]));
    let (branched, child) = branch_and_reopen(resumed, &root, chat, model.clone()).await;
    complete_turn(&branched, child, None, "continue the branch").await;
    branched.shutdown().await;
    assert_eq!(
        tool_results(&model.requests.lock().unwrap()[0]),
        vec![(
            "read-1".into(),
            ToolResultContent::Blocks(vec![
                ToolResultBlock::Text {
                    text: "BLOCK-ONE".into()
                },
                ToolResultBlock::Text {
                    text: "BLOCK-TWO".into()
                },
            ])
        )]
    );
    assert_eq!(
        edit_stored_tool_result(&transcript_file(&root, child), "read-1", |_| {}),
        blocks
    );
}

#[tokio::test]
async fn resuming_a_loaded_chat_returns_the_live_instance() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::open_embedded(
        directory.path().join("root"),
        config(),
        Arc::new(ScriptedModel::new(vec![call(
            "write",
            "Write",
            json!({"file_path":"approval.txt","content":"pending"}),
        )])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_workspace_write_approval(&agent, chat).await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "write a file".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let resumed = agent.resume_chat(chat).await.unwrap();
    assert_eq!(resumed.permissions.len(), 1);
    assert_eq!(agent.list_chats().await.len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn transcript_file_is_created_lazily_and_stays_after_cwd_changes() {
    let directory = tempfile::tempdir().unwrap();
    let first_directory = directory.path().join("first");
    let second_directory = directory.path().join("second");
    std::fs::create_dir(&first_directory).unwrap();
    std::fs::create_dir(&second_directory).unwrap();
    let agent = Agent::open_embedded(
        directory.path().join("root"),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer after moving")])),
    )
    .unwrap();
    let empty = agent.create_chat(first_directory.clone()).await.unwrap();
    agent
        .set_execution_mode(empty, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    assert!(
        agent
            .saved_chats(&first_directory)
            .await
            .unwrap()
            .is_empty()
    );

    let moved = agent.create_chat(first_directory.clone()).await.unwrap();
    agent
        .set_directories(moved, second_directory.clone(), Vec::new())
        .await
        .unwrap();
    complete_turn(&agent, moved, None, "prompt after moving").await;
    let first_listing = agent.saved_chats(&first_directory).await.unwrap();
    assert_eq!(
        first_listing.iter().map(|chat| chat.id).collect::<Vec<_>>(),
        vec![moved]
    );
    assert_eq!(
        first_listing[0].first_prompt.as_deref(),
        Some("prompt after moving")
    );
    assert!(
        agent
            .saved_chats(&second_directory)
            .await
            .unwrap()
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn long_working_directory_names_share_one_project() {
    let directory = tempfile::tempdir().unwrap();
    let mut workdir = directory.path().to_path_buf();
    for segment in 0..4 {
        workdir.push(format!(
            "{segment}-{}",
            "nested-project-directory".repeat(3)
        ));
    }
    std::fs::create_dir_all(&workdir).unwrap();
    assert!(workdir.canonicalize().unwrap().as_os_str().len() > 200);
    let root = directory.path().join("root");
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("long path answer")])),
    )
    .unwrap();
    let chat = first.create_chat(workdir.clone()).await.unwrap();
    complete_turn(&first, chat, None, "long path prompt").await;
    first.flush_transcripts().await;

    let second = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let saved = second.saved_chats(&workdir).await.unwrap();
    assert_eq!(
        saved.iter().map(|chat| chat.id).collect::<Vec<_>>(),
        vec![chat]
    );
    assert_eq!(second.resume_chat(chat).await.unwrap().messages.len(), 2);
    let project = transcript_file(&root, chat);
    let project_name = project.parent().unwrap().file_name().unwrap().len();
    assert!(project_name <= 255 && project_name > 200);
    first.shutdown().await;
    second.shutdown().await;
    assert_path_is_private(&project);
}

#[tokio::test]
async fn interruption_while_waiting_for_permission_leaves_no_pending_work_after_resume() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let original = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![call(
            "write",
            "Write",
            json!({"file_path":"approval.txt","content":"pending"}),
        )])),
    )
    .unwrap();
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    require_workspace_write_approval(&original, chat).await;
    let mut events = original.subscribe();
    original
        .submit(chat, None, "write a file".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    original.flush_transcripts().await;

    // 原进程停在等待权限处，另一个实例恢复同一 Chat，相当于进程在此退出后重启。
    let resumed =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert!(restored.permissions.is_empty());
    assert!(restored.operations.is_empty());
    resumed
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: Some("after interruption".into()),
        })
        .await
        .expect("an interrupted permission request must not keep the Chat busy");
    resumed.shutdown().await;
    original.shutdown().await;
}

#[tokio::test]
async fn another_instance_without_new_messages_keeps_the_rewound_branch_latest() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let origin = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("shared answer")])),
    )
    .unwrap();
    let chat = origin.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&origin, chat, None, "shared prompt").await;
    origin.shutdown().await;
    drop(origin);

    let idle_instance = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let rewinding = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("answer to ABANDONED-MARKER"),
            text("replacement answer"),
        ])),
    )
    .unwrap();
    idle_instance.resume_chat(chat).await.unwrap();
    rewinding.resume_chat(chat).await.unwrap();
    complete_turn(&rewinding, chat, None, "ABANDONED-MARKER prompt").await;
    let checkpoint = rewinding.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    rewinding
        .rewind(chat, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    complete_turn(&rewinding, chat, None, "replacement prompt").await;
    rewinding.flush_transcripts().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    idle_instance
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    idle_instance.flush_transcripts().await;
    rewinding.shutdown().await;
    idle_instance.shutdown().await;

    let model = Arc::new(ScriptedModel::new(vec![text("after the rewind")]));
    let latest = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let restored = latest.resume_chat(chat).await.unwrap();
    // 被 rewind 截断的 Loop 不在新的对话链上：共享提示、rewind 草稿与替换提示各一个。
    assert_eq!(restored.loops.len(), 3);
    complete_turn(&latest, chat, None, "continue after the rewind").await;
    let sent = request_text(&model.requests.lock().unwrap()[0]);
    assert!(sent.contains("shared prompt"), "{sent}");
    assert!(sent.contains("replacement answer"), "{sent}");
    assert!(!sent.contains("answer to ABANDONED-MARKER"), "{sent}");
    latest.shutdown().await;
}

#[tokio::test]
async fn failed_transcript_writes_are_reported_and_retried_once_writable() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("warm answer"),
            text("answer while read-only"),
            text("answer after recovery"),
        ])),
    )
    .unwrap();
    let warm = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, warm, None, "warm prompt").await;
    agent.flush_transcripts().await;
    let project = transcript_file(&root, warm).parent().unwrap().to_path_buf();
    let set_mode =
        |mode| std::fs::set_permissions(&project, std::fs::Permissions::from_mode(mode)).unwrap();

    set_mode(0o500);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, chat, None, "prompt while read-only").await;
    agent.flush_transcripts().await;
    let error = agent.transcript_write_error();
    let path = project.join(format!("{chat}.jsonl"));
    set_mode(0o700);
    assert!(error.is_some());
    assert!(!path.exists());

    complete_turn(&agent, chat, Some(first.loop_id), "prompt after recovery").await;
    agent.flush_transcripts().await;
    assert_eq!(agent.transcript_write_error(), None);
    agent.shutdown().await;
    drop(agent);
    let written = std::fs::read_to_string(&path).unwrap();
    let retried_prompts = written
        .lines()
        .filter(|line| {
            line.starts_with("{\"type\":\"user\"") && line.contains("prompt while read-only")
        })
        .count();
    assert_eq!(retried_prompts, 1, "{written}");

    let model = Arc::new(ScriptedModel::new(vec![text("resumed answer")]));
    let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
    restored.resume_chat(chat).await.unwrap();
    complete_turn(&restored, chat, Some(first.loop_id), "resumed prompt").await;
    assert_eq!(
        conversation(&model.requests.lock().unwrap()[0]),
        vec![
            user("prompt while read-only"),
            assistant("answer while read-only"),
            user("prompt after recovery"),
            assistant("answer after recovery"),
            user("resumed prompt"),
        ]
    );
    restored.shutdown().await;
}

/// 恢复的 Chat 不会立即写 Transcript；期间文件被外部删除（例如过期清理）后，继续对话既不重建只含后续
/// 条目的断链文件，退出时也不写出元数据空壳，并通过写入错误提示后续内容不再保存。
#[tokio::test]
async fn a_resumed_chat_whose_transcript_was_deleted_is_not_recreated() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("saved answer")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, None, "saved prompt").await;
    agent.shutdown().await;
    drop(agent);
    let path = transcript_file(&root, chat);

    let restored = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer after deletion")])),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    complete_turn(&restored, chat, None, "prompt after deletion").await;
    restored.flush_transcripts().await;

    let error = restored
        .transcript_write_error()
        .expect("the deleted Transcript is reported");
    assert!(error.contains("was deleted"), "{error}");
    restored.shutdown().await;
    assert!(!path.exists());
    assert_eq!(restored.transcript_write_error(), Some(error));
}

#[tokio::test]
async fn the_working_directory_resumes_from_the_last_recorded_entry() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("first answer"),
            text("answer in the right directory"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, None, "first prompt").await;
    let directories = [
        directory.path().join("left"),
        directory.path().join("right"),
    ];
    for path in &directories {
        std::fs::create_dir(path).unwrap();
        agent
            .set_directories(chat, path.clone(), Vec::new())
            .await
            .unwrap();
    }
    complete_turn(&agent, chat, None, "prompt in the right directory").await;
    agent.flush_transcripts().await;
    // 之后的切换没有写出任何新条目，不在 Transcript 中。
    agent
        .set_directories(chat, directories[0].clone(), Vec::new())
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.workdir, directories[1].canonicalize().unwrap());
    restored.shutdown().await;
}

fn assert_path_is_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn resume_filtering_after_a_compaction_keeps_the_boundary_chain_stable() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let origin = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("first answer"),
            text("compacted summary"),
            text("   "),
            text("third answer"),
        ])),
    )
    .unwrap();
    let chat = origin.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&origin, chat, None, "first prompt").await;
    origin.compact(chat).await.unwrap();
    complete_turn(&origin, chat, None, "second prompt").await;
    complete_turn(&origin, chat, None, "third prompt").await;
    origin.shutdown().await;
    drop(origin);

    // 照 Claude Code，恢复时只加载边界之后的链，并删除只含空白的回复。
    let first_model = Arc::new(ScriptedModel::new(vec![text("fourth answer")]));
    let first_resume = Agent::open_embedded(root.clone(), config(), first_model.clone()).unwrap();
    let restored = first_resume.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages.len(), 5);
    assert!(restored.is_compact_boundary(0));
    assert!(restored.is_compact_summary(1));
    assert_eq!(restored.rewind_points().len(), 2);
    complete_turn(&first_resume, chat, None, "fourth prompt").await;
    let sent = request_text(&first_model.requests.lock().unwrap()[0]);
    assert!(sent.contains("compacted summary"), "{sent}");
    assert!(!sent.contains("first answer"), "{sent}");
    first_resume.shutdown().await;
    drop(first_resume);

    // 被删除的消息仍在链上，再次恢复时同样被删除。
    let second_model = Arc::new(ScriptedModel::new(vec![text("fifth answer")]));
    let second_resume = Agent::open_embedded(root, config(), second_model.clone()).unwrap();
    let restored = second_resume.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages.len(), 7);
    assert_eq!(restored.rewind_points().len(), 3);
    complete_turn(&second_resume, chat, None, "fifth prompt").await;
    let sent = request_text(&second_model.requests.lock().unwrap()[0]);
    assert!(sent.contains("compacted summary"), "{sent}");
    assert!(sent.contains("fourth answer"), "{sent}");
    assert!(!sent.contains("first answer"), "{sent}");
    second_resume.shutdown().await;
}

/// 规格"会话内容的表示"中列出的 Transcript 条目类型。
const SPECIFIED_LINE_TYPES: [&str; 8] = [
    "user",
    "assistant",
    "system",
    "custom-title",
    "last-prompt",
    "branch-origin",
    "file-history-snapshot",
    "content-replacement",
];

/// Loop id、目标、计划（版本与内容）与进度步骤。
type LoopPlanState = (LoopId, String, Option<(u64, String)>, Option<String>);

fn plan_and_progress(snapshot: &tickloop_agent::runtime::ChatSnapshot) -> Vec<LoopPlanState> {
    let mut state = snapshot
        .loops
        .iter()
        .map(|goal| {
            (
                goal.id,
                goal.goal.clone(),
                snapshot
                    .plans
                    .iter()
                    .find(|plan| plan.loop_id == goal.id)
                    .map(|plan| (plan.version, plan.markdown.clone())),
                snapshot
                    .progress
                    .iter()
                    .find(|progress| progress.loop_id == goal.id)
                    .map(|progress| format!("{:?}", progress.todos)),
            )
        })
        .collect::<Vec<_>>();
    state.sort_by_key(|(loop_id, ..)| *loop_id);
    state
}

fn compaction_positions(snapshot: &tickloop_agent::runtime::ChatSnapshot) -> Vec<(usize, bool)> {
    (0..snapshot.messages.len())
        .filter(|index| snapshot.is_compact_boundary(*index) || snapshot.is_compact_summary(*index))
        .map(|index| (index, snapshot.is_compact_boundary(index)))
        .collect()
}

fn rewind_positions(
    snapshot: &tickloop_agent::runtime::ChatSnapshot,
) -> Vec<(Uuid, usize, String, ExecutionMode)> {
    snapshot
        .rewind_points()
        .into_iter()
        .map(|point| {
            (
                point.message_id,
                point.message_index,
                point.prompt,
                point.execution_mode,
            )
        })
        .collect()
}

#[tokio::test]
async fn transcripts_hold_only_specified_entries_and_restart_rebuilds_the_chat_from_the_chain() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let other = directory.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let lines = (0..2000)
        .map(|index| format!("delivery item {index:04} is waiting at the dock"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(other.join("notes.txt"), lines).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "Deliver the report."),
        call(
            "progress-1",
            "TodoWrite",
            json!({"todos":[{"content":"Write the report","status":"in_progress","activeForm":"Writing the report"}]}),
        ),
        text("Planned."),
        text("Summary of the planning turn."),
        call(
            "write-1",
            "Write",
            json!({"file_path":"report.txt","content":"first draft"}),
        ),
        call(
            "search",
            "Grep",
            json!({"path":"notes.txt","pattern":"delivery","output_mode":"content","head_limit":0}),
        ),
        text("First draft written."),
        call(
            "write-2",
            "Write",
            json!({"file_path":"report.txt","content":"second draft"}),
        ),
        call(
            "progress-2",
            "TodoWrite",
            json!({"todos":[{"content":"Write the report","status":"completed","activeForm":"Writing the report"}]}),
        ),
        text("Second draft written."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let goal = complete_turn(&agent, chat, None, "Plan the report")
        .await
        .loop_id;
    agent.compact(chat).await.unwrap();
    agent
        .set_directories(chat, other.clone(), Vec::new())
        .await
        .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Allow,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    complete_turn(&agent, chat, Some(goal), "Write the first draft").await;
    complete_turn(&agent, chat, Some(goal), "Write the second draft").await;
    let child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: Some("report branch".into()),
        })
        .await
        .unwrap();

    let before = agent.snapshot(chat).await.unwrap();
    let child_before = agent.snapshot(child.child_chat_id).await.unwrap();
    assert!(
        before
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(
                |content| matches!(content, Content::ToolResult { content, .. }
                if content.text().starts_with("<persisted-output>"))
            )
    );
    let second_draft_point = before
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "Write the second draft")
        .unwrap()
        .message_id;
    let files_before = agent
        .rewind_file_changes(chat, second_draft_point)
        .await
        .unwrap();
    assert!(files_before.as_ref().is_some_and(|files| !files.is_empty()));
    assert!(!plan_and_progress(&before).is_empty());
    agent.shutdown().await;
    drop(agent);

    for project in std::fs::read_dir(root.join("projects")).unwrap() {
        for file in std::fs::read_dir(project.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let entry = serde_json::from_str::<serde_json::Value>(line).unwrap();
                let kind = entry["type"].as_str().unwrap_or_default();
                assert!(
                    SPECIFIED_LINE_TYPES.contains(&kind),
                    "unexpected {kind} entry in {}: {line}",
                    path.display()
                );
            }
        }
    }

    let restarted =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let restored_child = restarted.resume_chat(child.child_chat_id).await.unwrap();
    assert_eq!(restored.workdir, before.workdir);
    assert_eq!(restored.workdir, other.canonicalize().unwrap());
    assert_eq!(plan_and_progress(&restored), plan_and_progress(&before));
    // 重启只加载最后一个压缩边界之后的链：进程内的位置减去边界下标后应一致。
    let chain_root = compaction_positions(&before)[0].0;
    assert_eq!(
        compaction_positions(&restored),
        compaction_positions(&before)
            .into_iter()
            .map(|(index, is_boundary)| (index - chain_root, is_boundary))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        restored.conversation_messages().unwrap(),
        before.conversation_messages().unwrap()
    );
    assert_eq!(
        rewind_positions(&restored),
        rewind_positions(&before)
            .into_iter()
            .filter(|(_, index, _, _)| *index > chain_root)
            .map(|(id, index, prompt, mode)| (id, index - chain_root, prompt, mode))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        restarted
            .rewind_file_changes(chat, second_draft_point)
            .await
            .unwrap(),
        files_before
    );
    assert_eq!(
        restored_child
            .branch_origin
            .as_ref()
            .map(|origin| origin.source_chat_id),
        Some(chat)
    );
    assert_eq!(restored_child.branch_name(), child_before.branch_name());
    assert_eq!(restored_child.workdir, child_before.workdir);
    assert_eq!(
        plan_and_progress(&restored_child),
        plan_and_progress(&child_before)
    );
    assert_eq!(
        restored_child.conversation_messages().unwrap(),
        child_before.conversation_messages().unwrap()
    );
    restarted.shutdown().await;
}
