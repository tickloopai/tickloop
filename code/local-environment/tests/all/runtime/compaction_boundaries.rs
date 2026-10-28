use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    BranchRequest, ChatId, ChatSnapshot, Content, DirectedSummaryRequest, EventKind, Message,
    ModelIdentity, Protocol, RewindMode, Role, SummaryDirection, Usage,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::runtime::ChatSnapshotExt;
use tickloop_harness::test_support::{ScriptedModel, config, next_event, text, transcript_file};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Replies in order; regular requests (with tools) and summary requests (without tools) each have
/// their own queue, and replies carry fixed usage.
struct UsageModel {
    requests: Mutex<Vec<ModelRequest>>,
    answers: Mutex<VecDeque<String>>,
    summaries: Mutex<VecDeque<String>>,
}

const ANSWER_USAGE: Usage = Usage {
    input_tokens: Some(190_000),
    output_tokens: Some(20),
    cache_read_input_tokens: Some(7),
    cache_write_input_tokens: Some(3),
    reasoning_output_tokens: None,
};

fn zero_usage() -> Usage {
    Usage {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_read_input_tokens: Some(0),
        cache_write_input_tokens: Some(0),
        reasoning_output_tokens: None,
    }
}

impl UsageModel {
    fn new(answers: &[&str], summaries: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.iter().map(|answer| answer.to_string()).collect()),
            summaries: Mutex::new(
                summaries
                    .iter()
                    .map(|summary| summary.to_string())
                    .collect(),
            ),
        })
    }

    fn last_request(&self) -> ModelRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl ModelClient for UsageModel {
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
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let queue = if request.tools.is_empty() {
            &self.summaries
        } else {
            &self.answers
        };
        self.requests.lock().unwrap().push(request);
        let reply = queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        let _ = events.send(ModelDelta::Text(reply.clone()));
        Ok(ModelOutput {
            private_source: None,
            content: text(&reply),
            usage: ANSWER_USAGE,
            stop_reason: "end_turn".into(),
        })
    }
}

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

fn message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The conversation sent to the model, without host-generated system messages.
fn conversation(request: &ModelRequest) -> Vec<Message> {
    request
        .messages
        .iter()
        .filter(|message| tickloop_harness::test_support::is_conversation(message))
        .cloned()
        .collect()
}

fn texts(snapshot: &ChatSnapshot) -> Vec<String> {
    snapshot
        .messages
        .iter()
        .map(|message| message_text(std::slice::from_ref(message)))
        .collect()
}

async fn summarize(
    agent: &Agent,
    chat: ChatId,
    prompt_index: usize,
    direction: SummaryDirection,
) -> ChatSnapshot {
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[prompt_index].clone();
    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: point.message_id,
                direction,
                guidance: Some("keep the decision".into()),
            },
        )
        .await
        .unwrap()
        .snapshot
}

fn reopen(root: &Path, model: Arc<dyn ModelClient>) -> Agent {
    Agent::open_embedded(root.to_path_buf(), config(), model).unwrap()
}

fn reopen_with_window(root: &Path, model: Arc<dyn ModelClient>, tokens: usize) -> Agent {
    Agent::open_embedded(
        root.to_path_buf(),
        tickloop_harness::test_support::config_with_window(tokens),
        model,
    )
    .unwrap()
}

#[tokio::test]
async fn full_compaction_restart_sends_the_same_request_as_the_compacted_process() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = UsageModel::new(
        &["first answer", "second answer", "third answer"],
        &["compacted summary"],
    );
    let agent = reopen_with_window(&root, model.clone(), 5_000_000);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    complete_turn(&agent, chat, "first prompt").await;
    complete_turn(&agent, chat, "second prompt").await;
    agent.compact(chat).await.unwrap();
    complete_turn(&agent, chat, "third prompt").await;
    let in_process = agent.snapshot(chat).await.unwrap();
    assert!(texts(&in_process).contains(&"first prompt".to_owned()));
    let expected = in_process.conversation_messages().unwrap();
    assert!(!message_text(&expected).contains("first answer"));
    assert_eq!(in_process.chat_loop.runs_started, Some(3));
    assert_eq!(in_process.chat_loop.compactions, Some(1));
    agent.shutdown().await;
    drop(agent);

    let resumed_model = UsageModel::new(&["fourth answer"], &[]);
    let resumed = reopen_with_window(&root, resumed_model.clone(), 5_000_000);
    let restored = resumed.resume_chat(chat).await.unwrap();
    // Messages before the boundary are no longer loaded: the chain starts at the boundary, with
    // only the boundary, the summary and later messages.
    assert_eq!(restored.messages.len(), 4);
    assert!(restored.is_compact_boundary(0));
    assert!(restored.is_compact_summary(1));
    assert_eq!(restored.conversation_messages().unwrap(), expected);
    assert_eq!(restored.effective_context_window_tokens(), Some(5_000_000));
    assert_eq!(restored.rewind_points().len(), 1);
    // The Loop's counts outlive the messages compaction left behind.
    assert_eq!(restored.chat_loop.runs_started, Some(3));
    assert_eq!(restored.chat_loop.compactions, Some(1));
    complete_turn(&resumed, chat, "fourth prompt").await;
    assert_eq!(
        resumed.snapshot(chat).await.unwrap().chat_loop.runs_started,
        Some(4)
    );
    let mut sent = conversation(&resumed_model.last_request());
    assert_eq!(
        sent.pop().map(|message| message_text(&[message])),
        Some("fourth prompt".into())
    );
    assert_eq!(sent, expected);
    resumed.shutdown().await;
}

