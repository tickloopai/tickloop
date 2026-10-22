//! Chat 的 Loop 随对话链记录：rewind、Branch、压缩与重启后保持同一个 Loop 与它的 Trigger。
//! 同一测试进程中的多个 Agent 实例模拟多个进程；Trigger 租约、锁文件接管、停止状态与子进程持锁见
//! `loop_trigger_safety.rs`。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    BranchRequest, ChatId, Content, DirectedSummaryRequest, EventKind, LoopState, Message,
    ModelIdentity, Protocol, Receipt, RewindMode, Role, SummaryDirection, Trigger,
    TriggerPromptSource, TriggerSource, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{config, next_event};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 每个请求都回复一段文字；不带工具的摘要请求回复摘要。
struct EchoModel {
    requests: Mutex<Vec<ModelRequest>>,
}

impl EchoModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }

    /// 最后一条消息包含 `needle` 的请求数，用于统计 Trigger 触发的运行。
    fn prompts_containing(&self, needle: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !request.tools.is_empty())
            .filter(|request| {
                request
                    .messages
                    .last()
                    .is_some_and(|message| message_text(message).contains(needle))
            })
            .count()
    }

    fn last_conversation_request(&self) -> ModelRequest {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|request| !request.tools.is_empty())
            .unwrap()
            .clone()
    }
}

#[async_trait]
impl ModelClient for EchoModel {
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
        let reply = if request.tools.is_empty() {
            "summary of the earlier conversation".to_owned()
        } else {
            format!("answered {}", request.messages.len())
        };
        self.requests.lock().unwrap().push(request);
        let _ = events.send(ModelDelta::Text(reply.clone()));
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text { text: reply }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn watching(prompt: &str) -> Trigger {
    Trigger {
        id: Uuid::new_v4(),
        prompt: prompt.into(),
        prompt_source: TriggerPromptSource::Explicit,
        accepts_events: true,
        armed: true,
        subscriptions: Vec::new(),
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

async fn wait_until(what: &str, timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn transcript_files(directory: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(transcript_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    files
}

/// A finished Loop's state time is written with its Loop record and read back on restart;
/// records written before the field existed restore the same Loop with the time unknown, never
/// the restart time.
#[tokio::test]
async fn loop_state_times_survive_restart_and_legacy_records_stay_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let started = now_ms();
    let finished = complete_turn(&agent, chat, "finish once").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = &snapshot.chat_loop;
    assert_eq!(goal.id, finished.loop_id);
    assert_eq!(goal.state, LoopState::Idle);
    let finished_at = goal.state_since.expect("a finish records its time");
    assert!(finished_at >= started && finished_at <= now_ms());
    agent.shutdown().await;
    drop(agent);

    let transcripts = transcript_files(&root);
    assert!(
        transcripts.iter().any(|path| std::fs::read_to_string(path)
            .unwrap()
            .contains(&format!("\"stateSince\":{finished_at}"))),
        "the Loop record carries the time on disk"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    let restarted = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, finished.loop_id);
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(
        restored.chat_loop.state_since,
        Some(finished_at),
        "restart keeps the original time"
    );
    restarted.shutdown().await;
    drop(restarted);

    for path in &transcripts {
        let legacy = std::fs::read_to_string(path)
            .unwrap()
            .replace(&format!(",\"stateSince\":{finished_at}"), "");
        std::fs::write(path, legacy).unwrap();
    }
    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(
        restored.chat_loop.state_since, None,
        "a legacy record leaves the time unknown"
    );
    reopened.shutdown().await;
}

/// A restart pauses an idle Loop's receiving without converting its state, so the Loop keeps
/// its recorded state time.
#[tokio::test]
async fn a_restart_pauses_an_idle_loop_and_keeps_its_state_time() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "watched goal").await;
    agent
        .configure_trigger(chat, Some(watching("watch check")))
        .await
        .unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(before.chat_loop.state, LoopState::Idle);
    let idle_at = before.chat_loop.state_since;
    assert!(idle_at.is_some());
    agent.shutdown().await;
    drop(agent);

    let restarted = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(restored.chat_loop.state_since, idle_at);
    assert!(!restored.chat_loop.trigger.as_ref().unwrap().armed);
    assert!(restored.trigger_awaits_resume);
    restarted.shutdown().await;
}

#[tokio::test]
async fn rewind_keeps_the_loop_and_its_trigger_across_restarts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = EchoModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, chat, "first goal").await;
    complete_turn(&agent, chat, "second goal").await;
    agent
        .configure_trigger(chat, Some(watching("second check")))
        .await
        .unwrap();
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    let outcome = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(outcome.draft.unwrap().prompt, "second goal");
    assert_eq!(outcome.snapshot.chat_loop.id, first.loop_id);
    assert!(outcome.snapshot.chat_loop.trigger.as_ref().unwrap().armed);
    agent
        .trigger(
            chat,
            TriggerSource {
                source: "manual".into(),
                payload: serde_json::json!({"note": "changed"}),
            },
        )
        .await
        .unwrap();
    wait_until(
        "the Trigger to deliver after the rewind",
        Duration::from_secs(3),
        || model.prompts_containing("second check") >= 1,
    )
    .await;
    let third = complete_turn(&agent, chat, "third goal").await;
    assert_eq!(third.loop_id, first.loop_id);
    agent.shutdown().await;
    drop(agent);

