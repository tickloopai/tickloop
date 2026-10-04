//! 同一 Loop 只触发一次、停止即停止：跨实例的 Trigger 租约（flock）与停止状态文件。
//! 同一测试进程中的多个 Agent 实例模拟多个进程；需要真实进程退出时，以本测试可执行文件
//! 运行 `lease_holder_child_process` 作为子进程持有租约。

mod support;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use support::{config, next_event};
use tickloop_agent::{
    model::{
        Content, Message, ModelClient, ModelDelta, ModelError, ModelIdentity, ModelOutput,
        ModelRequest, Protocol, Usage,
    },
    runtime::{
        Agent, AgentError, ChatId, ChatSnapshot, EventKind, LoopId, LoopState,
        LoopTriggerNoticeKind, Receipt, RewindMode, Trigger, TriggerSource,
    },
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// 拿不到租约时 5 秒重试一次，再加 1 秒触发间隔与调度余量：超过它仍未触发即视为不再触发。
const TAKEOVER_WINDOW: Duration = Duration::from_millis(7_500);
const LEASE_HOLDER_ENV: &str = "TICKLOOP_TEST_LEASE_HOLDER";
const SHARED_PROMPT: &str = "shared check";

struct EchoModel {
    prompts: Mutex<Vec<String>>,
}

impl EchoModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn prompts_containing(&self, needle: &str) -> usize {
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .filter(|prompt| prompt.contains(needle))
            .count()
    }
}

#[async_trait]
impl ModelClient for EchoModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

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
        if !request.tools.is_empty()
            && let Some(last) = request.messages.last()
        {
            self.prompts.lock().unwrap().push(message_text(last));
        }
        let reply = format!("answered {}", request.messages.len());
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

fn event_source() -> TriggerSource {
    TriggerSource {
        source: "test".into(),
        payload: serde_json::json!({}),
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

fn find_loop(snapshot: &ChatSnapshot, loop_id: LoopId) -> &tickloop_agent::runtime::LoopSnapshot {
    snapshot
        .loops
        .iter()
        .find(|goal| goal.id == loop_id)
        .unwrap_or_else(|| panic!("Loop {loop_id} is missing"))
}

/// Chat 目录：与 Transcript 同级的 `<projectDir>/<chatId>/`。
fn chat_directory(root: &Path, chat: ChatId) -> PathBuf {
    std::fs::read_dir(root.join("projects"))
        .unwrap()
        .map(|project| project.unwrap().path().join(format!("{chat}.jsonl")))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("Transcript for Chat {chat} does not exist"))
        .with_extension("")
}

fn lock_path(root: &Path, chat: ChatId, loop_id: LoopId) -> PathBuf {
    chat_directory(root, chat)
        .join("loops")
        .join(format!("{loop_id}.lock"))
}

/// 已保存的 Chat：一个普通目标与一个带 1 秒 Trigger 的共享目标。
async fn saved_chat_with_shared_loop(root: &Path, workdir: &Path) -> (ChatId, Receipt, Receipt) {
    let creator = Agent::open_embedded(root.into(), config(), EchoModel::new()).unwrap();
    let chat = creator.create_chat(workdir.into()).await.unwrap();
    let first = complete_turn(&creator, chat, None, "first goal").await;
    let shared = complete_turn(&creator, chat, None, "shared goal").await;
    creator
        .configure_trigger(chat, shared.loop_id, Some(periodic(SHARED_PROMPT, Some(1))))
        .await
        .unwrap();
    creator.shutdown().await;
    (chat, first, shared)
}

struct ArmedInstances {
    _directory: tempfile::TempDir,
    root: PathBuf,
    chat: ChatId,
    loop_id: LoopId,
    holder: Agent,
    holder_model: Arc<EchoModel>,
    other: Agent,
    other_model: Arc<EchoModel>,
}

/// 两个实例恢复同一 Chat 并都武装共享 Loop；先武装的实例持有租约并已触发，另一个实例未触发。
async fn two_armed_instances() -> ArmedInstances {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, _, shared) = saved_chat_with_shared_loop(&root, directory.path()).await;

    let holder_model = EchoModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), holder_model.clone()).unwrap();
    holder.resume_chat(chat).await.unwrap();
    complete_turn(&holder, chat, Some(shared.loop_id), "resume in the holder").await;
    wait_until("the holder to fire", WAIT_TIMEOUT, || {
        holder_model.prompts_containing(SHARED_PROMPT) >= 1
    })
    .await;

    let other_model = EchoModel::new();
    let other = Agent::open_embedded(root.clone(), config(), other_model.clone()).unwrap();
    other.resume_chat(chat).await.unwrap();
    complete_turn(&other, chat, Some(shared.loop_id), "resume in the other").await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(other_model.prompts_containing(SHARED_PROMPT), 0);
    ArmedInstances {
        _directory: directory,
        root,
        chat,
        loop_id: shared.loop_id,
        holder,
        holder_model,
        other,
        other_model,
    }
}

