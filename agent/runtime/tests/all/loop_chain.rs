//! Loop 随对话链记录（rewind、Branch、压缩与重启后是否存在）。
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
    BranchRequest, ChatId, ChatSnapshot, Content, DirectedSummaryRequest, EventKind, LoopId,
    LoopState, Message, ModelIdentity, Protocol, Receipt, RewindMode, Role, SummaryDirection,
    Trigger, TriggerPromptSource, TriggerSource, Usage,
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

async fn wait_until(what: &str, timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn loop_ids(snapshot: &ChatSnapshot) -> Vec<LoopId> {
    snapshot.loops.iter().map(|goal| goal.id).collect()
}

fn find_loop(snapshot: &ChatSnapshot, loop_id: LoopId) -> &tickloop_agent_protocol::LoopSnapshot {
    snapshot
        .loops
        .iter()
        .find(|goal| goal.id == loop_id)
        .unwrap_or_else(|| panic!("Loop {loop_id} is missing"))
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
    let finished = complete_turn(&agent, chat, None, "finish once").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = find_loop(&snapshot, finished.loop_id);
    assert_eq!(goal.state, LoopState::Completed);
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
    let goal = find_loop(&restored, finished.loop_id);
    assert_eq!(goal.state, LoopState::Completed);
    assert_eq!(
        goal.state_since,
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
    let goal = find_loop(&restored, finished.loop_id);
    assert_eq!(goal.state, LoopState::Completed);
    assert_eq!(
        goal.state_since, None,
        "a legacy record leaves the time unknown"
    );
    reopened.shutdown().await;
}

/// Restoring converts a Loop that was waiting or running before the restart into Interrupted or
/// Expired. Nobody knows when that happened, so the converted Loop's time becomes unknown; a Loop
/// whose persisted state survives unchanged keeps its recorded time.
#[tokio::test]
async fn recovery_conversions_leave_their_state_time_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let kept = complete_turn(&agent, chat, None, "kept goal").await;
    let watched = complete_turn(&agent, chat, None, "watched goal").await;
    agent
        .configure_trigger(chat, watched.loop_id, Some(watching("watch check")))
        .await
        .unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    let kept_at = find_loop(&before, kept.loop_id).state_since;
    assert_eq!(find_loop(&before, kept.loop_id).state, LoopState::Completed);
    assert!(kept_at.is_some());
    let waiting = find_loop(&before, watched.loop_id);
    assert_eq!(waiting.state, LoopState::WaitingTrigger);
    assert!(
        waiting.state_since.is_some(),
        "the waiting state has a known time"
    );
    agent.shutdown().await;
    drop(agent);

    let restarted = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let interrupted = find_loop(&restored, watched.loop_id);
    assert_eq!(interrupted.state, LoopState::Interrupted);
    assert_eq!(
        interrupted.state_since, None,
        "a recovery conversion does not reuse the waiting state's time"
    );
    let unchanged = find_loop(&restored, kept.loop_id);
    assert_eq!(unchanged.state, LoopState::Completed);
    assert_eq!(
        unchanged.state_since, kept_at,
        "an unchanged persisted state keeps its recorded time"
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn rewinding_before_a_loop_start_removes_it_and_earlier_loops_stay_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = EchoModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, chat, None, "first goal").await;
    let second = complete_turn(&agent, chat, None, "second goal").await;
    agent
        .configure_trigger(chat, second.loop_id, Some(watching("second check")))
        .await
        .unwrap();
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    let outcome = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let draft = outcome.draft.unwrap().loop_id;
    assert_eq!(loop_ids(&outcome.snapshot), vec![first.loop_id, draft]);
    // 被移除的 Loop 的 Trigger 随截断停止。
    assert!(
        agent
            .trigger(
                chat,
                second.loop_id,
                TriggerSource {
                    source: "manual".into(),
                    payload: serde_json::json!({"note": "changed"}),
                },
            )
            .await
            .is_err()
    );
    assert_eq!(model.prompts_containing("second check"), 0);
    let third = complete_turn(&agent, chat, None, "third goal").await;
    agent.shutdown().await;
    drop(agent);

    let restarted_model = EchoModel::new();
    let restarted = Agent::open_embedded(root.clone(), config(), restarted_model.clone()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(
        loop_ids(&restored),
        vec![first.loop_id, draft, third.loop_id]
    );
    // 重启后 rewind 到第三个目标的提示：之后创建的 Loop 移除，更早的 Loop 保留。
    let third_point = restored.rewind_points()[1].clone();
    assert_eq!(third_point.prompt, "third goal");
    let outcome = restarted
        .rewind(chat, third_point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let second_draft = outcome.draft.unwrap().loop_id;
    assert_eq!(
        loop_ids(&outcome.snapshot),
        vec![first.loop_id, second_draft]
    );
    let fourth = complete_turn(&restarted, chat, None, "fourth goal").await;
    let request = restarted_model.last_conversation_request();
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|message| message.role != Role::System)
            .map(message_text)
            .collect::<Vec<_>>(),
        vec!["first goal", "answered 2", "fourth goal"]
    );
    restarted.shutdown().await;
    drop(restarted);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(
        loop_ids(&restored),
        vec![first.loop_id, second_draft, fourth.loop_id]
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn branch_copies_loops_started_before_it_with_disarmed_triggers() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let watched = complete_turn(&agent, chat, None, "watch goal").await;
    agent
        .configure_trigger(chat, watched.loop_id, Some(watching("watch check")))
        .await
        .unwrap();
    complete_turn(&agent, chat, None, "abandoned goal").await;
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    let draft = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap()
        .draft
        .unwrap()
        .loop_id;

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap();
    let child = outcome.snapshot;
    assert_eq!(loop_ids(&child), vec![watched.loop_id, draft]);
    assert!(
        child
            .loops
            .iter()
            .all(|goal| { goal.trigger.as_ref().is_none_or(|trigger| !trigger.armed) })
    );
    assert_eq!(
        find_loop(&child, watched.loop_id).state,
        LoopState::Interrupted
    );
    assert!(child.resume_triggers.contains(&watched.loop_id));
    let source = agent.snapshot(chat).await.unwrap();
    assert!(
        find_loop(&source, watched.loop_id)
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );

    complete_turn(
        &agent,
        outcome.child_chat_id,
        Some(watched.loop_id),
        "resume watching in the branch",
    )
    .await;
    let resumed = agent.snapshot(outcome.child_chat_id).await.unwrap();
    let resumed_loop = find_loop(&resumed, watched.loop_id);
    assert!(resumed_loop.trigger.as_ref().unwrap().armed);
    assert_eq!(resumed_loop.state, LoopState::WaitingTrigger);
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(outcome.child_chat_id).await.unwrap();
    assert_eq!(loop_ids(&restored), vec![watched.loop_id, draft]);
    assert!(restored.resume_triggers.contains(&watched.loop_id));
    reopened.shutdown().await;
}

#[tokio::test]
async fn unfinished_loops_and_trigger_settings_survive_restart_after_compaction() {
    for directed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let watched = complete_turn(&agent, chat, None, "watch goal").await;
        let trigger = watching("watch check");
        agent
            .configure_trigger(chat, watched.loop_id, Some(trigger.clone()))
            .await
            .unwrap();
        let other = complete_turn(&agent, chat, None, "other goal").await;
        let third = complete_turn(&agent, chat, None, "third goal").await;
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
        assert_eq!(
            loop_ids(&restored),
            vec![watched.loop_id, other.loop_id, third.loop_id],
            "{label}"
        );
        let restored_watch = find_loop(&restored, watched.loop_id);
        let restored_trigger = restored_watch.trigger.as_ref().unwrap();
        assert_eq!(restored_trigger.id, trigger.id, "{label}");
        assert_eq!(restored_trigger.prompt, trigger.prompt, "{label}");
        assert!(restored_trigger.accepts_events, "{label}");
        assert!(!restored_trigger.armed, "{label}");
        assert_eq!(restored_watch.state, LoopState::Interrupted, "{label}");
        assert!(
            restored.resume_triggers.contains(&watched.loop_id),
            "{label}"
        );
        assert_eq!(
            find_loop(&restored, other.loop_id).state,
            LoopState::Completed,
            "{label}"
        );

        complete_turn(&restarted, chat, Some(watched.loop_id), "resume watching").await;
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
            find_loop(&restarted.snapshot(chat).await.unwrap(), watched.loop_id)
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
async fn restarted_triggers_are_interrupted_and_manual_resume_takes_events_again() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let watched = complete_turn(&agent, chat, None, "watched goal").await;
    agent
        .configure_trigger(chat, watched.loop_id, Some(watching("watch check")))
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let model = EchoModel::new();
    let restarted = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let watched_loop = find_loop(&restored, watched.loop_id);
    assert_eq!(watched_loop.state, LoopState::Interrupted);
    assert!(!watched_loop.trigger.as_ref().unwrap().armed);
    assert!(model.requests.lock().unwrap().is_empty());

    complete_turn(&restarted, chat, Some(watched.loop_id), "resume watching").await;
    let resumed = restarted.snapshot(chat).await.unwrap();
    assert!(
        find_loop(&resumed, watched.loop_id)
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );
    restarted
        .trigger(
            chat,
            watched.loop_id,
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