    let restarted_model = EchoModel::new();
    let restarted = Agent::open_embedded(root.clone(), config(), restarted_model.clone()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, first.loop_id);
    let third_point = restored
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "third goal")
        .unwrap();
    let outcome = restarted
        .rewind(chat, third_point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(outcome.snapshot.chat_loop.id, first.loop_id);
    complete_turn(&restarted, chat, "fourth goal").await;
    let request = restarted_model.last_conversation_request();
    let sent = request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(sent.first().map(String::as_str), Some("first goal"));
    assert_eq!(sent.last().map(String::as_str), Some("fourth goal"));
    assert!(!sent.iter().any(|text| text == "third goal"));
    restarted.shutdown().await;
    drop(restarted);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, first.loop_id);
    assert!(restored.chat_loop.trigger.is_some());
    reopened.shutdown().await;
}

#[tokio::test]
async fn branch_copies_the_loop_with_its_trigger_disarmed_until_continued() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let watched = complete_turn(&agent, chat, "watch goal").await;
    agent
        .configure_trigger(chat, Some(watching("watch check")))
        .await
        .unwrap();

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            name: None,
        })
        .await
        .unwrap();
    let child = outcome.snapshot;
    assert_eq!(child.chat_loop.id, watched.loop_id);
    assert!(!child.chat_loop.trigger.as_ref().unwrap().armed);
    assert_eq!(child.chat_loop.state, LoopState::Idle);
    assert!(child.trigger_awaits_resume);
    let source = agent.snapshot(chat).await.unwrap();
    assert!(source.chat_loop.trigger.as_ref().unwrap().armed);

    complete_turn(
        &agent,
        outcome.child_chat_id,
        "resume watching in the branch",
    )
    .await;
    let resumed = agent.snapshot(outcome.child_chat_id).await.unwrap();
    assert!(resumed.chat_loop.trigger.as_ref().unwrap().armed);
    assert_eq!(resumed.chat_loop.state, LoopState::Idle);
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(outcome.child_chat_id).await.unwrap();
    assert_eq!(restored.chat_loop.id, watched.loop_id);
    assert!(restored.trigger_awaits_resume);
    reopened.shutdown().await;
}

#[tokio::test]
async fn trigger_settings_survive_restart_after_compaction() {
    for directed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let watched = complete_turn(&agent, chat, "watch goal").await;
        let trigger = watching("watch check");
        agent
            .configure_trigger(chat, Some(trigger.clone()))
            .await
            .unwrap();
        complete_turn(&agent, chat, "other goal").await;
        complete_turn(&agent, chat, "third goal").await;
        if directed {
            let point = agent.snapshot(chat).await.unwrap().rewind_points()[2].clone();
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: point.message_id,
                        direction: SummaryDirection::UpToHere,
                        guidance: None,
                    },
                )
                .await
                .unwrap();
        } else {
            agent.compact(chat).await.unwrap();
        }
        let in_process = agent.snapshot(chat).await.unwrap();
        let in_process_conversation = in_process
            .conversation_messages()
            .unwrap()
            .into_iter()
            .filter(|message| message.role != Role::System)
            .collect::<Vec<_>>();
        agent.shutdown().await;
        drop(agent);

        let model = EchoModel::new();
        let restarted = Agent::open_embedded(root, config(), model.clone()).unwrap();
        let restored = restarted.resume_chat(chat).await.unwrap();
        let label = if directed { "directed" } else { "full" };
        assert_eq!(restored.chat_loop.id, watched.loop_id, "{label}");
        let restored_trigger = restored.chat_loop.trigger.as_ref().unwrap();
        assert_eq!(restored_trigger.id, trigger.id, "{label}");
        assert_eq!(restored_trigger.prompt, trigger.prompt, "{label}");
        assert!(restored_trigger.accepts_events, "{label}");
        assert!(!restored_trigger.armed, "{label}");
        assert_eq!(restored.chat_loop.state, LoopState::Idle, "{label}");
        assert!(restored.trigger_awaits_resume, "{label}");

        complete_turn(&restarted, chat, "resume watching").await;
        // Loop 条目不进入发给模型的对话：重启后的请求只多出恢复哨兵与新的提示。
        let sent = model
            .last_conversation_request()
            .messages
            .into_iter()
            .filter(|message| message.role != Role::System)
            .collect::<Vec<_>>();
        assert!(
            sent.iter().all(|message| !message.content.is_empty()),
            "{label}"
        );
        assert!(sent.starts_with(&in_process_conversation), "{label}");
        let extra = sent[in_process_conversation.len()..]
            .iter()
            .map(message_text)
            .collect::<Vec<_>>();
        assert!(
            extra == ["resume watching"] || extra == ["No response requested.", "resume watching"],
            "{label}: {extra:?}"
        );
        assert!(
            restarted
                .snapshot(chat)
                .await
                .unwrap()
                .chat_loop
                .trigger
                .as_ref()
                .unwrap()
                .armed,
            "{label}"
        );
        restarted.shutdown().await;
    }
}