#[derive(Clone, Copy, Debug)]
enum StopPath {
    Stop,
    Disarm,
    RewindRemoval,
}

async fn stop_in_the_holder(path: StopPath) {
    let instances = two_armed_instances().await;
    let ArmedInstances {
        chat,
        loop_id,
        holder,
        other,
        other_model,
        ..
    } = &instances;
    match path {
        StopPath::Stop => {
            holder.stop(*chat, *loop_id).await.unwrap();
        }
        StopPath::Disarm => {
            holder
                .configure_trigger(
                    *chat,
                    *loop_id,
                    Some(Trigger {
                        armed: false,
                        ..periodic(SHARED_PROMPT, Some(1))
                    }),
                )
                .await
                .unwrap();
        }
        StopPath::RewindRemoval => {
            let point = holder
                .snapshot(*chat)
                .await
                .unwrap()
                .rewind_points()
                .into_iter()
                .find(|point| point.prompt == "shared goal")
                .unwrap();
            let deadline = Instant::now() + WAIT_TIMEOUT;
            loop {
                match holder
                    .rewind(*chat, point.message_id, RewindMode::Conversation)
                    .await
                {
                    Ok(outcome) => {
                        assert!(
                            !outcome
                                .snapshot
                                .loops
                                .iter()
                                .any(|goal| goal.id == *loop_id)
                        );
                        break;
                    }
                    Err(error) => {
                        assert!(Instant::now() < deadline, "rewind stayed busy: {error}");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }
    tokio::time::sleep(TAKEOVER_WINDOW).await;
    assert_eq!(
        other_model.prompts_containing(SHARED_PROMPT),
        0,
        "{path:?}: the other instance fired after the holder stopped the Loop"
    );
    let synced = other.snapshot(*chat).await.unwrap();
    let goal = find_loop(&synced, *loop_id);
    match path {
        StopPath::Stop | StopPath::RewindRemoval => assert_eq!(goal.state, LoopState::Stopped),
        StopPath::Disarm => assert!(!goal.trigger.as_ref().unwrap().armed),
    }
    holder.shutdown().await;
    other.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_loop_in_the_lease_holder_keeps_the_other_instance_from_firing() {
    stop_in_the_holder(StopPath::Stop).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn disarming_a_trigger_in_the_lease_holder_keeps_the_other_instance_from_firing() {
    stop_in_the_holder(StopPath::Disarm).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rewinding_a_loop_away_in_the_lease_holder_keeps_the_other_instance_from_firing() {
    stop_in_the_holder(StopPath::RewindRemoval).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_loop_in_the_waiting_instance_stops_the_lease_holder() {
    let instances = two_armed_instances().await;
    let mut holder_events = instances.holder.subscribe();
    instances
        .other
        .stop(instances.chat, instances.loop_id)
        .await
        .unwrap();
    // 持有者下一次到点时读到停止状态：同步为已停止、归还租约且不触发。
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let holder_fires = instances.holder_model.prompts_containing(SHARED_PROMPT);
    let synced = instances.holder.snapshot(instances.chat).await.unwrap();
    assert_eq!(
        find_loop(&synced, instances.loop_id).state,
        LoopState::Stopped
    );
    let mut ended = false;
    while let Ok(event) = holder_events.try_recv() {
        ended |=
            event.loop_id == instances.loop_id && matches!(event.kind, EventKind::GoalEnded { .. });
    }
    assert!(ended, "the holder reports the Loop as ended");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(
        instances.holder_model.prompts_containing(SHARED_PROMPT),
        holder_fires
    );
    assert!(!lock_path(&instances.root, instances.chat, instances.loop_id).exists());
    instances.holder.shutdown().await;
    instances.other.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_triggers_leave_exactly_one_holder_and_keep_its_lock() {
    const INSTANCES: usize = 4;
    const LOOPS: usize = 12;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let creator = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = creator.create_chat(directory.path().into()).await.unwrap();
    let mut loops = Vec::new();
    for index in 0..LOOPS {
        loops.push(
            complete_turn(&creator, chat, None, &format!("goal {index}"))
                .await
                .loop_id,
        );
    }
    creator.shutdown().await;
    drop(creator);

    let mut instances = Vec::new();
    for _ in 0..INSTANCES {
        let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
        agent.resume_chat(chat).await.unwrap();
        instances.push(agent);
    }
    // 全部恢复之后再武装：先武装的实例写出的 Trigger 会被之后恢复的实例按恢复规则解除武装。
    for agent in &instances {
        for loop_id in &loops {
            agent
                .configure_trigger(chat, *loop_id, Some(periodic("event check", None)))
                .await
                .unwrap();
        }
    }
    for loop_id in loops {
        let barrier = Arc::new(tokio::sync::Barrier::new(INSTANCES));
        let attempts = instances
            .iter()
            .cloned()
            .map(|agent| {
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    agent.trigger(chat, loop_id, event_source()).await
                })
            })
            .collect::<Vec<_>>();
        let mut delivered = 0;
        for attempt in attempts {
            match attempt.await.unwrap() {
                Ok(_) => delivered += 1,
                Err(AgentError::Invalid(message)) => {
                    assert!(message.contains("another Tick Loop process"), "{message}")
                }
                Err(error) => panic!("unexpected trigger failure: {error}"),
            }
        }
        assert_eq!(
            delivered, 1,
            "Loop {loop_id} was delivered {delivered} times"
        );
        assert!(
            lock_path(&root, chat, loop_id).exists(),
            "a failed attempt removed the holder's lock for Loop {loop_id}"
        );
    }
    for agent in instances {
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn a_stopped_loop_stays_stopped_after_continuing_a_restored_chat() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let stopped = complete_turn(&agent, chat, None, "goal to stop").await;
    agent
        .configure_trigger(
            chat,
            stopped.loop_id,
            Some(periodic("stopped check", Some(60))),
        )
        .await
        .unwrap();
    // 先写出武装状态，停止写成对话叶子之后的尾随 Loop 条目。
    agent.flush_transcripts().await;
    agent.stop(chat, stopped.loop_id).await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let restarted = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(
        find_loop(&restored, stopped.loop_id).state,
        LoopState::Stopped
    );
    complete_turn(&restarted, chat, None, "continue after restart").await;
    restarted.shutdown().await;
    drop(restarted);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(
        find_loop(&restored, stopped.loop_id).state,
        LoopState::Stopped
    );
    assert!(
        reopened
            .submit(chat, Some(stopped.loop_id), "revive".into())
            .await
            .is_err()
    );
    reopened.shutdown().await;
}

#[derive(serde::Deserialize, serde::Serialize)]
struct LeaseHolderPlan {
    root: PathBuf,
    chat: ChatId,
    loop_id: LoopId,
    ready: PathBuf,
}

/// 子进程入口：未设置 `TICKLOOP_TEST_LEASE_HOLDER` 时什么也不做。设置时恢复 Chat、重新武装共享
/// Loop，第一次触发后写出就绪文件，之后一直持有租约直到被杀死。
#[tokio::test(flavor = "multi_thread")]
async fn lease_holder_child_process() {
    let Ok(plan) = std::env::var(LEASE_HOLDER_ENV) else {
        return;
    };
    let plan: LeaseHolderPlan = serde_json::from_str(&plan).unwrap();
    let model = EchoModel::new();
    let agent = Agent::open_embedded(plan.root, config(), model.clone()).unwrap();
    agent.resume_chat(plan.chat).await.unwrap();
    complete_turn(&agent, plan.chat, Some(plan.loop_id), "resume in the child").await;
    wait_until("the child to fire", WAIT_TIMEOUT, || {
        model.prompts_containing(SHARED_PROMPT) >= 1
    })
    .await;
    std::fs::write(&plan.ready, std::process::id().to_string()).unwrap();
    tokio::time::sleep(Duration::from_secs(120)).await;
}

fn lock_holder_pid(path: &Path) -> Option<u64> {
    let content = std::fs::read(path).ok()?;
    serde_json::from_slice::<serde_json::Value>(&content).ok()?["pid"].as_u64()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_holder_process_releases_its_lease_to_the_waiting_instance() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, _, shared) = saved_chat_with_shared_loop(&root, directory.path()).await;
    let ready = directory.path().join("holder-ready");
    let plan = LeaseHolderPlan {
        root: root.clone(),
        chat,
        loop_id: shared.loop_id,
        ready: ready.clone(),
    };
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_holder_child_process", "--nocapture"])
        .env(LEASE_HOLDER_ENV, serde_json::to_string(&plan).unwrap())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        "the child process to hold the lease",
        Duration::from_secs(60),
        || ready.exists(),
    )
    .await;
    let lock = lock_path(&root, chat, shared.loop_id);
    assert_eq!(lock_holder_pid(&lock), Some(u64::from(child.id())));

    let model = EchoModel::new();
    let other = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    other.resume_chat(chat).await.unwrap();
    complete_turn(&other, chat, Some(shared.loop_id), "resume in the other").await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(model.prompts_containing(SHARED_PROMPT), 0);
    assert!(
        other
            .loop_trigger_notices()
            .iter()
            .any(|notice| notice.loop_id == shared.loop_id
                && notice.kind == LoopTriggerNoticeKind::HeldByAnotherProcess)
    );

    // SIGKILL：子进程来不及归还租约，锁文件留在原处，由内核释放 flock。
    child.kill().unwrap();
    child.wait().unwrap();
    let killed_at = Instant::now();
    assert!(lock.exists());
    wait_until("the waiting instance to take over", TAKEOVER_WINDOW, || {
        model.prompts_containing(SHARED_PROMPT) >= 1
    })
    .await;
    assert!(killed_at.elapsed() < TAKEOVER_WINDOW);
    assert_eq!(lock_holder_pid(&lock), Some(u64::from(std::process::id())));
    assert!(other.loop_trigger_notices().is_empty());
    other.shutdown().await;
}

fn write_lock(root: &Path, chat: ChatId, loop_id: LoopId, content: &[u8]) {
    let path = lock_path(root, chat, loop_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_lock_files_are_taken_over_whatever_pid_they_name_but_not_from_another_host() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = EchoModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let names = ["corrupt", "exited", "reused", "own", "foreign"];
    let mut loops = Vec::new();
    for name in names {
        loops.push(
            complete_turn(&agent, chat, None, &format!("{name} goal"))
                .await
                .loop_id,
        );
    }
    agent.flush_transcripts().await;

    let mut exited = std::process::Command::new("true").spawn().unwrap();
    let exited_pid = exited.id();
    exited.wait().unwrap();
    // 仍在运行但与 Tick Loop 无关的进程：模拟锁文件中的 PID 已被复用。
    let mut unrelated = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let holder = |pid: u32| {
        serde_json::json!({"chatId": chat, "pid": pid, "acquiredAt": 1})
            .to_string()
            .into_bytes()
    };
    write_lock(&root, chat, loops[0], b"{\"chatId\":");
    write_lock(&root, chat, loops[1], &holder(exited_pid));
    write_lock(&root, chat, loops[2], &holder(unrelated.id()));
    write_lock(&root, chat, loops[3], &holder(std::process::id()));
    write_lock(
        &root,
        chat,
        loops[4],
        serde_json::json!({
            "chatId": chat,
            "pid": std::process::id(),
            "nonce": Uuid::new_v4(),
            "startedAt": 1,
            "host": "another-host.invalid",
        })
        .to_string()
        .as_bytes(),
    );

    for (loop_id, name) in loops.iter().zip(names) {
        agent
            .configure_trigger(
                chat,
                *loop_id,
                Some(periodic(&format!("{name} check"), Some(1))),
            )
            .await
            .unwrap();
    }
    wait_until(
        "stale locks to be taken over",
        Duration::from_secs(4),
        || {
            ["corrupt", "exited", "reused", "own"]
                .iter()
                .all(|name| model.prompts_containing(&format!("{name} check")) >= 1)
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(model.prompts_containing("foreign check"), 0);
    let notices = agent.loop_trigger_notices();
    let foreign = notices
        .iter()
        .find(|notice| notice.loop_id == loops[4])
        .expect("the foreign host lock is reported");
    assert_eq!(foreign.kind, LoopTriggerNoticeKind::Warning);
    assert!(
        foreign.message.contains("another-host.invalid"),
        "{}",
        foreign.message
    );
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    agent.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_lease_directory_fails_the_trigger_with_a_visible_warning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let goal = complete_turn(&agent, chat, None, "event goal").await;
    agent.flush_transcripts().await;
    let chat_directory = chat_directory(&root, chat);
    std::fs::create_dir_all(&chat_directory).unwrap();
    std::fs::write(chat_directory.join("loops"), b"not a directory").unwrap();

    agent
        .configure_trigger(chat, goal.loop_id, Some(periodic("event check", None)))
        .await
        .unwrap();
    let error = agent
        .trigger(chat, goal.loop_id, event_source())
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::Storage(_)), "{error}");
    let notices = agent.loop_trigger_notices();
    assert!(
        notices.iter().any(|notice| notice.chat_id == chat
            && notice.loop_id == goal.loop_id
            && notice.kind == LoopTriggerNoticeKind::Warning),
        "{:?}",
        notices
            .iter()
            .map(|notice| &notice.message)
            .collect::<Vec<_>>()
    );
    agent.shutdown().await;
}
