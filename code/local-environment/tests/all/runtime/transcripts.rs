use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    AgentHostOptions, BranchRequest, ChatId, Content, Decision, EventKind, ExecutionMode, LoopId,
    LoopState, Message, ModelIdentity, PauseReason, PermissionRule, PromptPolicy, Protocol,
    Receipt, Receiving, RewindMode, Role, ToolResultBlock, ToolResultContent, Usage,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::{
    ScriptedModel, call, config, is_tool_result_line, next_event, submit_plan, text,
    transcript_file, truncate_transcript_before,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const NO_RESPONSE_REQUESTED: &str = "No response requested.";

async fn require_workspace_write_approval(agent: &Agent, chat: Uuid) {
    agent
        .set_permission_rules(
            tickloop_ids::Id::from(chat),
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

/// A model replying in order; `None` means this request waits until the run is cancelled.
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
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
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

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
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
        .filter(|message| tickloop_harness::test_support::is_conversation(message))
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
        complete_turn(&first, first_chat, "prompt for first host"),
        complete_turn(&second, second_chat, "prompt for second host"),
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
        complete_turn(&third, chat, "continue").await;
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
    complete_turn(&origin, chat, "shared prompt").await;
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
    complete_turn(&left, chat, "from the left host").await;
    left.flush_transcripts().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    complete_turn(&right, chat, "from the right host").await;
    left.shutdown().await;
    right.shutdown().await;

    let model = Arc::new(ScriptedModel::new(vec![text("after the fork")]));
    let latest = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    latest.resume_chat(chat).await.unwrap();
    complete_turn(&latest, chat, "which branch").await;
    let sent = request_text(&model.requests.lock().unwrap()[0]);
    assert!(sent.contains("shared prompt"), "{sent}");
    assert!(sent.contains("from the right host"), "{sent}");
    assert!(!sent.contains("from the left host"), "{sent}");
    latest.shutdown().await;
    let file = std::fs::read_to_string(transcript_file(&root, chat.uuid())).unwrap();
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
    complete_turn(&agent, chat, "first prompt").await;
    complete_turn(&agent, chat, "second prompt").await;
    agent.shutdown().await;
    drop(agent);

    let path = transcript_file(&root, chat.uuid());
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
    complete_turn(&restored, chat, "third prompt").await;
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
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    std::fs::write(directory.join("chat"), chat.uuid().to_string()).unwrap();
    complete_turn(&agent, chat, "first prompt").await;
    agent
        .submit(chat, "prompt before the kill".into())
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
            tickloop_harness::test_support::test_path!(killed_process_fixture).as_str(),
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
        .parse::<uuid::Uuid>()
        .unwrap()
        .into();

    let model = Arc::new(ScriptedModel::new(vec![text("resumed answer")]));
    let restored =
        Agent::open_embedded(directory.path().join("root"), config(), model.clone()).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert_ne!(snapshot.chat_loop.state, LoopState::Running);
    complete_turn(&restored, chat, "after the kill").await;
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
    // Ends with a tool result: following Claude Code's resume the user starts, only the sentinel
    // is appended, no "continue from where you left off" prompt, and no Run starts.
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
        complete_turn(&agent, chat, "read the notes").await;
        agent.shutdown().await;
        drop(agent);
        truncate_transcript_before(&transcript_file(&root, chat.uuid()), |line| {
            line.contains("final answer that was lost")
        });

        let model = Arc::new(ScriptedModel::new(vec![text("continued")]));
        let restored = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
        let resumed = restored.resume_chat(chat).await.unwrap();
        // No Run starts on resume: the first model request is the one the next prompt makes.
        assert_eq!(resumed.chat_loop.state, LoopState::Interrupted);
        assert!(model.requests.lock().unwrap().is_empty());
        let resumed_messages = resumed.conversation_messages().unwrap();
        assert_eq!(
            resumed_messages.last(),
            Some(&assistant(NO_RESPONSE_REQUESTED))
        );
        assert!(
            !format!("{resumed_messages:?}").contains("Continue from where you left off."),
            "{resumed_messages:?}"
        );
        complete_turn(&restored, chat, "next prompt").await;
        let lines = std::fs::read_to_string(transcript_file(&root, chat.uuid()))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect::<Vec<tickloop_harness::transcript::Line>>();
        let displayed =
            serde_json::to_string(&tickloop_harness::transcript::display::project(&lines)).unwrap();
        assert!(displayed.contains("next prompt"), "{displayed}");
        assert!(!displayed.contains(NO_RESPONSE_REQUESTED));
        assert!(!displayed.contains("Continue from where you left off."));
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
            [assistant(NO_RESPONSE_REQUESTED), user("next prompt")]
        );
        restored.shutdown().await;
    }
    // Tool results were not written: assistant messages whose tool_uses all lack results are
    // filtered out, and the sentinel follows the prompt.
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
        complete_turn(&agent, chat, "read the notes").await;
        agent.shutdown().await;
        drop(agent);
        truncate_transcript_before(&transcript_file(&root, chat.uuid()), |line| {
            is_tool_result_line(line, "read")
        });

        let model = Arc::new(ScriptedModel::new(vec![text("continued")]));
        let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
        restored.resume_chat(chat).await.unwrap();
        complete_turn(&restored, chat, "next prompt").await;
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
        complete_turn(&agent, chat, "think only").await;
        complete_turn(&agent, chat, "answer now").await;
        agent.shutdown().await;
        drop(agent);

        let model = Arc::new(ScriptedModel::new(vec![text("continued")]));
        let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
        restored.resume_chat(chat).await.unwrap();
        complete_turn(&restored, chat, "next prompt").await;
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
async fn a_restored_loop_is_interrupted_and_paused_until_a_new_input() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = SequenceModel::new(vec![Some(text("watching")), None]);
    let original = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&original, chat, "watch for events").await;
    original
        .submit(chat, "long running work".into())
        .await
        .unwrap();
    let live = original.snapshot(chat).await.unwrap();
    assert_eq!(live.chat_loop.state, LoopState::Running);
    original.flush_transcripts().await;

    // While the original process is still running, another instance resumes the same Chat.
    let resumed_model = Arc::new(ScriptedModel::new(vec![text("trigger resumed")]));
    let other = Agent::open_embedded(root, config(), resumed_model.clone()).unwrap();
    let restored = other.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, live.chat_loop.id);
    assert_eq!(restored.chat_loop.state, LoopState::Interrupted);
    assert_eq!(
        restored.chat_loop.state_since, None,
        "a recovery conversion does not reuse the running state's time"
    );
    assert_eq!(
        restored.chat_loop.receiving,
        Receiving::Paused {
            reason: PauseReason::HistoryLoaded
        }
    );
    assert!(resumed_model.requests.lock().unwrap().is_empty());

    complete_turn(&other, chat, "resume receiving").await;
    let resumed = other.snapshot(chat).await.unwrap();
    assert!(resumed.chat_loop.receiving.is_receiving());
    assert_eq!(resumed.chat_loop.state, LoopState::Idle);
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
    complete_turn(&agent, source, "kept prompt").await;
    complete_turn(&agent, source, "ABANDONED-MARKER prompt").await;
    let checkpoint = agent.snapshot(source).await.unwrap().rewind_points()[1].clone();
    agent
        .rewind(source, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
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
    complete_turn(&reopened, branch.child_chat_id, "continue the branch").await;
    let sent = request_text(&model.requests.lock().unwrap()[0]);
    assert!(sent.contains("kept prompt"), "{sent}");
    assert!(!sent.contains("ABANDONED-MARKER"), "{sent}");
    reopened.shutdown().await;
    let child_file =
        std::fs::read_to_string(transcript_file(&root, branch.child_chat_id.uuid())).unwrap();
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

/// Reads or rewrites the raw `content` JSON of a tool result entry in the Transcript.
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

/// Branches from the loaded source Chat, closes, reopens with `model` and resumes the child Chat,
/// so it can only be read back from the Transcript.
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
    complete_turn(&agent, chat, "read the notes").await;
    complete_turn(&agent, chat, "ABANDONED-MARKER prompt").await;
    agent.shutdown().await;
    drop(agent);
    let stored = edit_stored_tool_result(&transcript_file(&root, chat.uuid()), "read-1", |_| {});
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
    complete_turn(&branched, child, "continue the branch").await;
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
    complete_turn(&agent, chat, "read the notes").await;
    agent.shutdown().await;
    drop(agent);
    let blocks = json!([
        {"type":"text","text":"BLOCK-ONE"},
        {"type":"text","text":"BLOCK-TWO"}
    ]);
    edit_stored_tool_result(&transcript_file(&root, chat.uuid()), "read-1", |content| {
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
    complete_turn(&branched, child, "continue the branch").await;
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
        edit_stored_tool_result(&transcript_file(&root, child.uuid()), "read-1", |_| {}),
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
    require_workspace_write_approval(&agent, chat.uuid()).await;
    let mut events = agent.subscribe();
    agent.submit(chat, "write a file".into()).await.unwrap();
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
    complete_turn(&agent, moved, "prompt after moving").await;
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
    complete_turn(&first, chat, "long path prompt").await;
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
    let project = transcript_file(&root, chat.uuid());
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
    require_workspace_write_approval(&original, chat.uuid()).await;
    let mut events = original.subscribe();
    original.submit(chat, "write a file".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    original.flush_transcripts().await;

    // The original process stopped while waiting for permission and another instance resumes the
    // same Chat, equivalent to the process exiting here and restarting.
    let resumed =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert!(restored.permissions.is_empty());
    assert!(restored.operations.is_empty());
    resumed
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
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
    complete_turn(&origin, chat, "shared prompt").await;
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
    complete_turn(&rewinding, chat, "ABANDONED-MARKER prompt").await;
    let checkpoint = rewinding.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    rewinding
        .rewind(chat, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    complete_turn(&rewinding, chat, "replacement prompt").await;
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
    assert_eq!(
        restored
            .rewind_points()
            .iter()
            .map(|point| point.prompt.as_str())
            .collect::<Vec<_>>(),
        ["shared prompt", "replacement prompt"]
    );
    complete_turn(&latest, chat, "continue after the rewind").await;
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
    complete_turn(&agent, warm, "warm prompt").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while agent
            .snapshot(warm)
            .await
            .unwrap()
            .generated_title
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent.flush_transcripts().await;
    let project = transcript_file(&root, warm.uuid())
        .parent()
        .unwrap()
        .to_path_buf();
    let set_mode =
        |mode| std::fs::set_permissions(&project, std::fs::Permissions::from_mode(mode)).unwrap();

    set_mode(0o500);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "prompt while read-only").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .generated_title
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent.flush_transcripts().await;
    let error = agent.transcript_write_error();
    let path = project.join(format!("{}.jsonl", chat.uuid()));
    set_mode(0o700);
    assert!(error.is_some());
    assert!(!path.exists());

    complete_turn(&agent, chat, "prompt after recovery").await;
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
    complete_turn(&restored, chat, "resumed prompt").await;
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

/// A resumed Chat does not write its Transcript immediately; if the file is deleted externally
/// meanwhile, continuing the conversation neither recreates a broken-chain file containing only
/// later entries nor writes an empty metadata shell on exit, and a write error signals that later
/// content is no longer saved.
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
    complete_turn(&agent, chat, "saved prompt").await;
    agent.shutdown().await;
    drop(agent);
    let path = transcript_file(&root, chat.uuid());

    let restored = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer after deletion")])),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    complete_turn(&restored, chat, "prompt after deletion").await;
    restored.flush_transcripts().await;

    let error = restored
        .transcript_write_error()
        .expect("the deleted Transcript is reported");
    assert!(error.contains("was deleted"), "{error}");
    restored.shutdown().await;
    assert!(!path.exists());
    assert_eq!(restored.transcript_write_error(), Some(error));
}

/// External removal must not recreate a disconnected Transcript or a metadata-only shell.
#[tokio::test]
async fn externally_removing_the_transcript_of_a_running_chat_stops_saving_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("saved answer"),
            text("unsaved answer"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    complete_turn(&agent, chat, "saved prompt").await;
    agent.flush_transcripts().await;
    let transcript = transcript_file(&root, chat.uuid());

    std::fs::remove_file(&transcript).unwrap();

    complete_turn(&agent, chat, "prompt after removal").await;
    agent.flush_transcripts().await;
    let error = agent
        .transcript_write_error()
        .expect("the deleted Transcript is reported");
    assert!(error.contains("was deleted"), "{error}");
    agent.shutdown().await;
    assert!(!transcript.exists());
    let transcripts = std::fs::read_dir(transcript.parent().unwrap())
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert!(transcripts.is_empty(), "{transcripts:?}");
}

/// Following Claude Code's `shouldSkipPersistence`: when `cleanupPeriodDays` is 0, conversations
/// produce no Transcript, but user settings are still read and written and existing records are not
/// deleted.
#[tokio::test]
async fn a_zero_cleanup_period_writes_no_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let earlier_transcript = root.join(format!("projects/-earlier/{}.jsonl", Uuid::new_v4()));
    std::fs::create_dir_all(earlier_transcript.parent().unwrap()).unwrap();
    std::fs::write(&earlier_transcript, "{}\n").unwrap();
    std::fs::write(root.join("settings.json"), r#"{"cleanupPeriodDays": 0}"#).unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Not saved.")])),
    )
    .unwrap();
    assert!(!agent.saves_chats());

    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    complete_turn(&agent, chat, "Keep this private").await;
    tickloop_code_settings::UserSettings::in_root(&root)
        .save_thinking_enabled(false)
        .unwrap();
    agent.flush_transcripts().await;
    agent.shutdown().await;

    let project_entries = std::fs::read_dir(root.join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(project_entries, vec![std::ffi::OsString::from("-earlier")]);
    assert!(earlier_transcript.exists());
    let settings: Value =
        serde_json::from_slice(&std::fs::read(root.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["alwaysThinkingEnabled"], false);
    assert!(settings.get("contextWindowTokens").is_none());
    assert_eq!(settings["cleanupPeriodDays"], 0);
    assert!(agent.saved_chats(&workdir).await.unwrap().is_empty());
    let reopened =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    assert!(matches!(
        reopened.resume_chat(chat).await,
        Err(tickloop_harness::runtime::AgentError::NotFound)
    ));
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
    complete_turn(&agent, chat, "first prompt").await;
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
    complete_turn(&agent, chat, "prompt in the right directory").await;
    agent.flush_transcripts().await;
    // Later switches wrote no new entries and are not in the Transcript.
    agent
        .set_directories(chat, directories[0].clone(), Vec::new())
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert_eq!(
        snapshot.folder,
        Some(directories[1].canonicalize().unwrap())
    );
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
    complete_turn(&origin, chat, "first prompt").await;
    origin.compact(chat).await.unwrap();
    complete_turn(&origin, chat, "second prompt").await;
    complete_turn(&origin, chat, "third prompt").await;
    origin.shutdown().await;
    drop(origin);

    // Following Claude Code, resume loads only the chain after the boundary and removes
    // whitespace-only replies.
    let first_model = Arc::new(ScriptedModel::new(vec![text("fourth answer")]));
    let first_resume = Agent::open_embedded(root.clone(), config(), first_model.clone()).unwrap();
    let restored = first_resume.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages.len(), 5);
    assert!(restored.is_compact_boundary(0));
    assert!(restored.is_compact_summary(1));
    assert_eq!(restored.rewind_points().len(), 2);
    complete_turn(&first_resume, chat, "fourth prompt").await;
    let sent = request_text(&first_model.requests.lock().unwrap()[0]);
    assert!(sent.contains("compacted summary"), "{sent}");
    assert!(!sent.contains("first answer"), "{sent}");
    first_resume.shutdown().await;
    drop(first_resume);

    // Removed messages are still on the chain and are removed again on the next resume.
    let second_model = Arc::new(ScriptedModel::new(vec![text("fifth answer")]));
    let second_resume = Agent::open_embedded(root, config(), second_model.clone()).unwrap();
    let restored = second_resume.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages.len(), 7);
    assert_eq!(restored.rewind_points().len(), 3);
    complete_turn(&second_resume, chat, "fifth prompt").await;
    let sent = request_text(&second_model.requests.lock().unwrap()[0]);
    assert!(sent.contains("compacted summary"), "{sent}");
    assert!(sent.contains("fourth answer"), "{sent}");
    assert!(!sent.contains("first answer"), "{sent}");
    second_resume.shutdown().await;
}

/// Transcript entry types listed in the spec section "Representation of session content".
const SPECIFIED_LINE_TYPES: [&str; 20] = [
    "acp-session",
    "recorded-edit",
    "user",
    "assistant",
    "system",
    "custom-title",
    "ai-title",
    "subagents",
    "family-completions",
    "family-inputs",
    "family-message-hold",
    "sent-agent-messages",
    "last-prompt",
    "branch-origin",
    "file-history-snapshot",
    "content-replacement",
    "response",
    "input",
    "wait",
    "retained-snapshot",
];

/// Loop id, plan (version and content) and progress steps.
type LoopPlanState = (LoopId, Option<(u64, String)>, Option<String>);

fn plan_and_progress(snapshot: &tickloop_harness::protocol::ChatSnapshot) -> LoopPlanState {
    let loop_id = snapshot.chat_loop.id;
    (
        loop_id,
        snapshot
            .plans
            .iter()
            .find(|plan| plan.loop_id == loop_id)
            .map(|plan| (plan.version, plan.markdown.clone())),
        snapshot
            .progress
            .iter()
            .find(|progress| progress.loop_id == loop_id)
            .map(|progress| format!("{:?}", progress.todos)),
    )
}

fn compaction_positions(snapshot: &tickloop_harness::protocol::ChatSnapshot) -> Vec<(usize, bool)> {
    (0..snapshot.messages.len())
        .filter(|index| snapshot.is_compact_boundary(*index) || snapshot.is_compact_summary(*index))
        .map(|index| (index, snapshot.is_compact_boundary(index)))
        .collect()
}

fn rewind_positions(
    snapshot: &tickloop_harness::protocol::ChatSnapshot,
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
        text("Second draft written."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Plan the report".into()).await.unwrap();
    loop {
        if let EventKind::PermissionRequested { request } = next_event(&mut events).await.kind {
            assert_eq!(request.name, "ExitPlanMode");
            agent
                .approve_plan(chat, 1, ExecutionMode::AcceptEdits)
                .await
                .unwrap();
            break;
        }
    }
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
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
    complete_turn(&agent, chat, "Write the first draft").await;
    complete_turn(&agent, chat, "Write the second draft").await;
    let child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
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
    let (_, plan, progress) = plan_and_progress(&before);
    assert!(plan.is_some() || progress.is_some());
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
    assert_eq!(restored.folder, before.folder);
    assert_eq!(restored.folder, Some(other.canonicalize().unwrap()));
    assert_eq!(plan_and_progress(&restored), plan_and_progress(&before));
    // A restart loads only the chain after the last compaction boundary: in-process positions minus
    // the boundary index should match.
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
    assert_eq!(restored_child.folder, child_before.folder);
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

#[tokio::test]
async fn artifact_ticks_survive_restart_branch_and_conversation_rewind() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![text("first"), text("second")]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "first prompt").await;
    let doc = tickloop_harness::protocol::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Research".into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1000,
    };
    let slides = tickloop_harness::protocol::ArtifactTick {
        artifact_id: "deck".into(),
        title: "Briefing".into(),
        format: "slides".into(),
        url: "https://docs.example/a/deck".into(),
        ..doc.clone()
    };
    let ticks = vec![doc.clone(), slides.clone()];
    agent.add_artifact_tick(chat, doc).await.unwrap();
    agent.add_artifact_tick(chat, slides).await.unwrap();
    complete_turn(&agent, chat, "second prompt").await;
    let host_request = request_text(model.requests.lock().unwrap().last().unwrap());
    // The linked artifacts reach the model as a note, never in the system prompt.
    assert!(
        host_request.contains("Artifacts linked to this Chat changed")
            && host_request.contains("Research")
            && host_request.contains("Briefing")
    );
    let points = agent.snapshot(chat).await.unwrap().rewind_points();
    agent
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(agent.artifact_ticks(chat).await.unwrap(), ticks);
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            name: Some("artifact branch".into()),
        })
        .await
        .unwrap();
    assert_eq!(branch.snapshot.artifact_ticks, ticks);
    agent.flush_transcripts().await;
    agent.shutdown().await;
    let reopened =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    assert_eq!(
        reopened.resume_chat(chat).await.unwrap().artifact_ticks,
        ticks
    );
    assert_eq!(
        reopened
            .resume_chat(branch.child_chat_id)
            .await
            .unwrap()
            .artifact_ticks,
        ticks
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn artifact_only_chat_survives_restart_before_its_first_message() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let tick = tickloop_harness::protocol::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Before first prompt".into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1000,
    };
    agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
    agent.shutdown().await;
    let reopened =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    assert!(
        reopened
            .saved_chats(directory.path())
            .await
            .unwrap()
            .iter()
            .any(|saved| saved.id == chat)
    );
    let snapshot = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks, vec![tick]);
    assert_eq!(
        snapshot.folder,
        Some(directory.path().canonicalize().unwrap())
    );
    assert!(snapshot.messages.is_empty());
    reopened.shutdown().await;
}