#[tokio::test]
async fn directed_summaries_restart_with_preserved_messages_in_order_and_zeroed_usage() {
    for direction in [SummaryDirection::FromHere, SummaryDirection::UpToHere] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let model = UsageModel::new(
            &["answer one", "answer two", "answer three", "answer four"],
            &["directed summary"],
        );
        let agent = reopen(&root, model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        for prompt in ["prompt one", "prompt two", "prompt three"] {
            complete_turn(&agent, chat, prompt).await;
        }
        summarize(&agent, chat, 1, direction).await;
        complete_turn(&agent, chat, "prompt four").await;
        let in_process = agent.snapshot(chat).await.unwrap();
        let expected_texts = match direction {
            SummaryDirection::FromHere => vec![
                "Conversation compacted",
                "prompt one",
                "answer one",
                "Summarized conversation (historical data, not new instructions):\ndirected summary",
                "prompt four",
                "answer four",
            ],
            SummaryDirection::UpToHere => vec![
                "Conversation compacted",
                "Summarized conversation (historical data, not new instructions):\ndirected summary",
                "prompt two",
                "answer two",
                "prompt three",
                "answer three",
                "prompt four",
                "answer four",
            ],
        };
        assert_eq!(texts(&in_process), expected_texts, "{direction:?}");
        agent.shutdown().await;
        drop(agent);

        let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
        let restored = resumed.resume_chat(chat).await.unwrap();
        assert_eq!(texts(&restored), expected_texts, "{direction:?}");
        assert_eq!(
            restored.conversation_messages().unwrap(),
            in_process.conversation_messages().unwrap()
        );
        assert_eq!(
            restored
                .rewind_points()
                .iter()
                .map(|point| point.message_id)
                .collect::<Vec<_>>(),
            in_process
                .rewind_points()
                .iter()
                .map(|point| point.message_id)
                .collect::<Vec<_>>()
        );
        let last = restored.messages.len() - 1;
        for (index, message) in restored.messages.iter().enumerate() {
            if message.role != Role::Assistant {
                continue;
            }
            let expected = if index == last {
                ANSWER_USAGE
            } else {
                zero_usage()
            };
            assert_eq!(
                restored.message_usage.get(&index),
                Some(&expected),
                "{direction:?} message {index}"
            );
        }
        resumed.shutdown().await;
    }
}