#[tokio::test]
async fn restarted_triggers_are_paused_and_a_new_input_resumes_them() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "watched goal").await;
    agent
        .configure_trigger(chat, Some(watching("watch check")))
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let model = EchoModel::new();
    let restarted = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert!(!restored.chat_loop.trigger.as_ref().unwrap().armed);
    assert!(model.requests.lock().unwrap().is_empty());

    complete_turn(&restarted, chat, "resume watching").await;
    let resumed = restarted.snapshot(chat).await.unwrap();
    assert!(resumed.chat_loop.trigger.as_ref().unwrap().armed);
    restarted
        .trigger(
            chat,
            TriggerSource {
                source: "manual".into(),
                payload: serde_json::json!({"note": "changed"}),
            },
        )
        .await
        .unwrap();
    wait_until(
        "the resumed trigger to deliver",
        Duration::from_secs(3),
        || model.prompts_containing("watch check") >= 1,
    )
    .await;
    restarted.shutdown().await;
}

/// A Chat is created with its one Loop: idle, with nothing to receive. Every turn runs in it and
/// a restart restores the same Loop.
#[tokio::test]
async fn a_new_chat_has_one_loop_that_every_turn_and_restart_keep() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let created = agent.snapshot(chat).await.unwrap().chat_loop;
    assert_eq!(created.state, LoopState::Idle);
    assert!(created.run_id.is_none());
    assert!(created.trigger.is_none());
    assert!(created.event_sources.is_empty());
    let first = complete_turn(&agent, chat, "first task").await;
    let second = complete_turn(&agent, chat, "an unrelated task").await;
    assert_eq!(first.loop_id, created.id);
    assert_eq!(second.loop_id, created.id);
    agent.shutdown().await;
    drop(agent);

    let restarted = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, created.id);
    let third = complete_turn(&restarted, chat, "after the restart").await;
    assert_eq!(third.loop_id, created.id);
    restarted.shutdown().await;
}

/// An interrupted or failed turn is only how that turn ended: the next input continues the same
/// Loop.
#[tokio::test]
async fn interrupted_and_failed_turns_continue_with_the_next_input() {
    use tickloop_agent_test_support::{ScriptedModel, call, single_question, text};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![call(
        "ask",
        "AskUserQuestion",
        single_question("Which file?"),
    )]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let asked = agent.submit(chat, "ask me".into()).await.unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::QuestionAsked { .. }
        ) {
            break;
        }
    }
    agent.interrupt(chat).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == asked.run_id && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Interrupted
    );

    // The script is empty now, so this turn fails.
    let failed = agent.submit(chat, "try again".into()).await.unwrap();
    assert_eq!(failed.loop_id, asked.loop_id);
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == failed.run_id && matches!(event.kind, EventKind::Failed(_)) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Failed
    );

    model.enqueue(text("done at last"));
    let continued = complete_turn(&agent, chat, "go on").await;
    assert_eq!(continued.loop_id, asked.loop_id);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Idle
    );
    agent.shutdown().await;
}

/// Loop records written before a Chat had one Loop are dropped on load; the conversation they
/// sat in stays visible and the Chat goes on with a Loop of its own.
#[tokio::test]
async fn old_format_loop_records_are_dropped_and_the_history_stays_visible() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let before = complete_turn(&agent, chat, "OLD-FORMAT-PROMPT").await;
    agent
        .configure_trigger(chat, Some(watching("watch check")))
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    for path in transcript_files(&root) {
        let old = std::fs::read_to_string(&path).unwrap().replace(
            "\"subtype\":\"chat_loop\",\"loop\":{",
            "\"subtype\":\"loop_state\",\"loop\":{\"goal\":\"OLD-FORMAT-PROMPT\",",
        );
        std::fs::write(&path, old).unwrap();
    }

    let model = EchoModel::new();
    let reopened = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert!(
        restored
            .messages
            .iter()
            .any(|message| message_text(message).contains("OLD-FORMAT-PROMPT")),
        "the history stays visible"
    );
    assert_ne!(restored.chat_loop.id, before.loop_id);
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert!(restored.chat_loop.trigger.is_none());
    let continued = complete_turn(&reopened, chat, "go on").await;
    assert_eq!(continued.loop_id, restored.chat_loop.id);
    assert!(
        model
            .last_conversation_request()
            .messages
            .iter()
            .any(|message| message_text(message).contains("OLD-FORMAT-PROMPT")),
        "the continued turn still sees the earlier conversation"
    );
    reopened.shutdown().await;
}