#[tokio::test]
async fn artifact_tick_metadata_before_a_compaction_boundary_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer"), text("summary")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "a saved prompt").await;
    let tick = tickloop_harness::protocol::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Before boundary".into(),
        format: "html".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "editor".into(),
        added_at_ms: 1000,
    };
    agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
    agent.compact(chat).await.unwrap();
    agent.shutdown().await;
    let reopened =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    assert_eq!(
        reopened.resume_chat(chat).await.unwrap().artifact_ticks,
        vec![tick]
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn saved_chats_leave_subagents_to_their_family_and_a_subagent_record_names_its_root() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(tickloop_harness::test_support::routed::RoutedModel::new(
        |request| {
            if !tickloop_harness::test_support::routed::state(request)["delegation"].is_null() {
                return text("child report");
            }
            if !tickloop_harness::test_support::routed::has_tool_result(request, "delegate") {
                return call(
                    "delegate",
                    "Subagent",
                    json!({"task":"Inspect the module","name":"inspector"}),
                );
            }
            text("root done")
        },
    ));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "Delegate the inspection").await;
    let child = agent.subagents(chat).await.unwrap()[0].child_chat_id;
    agent.shutdown().await;

    let reopened = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::<Vec<_>>::new())),
    )
    .unwrap();
    let saved = reopened.saved_chats(directory.path()).await.unwrap();
    assert_eq!(saved.iter().map(|chat| chat.id).collect::<Vec<_>>(), [chat]);
    reopened.shutdown().await;
    let records = tickloop_harness::runtime::transcript::records::saved_records(
        root.clone(),
        directory.path().into(),
        0,
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        records.iter().map(|chat| chat.id).collect::<Vec<_>>(),
        [chat]
    );
    let family_root = |chat: ChatId| {
        tickloop_harness::runtime::transcript::records::saved_family_root(root.clone(), chat.uuid())
    };
    assert_eq!(family_root(child).await.unwrap(), Some(chat.uuid()));
    assert_eq!(family_root(chat).await.unwrap(), None);
}