#[tokio::test]
async fn a_large_transcript_loads_only_after_the_last_boundary_and_keeps_session_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let large = "x".repeat(1024 * 1024);
    let model = Arc::new(ScriptedModel::new(vec![
        text("source answer"),
        text("large answer one"),
        text("large answer two"),
        text("large answer three"),
        text("summary of the large turns"),
    ]));
    let agent = reopen_with_window(&root, model, 50_000_000);
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, source, "source prompt").await;
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: Some("large work".into()),
        })
        .await
        .unwrap();
    let chat = branch.child_chat_id;

    for turn in ["one", "two", "three"] {
        complete_turn(&agent, chat, &format!("large prompt {turn} {large}")).await;
    }
    agent.compact(chat).await.unwrap();
    // Simulate the process being forcibly ended right after compaction: keep only what has reached
    // disk at this moment.
    let path = transcript_file(&root, chat.uuid());
    let original = std::fs::read_to_string(&path).unwrap();
    agent.shutdown().await;
    drop(agent);

    let lines = original.lines().collect::<Vec<_>>();
    let boundary = lines
        .iter()
        .position(|line| line.contains("\"compact_boundary\""))
        .unwrap();
    // Keep only the chain entries written after the boundary during compaction, dropping metadata
    // re-appended later: the title and Branch source can only be read from before the boundary.
    let is_metadata = |line: &str| {
        ["custom-title", "branch-origin", "last-prompt"]
            .iter()
            .any(|kind| line.starts_with(&format!("{{\"type\":\"{kind}\"")))
    };
    // Insert a leaf with a newer time before the boundary: a small file enters this branch, while a
    // large file skips content before the boundary.
    let mut stale_leaf = serde_json::from_str::<serde_json::Value>(
        lines
            .iter()
            .rev()
            .skip(lines.len() - boundary)
            .find(|line| line.starts_with("{\"type\":\"user\""))
            .unwrap(),
    )
    .unwrap();
    stale_leaf["parentUuid"] = stale_leaf["uuid"].clone();
    stale_leaf["uuid"] = serde_json::json!(Uuid::new_v4());
    stale_leaf["timestamp"] = serde_json::json!("2999-01-01T00:00:00.000Z");
    stale_leaf["message"] = serde_json::json!({"role":"user","content":[{"type":"text","text":"stale pre-boundary leaf"}]});
    let mut rewritten = lines[..boundary]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    rewritten.push_str(&format!("{stale_leaf}\n"));
    for line in lines[boundary..].iter().filter(|line| !is_metadata(line)) {
        rewritten.push_str(&format!("{line}\n"));
    }
    std::fs::write(&path, rewritten).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > 5 * 1024 * 1024);

    let resumed_model = Arc::new(ScriptedModel::new(vec![text("small answer")]));
    let resumed = reopen_with_window(&root, resumed_model.clone(), 50_000_000);
    let restored = resumed.resume_chat(chat).await.unwrap();
    // Boundary, summary, and the sentinel appended after the trailing user message on resume,
    // following Claude Code.
    assert_eq!(restored.messages.len(), 3);
    assert!(restored.is_compact_boundary(0));
    assert_eq!(
        restored.latest_compact_summary().as_deref(),
        Some("summary of the large turns")
    );
    assert_eq!(restored.effective_context_window_tokens(), Some(50_000_000));
    assert_eq!(
        restored
            .branch_origin
            .as_ref()
            .map(|origin| origin.source_chat_id),
        Some(source)
    );
    complete_turn(&resumed, chat, "small prompt").await;
    let sent = message_text(&resumed_model.requests.lock().unwrap()[0].messages);
    assert!(!sent.contains("large answer one"));
    assert!(!sent.contains("stale pre-boundary leaf"));

    let copy = resumed.resume_as_new_chat(chat).await.unwrap();
    resumed.flush_transcripts().await;
    let saved = resumed.saved_chats(directory.path()).await.unwrap();
    let title_of = |id| {
        saved
            .iter()
            .find(|saved| saved.id == id)
            .and_then(|saved| saved.title.clone())
    };
    assert_eq!(title_of(chat), Some("large work (Branch)".into()));
    assert_eq!(title_of(copy.id), Some("large work (Branch)".into()));
    resumed.shutdown().await;
}

