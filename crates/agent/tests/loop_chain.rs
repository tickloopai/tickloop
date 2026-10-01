//! Loop 随对话链记录（rewind、Branch、压缩与重启后是否存在）与 Trigger 的跨进程租约。
//! 同一测试进程中的多个 Agent 实例模拟多个进程；锁文件接管、停止状态与子进程持锁见 `loop_trigger_safety.rs`。

mod support;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use support::{config, next_event};
use tickloop_agent::{
    model::{
        Content, Message, ModelClient, ModelDelta, ModelError, ModelIdentity, ModelOutput,
        ModelRequest, Protocol, Role, Usage,
    },
    runtime::{
        Agent, BranchRequest, ChatId, ChatSnapshot, DirectedSummaryRequest, EventKind, LoopId,
        LoopState, Receipt, RewindMode, SummaryDirection, Trigger,
    },
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

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

fn periodic(prompt: &str, interval_seconds: Option<u64>) -> Trigger {
    Trigger {
        id: Uuid::new_v4(),
        prompt: prompt.into(),
        interval_seconds,
        expires_at_ms: None,
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

fn find_loop(snapshot: &ChatSnapshot, loop_id: LoopId) -> &tickloop_agent::runtime::LoopSnapshot {
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
        .configure_trigger(
            chat,
            second.loop_id,
            Some(periodic("second check", Some(1))),
        )
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
    tokio::time::sleep(Duration::from_millis(1_600)).await;
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
        .configure_trigger(
            chat,
            watched.loop_id,
            Some(periodic("watch check", Some(3_600))),
        )
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
        let trigger = periodic("watch check", Some(3_600));
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
        assert_eq!(restored_trigger.interval_seconds, Some(3_600), "{label}");
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
async fn restarted_triggers_are_interrupted_or_expired_and_manual_resume_fires_on_schedule() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let scheduled = complete_turn(&agent, chat, None, "scheduled goal").await;
    agent
        .configure_trigger(
            chat,
            scheduled.loop_id,
            Some(periodic("scheduled check", Some(1))),
        )
        .await
        .unwrap();
    let deadline = complete_turn(&agent, chat, None, "deadline goal").await;
    agent
        .configure_trigger(
            chat,
            deadline.loop_id,
            Some(Trigger {
                expires_at_ms: Some(now_ms() + 1_000),
                ..periodic("deadline check", None)
            }),
        )
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    let model = EchoModel::new();
    let restarted = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let scheduled_loop = find_loop(&restored, scheduled.loop_id);
    assert_eq!(scheduled_loop.state, LoopState::Interrupted);
    assert!(!scheduled_loop.trigger.as_ref().unwrap().armed);
    assert_eq!(
        find_loop(&restored, deadline.loop_id).state,
        LoopState::Expired
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(model.requests.lock().unwrap().is_empty());

    complete_turn(
        &restarted,
        chat,
        Some(scheduled.loop_id),
        "resume the schedule",
    )
    .await;
    let resumed = restarted.snapshot(chat).await.unwrap();
    assert!(
        find_loop(&resumed, scheduled.loop_id)
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );
    wait_until(
        "the resumed schedule to fire",
        Duration::from_secs(3),
        || model.prompts_containing("scheduled check") >= 1,
    )
    .await;
    assert!(
        restarted
            .submit(chat, Some(deadline.loop_id), "resume the deadline".into())
            .await
            .is_err()
    );
    restarted.shutdown().await;
}

#[derive(Clone, Copy, Debug)]
enum HolderExit {
    Shutdown,
    Drop,
}

/// 在独立的 tokio 运行时中恢复 Chat、重新武装 Loop，第一次触发后通知 `ready`，收到 `release`
/// 后按 `exit` 退出；`Drop` 时直接释放 Agent 与运行时，模拟实例退出而未调用 shutdown。
fn spawn_lease_holder(
    root: PathBuf,
    chat: ChatId,
    loop_id: LoopId,
    model: Arc<EchoModel>,
    exit: HolderExit,
    ready: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let agent = Agent::open_embedded(root, config(), model.clone()).unwrap();
            agent.resume_chat(chat).await.unwrap();
            complete_turn(&agent, chat, Some(loop_id), "resume in the holder").await;
            wait_until("the holder to fire", WAIT_TIMEOUT, || {
                model.prompts_containing("shared check") >= 1
            })
            .await;
            ready.send(()).unwrap();
            release.await.unwrap();
            match exit {
                HolderExit::Shutdown => agent.shutdown().await,
                HolderExit::Drop => drop(agent),
            }
        });
        drop(runtime);
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn two_instances_fire_a_shared_loop_once_and_the_other_takes_over_after_the_holder_exits() {
    for exit in [HolderExit::Shutdown, HolderExit::Drop] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let creator = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
        let chat = creator.create_chat(directory.path().into()).await.unwrap();
        let watched = complete_turn(&creator, chat, None, "shared goal").await;
        creator
            .configure_trigger(
                chat,
                watched.loop_id,
                Some(periodic("shared check", Some(1))),
            )
            .await
            .unwrap();
        creator.shutdown().await;
        drop(creator);

        let holder_model = EchoModel::new();
        let (ready_sender, ready) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = tokio::sync::oneshot::channel();
        let holder = spawn_lease_holder(
            root.clone(),
            chat,
            watched.loop_id,
            holder_model.clone(),
            exit,
            ready_sender,
            release_receiver,
        );
        tokio::time::timeout(WAIT_TIMEOUT, ready)
            .await
            .expect("holder did not fire")
            .unwrap();

        let other_model = EchoModel::new();
        let other = Agent::open_embedded(root, config(), other_model.clone()).unwrap();
        other.resume_chat(chat).await.unwrap();
        complete_turn(&other, chat, Some(watched.loop_id), "resume in the other").await;
        let holder_fires = holder_model.prompts_containing("shared check");
        tokio::time::sleep(Duration::from_millis(3_200)).await;
        assert!(
            holder_model.prompts_containing("shared check") >= holder_fires + 2,
            "{exit:?}"
        );
        assert_eq!(
            other_model.prompts_containing("shared check"),
            0,
            "{exit:?}"
        );

        release.send(()).unwrap();
        let exited_at = Instant::now();
        tokio::task::spawn_blocking(move || holder.join().unwrap())
            .await
            .unwrap();
        wait_until(
            "the other instance to take over",
            Duration::from_secs(8),
            || other_model.prompts_containing("shared check") >= 1,
        )
        .await;
        // 拿不到租约时每 5 秒重试一次；接手后在下一个 1 秒间隔触发。
        assert!(
            exited_at.elapsed() < Duration::from_millis(6_500),
            "{exit:?}: took over after {:?}",
            exited_at.elapsed()
        );
        other.shutdown().await;
    }
}