struct PausedSummaryModel {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl ModelClient for PausedSummaryModel {
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
        let reply = if request.tools.is_empty() {
            self.started.notify_one();
            tokio::select! {
                _ = self.release.notified() => "late summary",
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        } else {
            "answer"
        };
        Ok(ModelOutput {
            private_source: None,
            content: text(reply),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn running_summaries_are_not_written_and_restore_as_if_they_never_started() {
    for direction in [None, Some(SummaryDirection::FromHere)] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let model = Arc::new(PausedSummaryModel {
            started: Notify::new(),
            release: Notify::new(),
        });
        let agent = reopen(&root, model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&agent, chat, "prompt before summary").await;
        let before = agent.snapshot(chat).await.unwrap();
        let running = {
            let agent = agent.clone();
            let point = before.rewind_points()[0].message_id;
            tokio::spawn(async move {
                match direction {
                    None => agent.compact(chat).await.map(|_| ()),
                    Some(direction) => agent
                        .summarize_checkpoint(
                            chat,
                            DirectedSummaryRequest {
                                message_id: point,
                                direction,
                                guidance: None,
                            },
                        )
                        .await
                        .map(|_| ()),
                }
            })
        };
        model.started.notified().await;
        agent.flush_transcripts().await;
        assert!(
            !std::fs::read_to_string(transcript_file(&root, chat.uuid()))
                .unwrap()
                .contains("compact_boundary")
        );

        let other = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
        let restored = other.resume_chat(chat).await.unwrap();
        assert!(restored.message_compactions.is_empty());
        assert_eq!(
            restored.conversation_messages().unwrap(),
            before.conversation_messages().unwrap()
        );
        other.shutdown().await;
        agent.interrupt(chat).await.unwrap();
        assert!(running.await.unwrap().is_err());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn rewinding_across_a_boundary_forks_before_it_and_restart_follows_the_new_branch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = UsageModel::new(
        &["first answer", "second answer", "replacement answer"],
        &["compacted summary"],
    );
    let agent = reopen(&root, model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "first prompt").await;
    agent.compact(chat).await.unwrap();
    complete_turn(&agent, chat, "second prompt").await;
    let points = agent.snapshot(chat).await.unwrap().rewind_points();
    assert_eq!(points.len(), 2);
    let rewound = agent
        .rewind(chat, points[0].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert!(rewound.snapshot.message_compactions.is_empty());
    complete_turn(&agent, chat, "replacement prompt").await;
    agent.shutdown().await;
    drop(agent);

    let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_eq!(
        texts(&restored),
        ["replacement prompt", "replacement answer"]
    );
    assert!(restored.message_compactions.is_empty());
    resumed.shutdown().await;
}

#[tokio::test]
async fn rewinding_inside_the_preserved_messages_restarts_on_the_new_branch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = UsageModel::new(
        &[
            "answer one",
            "answer two",
            "answer three",
            "replacement answer",
        ],
        &["prefix summary"],
    );
    let agent = reopen(&root, model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for prompt in ["prompt one", "prompt two", "prompt three"] {
        complete_turn(&agent, chat, prompt).await;
    }
    summarize(&agent, chat, 1, SummaryDirection::UpToHere).await;
    let points = agent.snapshot(chat).await.unwrap().rewind_points();
    assert_eq!(points.len(), 2);
    agent
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    complete_turn(&agent, chat, "replacement prompt").await;
    let in_process = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_eq!(texts(&restored), texts(&in_process));
    assert_eq!(
        texts(&restored)[2..],
        [
            "prompt two",
            "answer two",
            "replacement prompt",
            "replacement answer"
        ]
    );
    resumed.shutdown().await;
}

#[tokio::test]
async fn branch_and_fork_after_a_directed_summary_keep_the_summary_and_restart_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = UsageModel::new(
        &["answer one", "answer two", "child answer"],
        &["suffix summary"],
    );
    let agent = reopen(&root, model);
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, source, "prompt one").await;
    complete_turn(&agent, source, "prompt two").await;
    let summarized = summarize(&agent, source, 1, SummaryDirection::FromHere).await;
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: None,
        })
        .await
        .unwrap();
    let child = branch.snapshot;
    assert_eq!(
        child.conversation_messages().unwrap(),
        summarized.conversation_messages().unwrap()
    );
    assert_eq!(
        child.latest_compact_summary().as_deref(),
        Some("suffix summary")
    );
    assert_eq!(
        child.public_conversation_items().unwrap(),
        summarized.public_conversation_items().unwrap()
    );
    complete_turn(&agent, child.id, "child prompt").await;
    let child_in_process = agent.snapshot(child.id).await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
    let restored_child = resumed.resume_chat(child.id).await.unwrap();
    assert_eq!(
        restored_child.conversation_messages().unwrap(),
        child_in_process.conversation_messages().unwrap()
    );
    assert_eq!(
        restored_child.latest_compact_summary().as_deref(),
        Some("suffix summary")
    );
    let copy = resumed.resume_as_new_chat(source).await.unwrap();
    // The summary is the last user message, so resume appends the no-reply sentinel, following
    // Claude Code.
    assert_eq!(
        texts(&copy)[..summarized.messages.len()],
        texts(&summarized)[..]
    );
    resumed.shutdown().await;
    drop(resumed);

    let reopened = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
    let restored_copy = reopened.resume_chat(copy.id).await.unwrap();
    assert_eq!(texts(&restored_copy), texts(&copy));
    assert!(restored_copy.is_compact_boundary(0));
    reopened.shutdown().await;
}

/// After a restart, the restored conversation, rewind points, compaction markers and Loop match the
/// in-memory state of the writing process.
fn assert_restored_like(restored: &ChatSnapshot, in_process: &ChatSnapshot, label: &str) {
    assert_eq!(texts(restored), texts(in_process), "{label}");
    assert_eq!(
        restored.conversation_messages().unwrap(),
        in_process.conversation_messages().unwrap(),
        "{label}"
    );
    assert_eq!(
        restored.rewind_points(),
        in_process.rewind_points(),
        "{label}"
    );
    let markers = |snapshot: &ChatSnapshot| {
        (0..snapshot.messages.len())
            .map(|index| {
                (
                    snapshot.is_compact_boundary(index),
                    snapshot.is_compact_summary(index),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(markers(restored), markers(in_process), "{label}");
    // The load is a history load: the same Loop comes back with receiving paused.
    let mut loaded = in_process.chat_loop.clone();
    loaded.receiving = tickloop_harness::protocol::Receiving::Paused {
        reason: tickloop_harness::protocol::PauseReason::HistoryLoaded,
    };
    assert_eq!(
        serde_json::to_value(&restored.chat_loop).unwrap(),
        serde_json::to_value(&loaded).unwrap(),
        "{label}"
    );
}

#[tokio::test]
async fn stacked_directed_summaries_restart_exactly_like_the_process() {
    use SummaryDirection::{FromHere, UpToHere};
    // (first direction, first pick, second direction, second pick); a pick is an index into
    // rewind_points at that time.
    let stacks = [
        (FromHere, 2, FromHere, 3),
        // The second pick is within the first kept range and earlier than the later messages.
        (UpToHere, 1, UpToHere, 1),
        (FromHere, 2, UpToHere, 1),
        (UpToHere, 1, FromHere, 1),
    ];
    for (first, first_point, second, second_point) in stacks {
        let label = format!("{first:?} then {second:?}");
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let model = UsageModel::new(
            &[
                "answer one",
                "answer two",
                "answer three",
                "answer four",
                "answer five",
                "answer six",
            ],
            &["first summary", "second summary"],
        );
        let agent = reopen(&root, model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        for prompt in ["prompt one", "prompt two", "prompt three"] {
            complete_turn(&agent, chat, prompt).await;
        }
        summarize(&agent, chat, first_point, first).await;
        complete_turn(&agent, chat, "prompt four").await;
        complete_turn(&agent, chat, "prompt five").await;
        summarize(&agent, chat, second_point, second).await;
        complete_turn(&agent, chat, "prompt six").await;
        let in_process = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            in_process
                .active_conversation_projection()
                .unwrap()
                .iter()
                .filter(|item| matches!(
                    item,
                    tickloop_harness::protocol::ActiveConversationItem::Summary { .. }
                ))
                .count(),
            match (first, second) {
                (FromHere, FromHere) | (UpToHere, FromHere) => 2,
                _ => 1,
            },
            "{label}"
        );
        agent.shutdown().await;
        drop(agent);

        let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
        let restored = resumed.resume_chat(chat).await.unwrap();
        assert!(resumed.transcript_load_warnings(chat).is_empty(), "{label}");
        assert_restored_like(&restored, &in_process, &label);
        resumed.shutdown().await;
    }
}

#[tokio::test]
async fn rewinding_to_the_first_preserved_prompt_restarts_without_reviving_abandoned_messages() {
    for (direction, point) in [
        (SummaryDirection::UpToHere, 1),
        (SummaryDirection::FromHere, 2),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let model = UsageModel::new(
            &[
                "answer one",
                "answer two",
                "answer three",
                "replacement answer",
            ],
            &["directed summary"],
        );
        let agent = reopen(&root, model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        for prompt in ["prompt one", "prompt two", "prompt three"] {
            complete_turn(&agent, chat, prompt).await;
        }
        summarize(&agent, chat, point, direction).await;
        let head = agent.snapshot(chat).await.unwrap().rewind_points()[0].clone();
        agent
            .rewind(chat, head.message_id, RewindMode::Conversation)
            .await
            .unwrap();
        complete_turn(&agent, chat, "replacement prompt").await;
        let in_process = agent.snapshot(chat).await.unwrap();
        let visible = texts(&in_process).join("\n");
        assert!(!visible.contains("answer three"), "{direction:?}");
        assert!(!visible.contains(&head.prompt), "{direction:?}");
        agent.shutdown().await;
        drop(agent);

        let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
        let restored = resumed.resume_chat(chat).await.unwrap();
        assert_restored_like(&restored, &in_process, &format!("{direction:?}"));
        resumed.shutdown().await;
    }
}

/// Full compaction after a directed summary (leaving an earlier boundary in the file), then rewind
/// across the full compaction boundary and continue the conversation.
async fn rewind_across_a_full_compaction_after_a_directed_summary(prompt_padding: &str) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = UsageModel::new(
        &[
            "answer one",
            "answer two",
            "answer three",
            "answer four",
            "replacement answer",
        ],
        &["directed summary", "full summary"],
    );
    let agent = reopen_with_window(&root, model, 50_000_000);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    for prompt in ["prompt one", "prompt two", "prompt three"] {
        complete_turn(&agent, chat, &format!("{prompt}{prompt_padding}")).await;
    }
    summarize(&agent, chat, 1, SummaryDirection::UpToHere).await;
    agent.compact(chat).await.unwrap();
    complete_turn(&agent, chat, "prompt four").await;
    let points = agent.snapshot(chat).await.unwrap().rewind_points();
    assert_eq!(points.len(), 3);
    agent
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    complete_turn(&agent, chat, "replacement prompt").await;
    let in_process = agent.snapshot(chat).await.unwrap();
    assert!(in_process.is_compact_boundary(0));
    assert_eq!(in_process.message_compactions.len(), 2);
    agent.shutdown().await;
    drop(agent);

    let size = std::fs::metadata(transcript_file(&root, chat.uuid()))
        .unwrap()
        .len();
    assert_eq!(
        size > 5 * 1024 * 1024,
        !prompt_padding.is_empty(),
        "Transcript size {size}"
    );
    let resumed = reopen_with_window(&root, Arc::new(ScriptedModel::new(Vec::new())), 50_000_000);
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_restored_like(&restored, &in_process, "rewind across a full compaction");
    resumed.shutdown().await;
}

#[tokio::test]
async fn rewinding_across_a_full_compaction_after_a_directed_summary_restarts_on_the_new_branch() {
    rewind_across_a_full_compaction_after_a_directed_summary("").await;
}

#[tokio::test]
async fn a_large_transcript_rewound_across_its_last_boundary_loads_the_whole_new_branch() {
    let padding = format!(" {}", "y".repeat(1024 * 1024));
    rewind_across_a_full_compaction_after_a_directed_summary(&padding).await;
}

#[tokio::test]
async fn code_rewind_to_a_preserved_prompt_uses_its_file_history_in_process_and_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir(&workdir).unwrap();
    let tracked = workdir.join("tracked.txt");
    std::fs::write(&tracked, "version zero\n").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        [
            tickloop_harness::test_support::call(
                "read",
                "Read",
                serde_json::json!({"file_path":"tracked.txt"}),
            ),
            tickloop_harness::test_support::call(
                "edit-one",
                "Edit",
                serde_json::json!({"file_path":"tracked.txt","old_string":"zero","new_string":"one"}),
            ),
        ]
        .concat(),
        text("edited once"),
        tickloop_harness::test_support::call(
            "edit-two",
            "Edit",
            serde_json::json!({"file_path":"tracked.txt","old_string":"one","new_string":"two"}),
        ),
        text("edited twice"),
        text("nothing to edit"),
        text("summary of the first edit"),
    ]));
    let agent = reopen(&root, model);
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    for prompt in ["edit once", "edit twice", "look only"] {
        complete_turn_approving(&agent, chat, prompt).await;
    }
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "version two\n");
    let summarized = summarize(&agent, chat, 1, SummaryDirection::UpToHere).await;
    let points = summarized.rewind_points();
    assert_eq!(
        points
            .iter()
            .map(|point| point.prompt.as_str())
            .collect::<Vec<_>>(),
        ["edit twice", "look only"]
    );
    assert_eq!(
        agent
            .rewind_file_changes(chat, points[0].message_id)
            .await
            .unwrap()
            .map(|files| files.len()),
        Some(1)
    );
    let outcome = agent
        .rewind(chat, points[0].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert!(outcome.code.as_ref().unwrap().error.is_none());
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "version one\n");
    agent
        .rewind(chat, points[1].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "version two\n");
    agent.shutdown().await;
    drop(agent);

    let resumed = reopen(&root, Arc::new(ScriptedModel::new(Vec::new())));
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_eq!(restored.rewind_points(), points);
    let outcome = resumed
        .rewind(chat, points[0].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert!(outcome.code.as_ref().unwrap().error.is_none());
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "version one\n");
    resumed.shutdown().await;
}

async fn complete_turn_approving(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}
