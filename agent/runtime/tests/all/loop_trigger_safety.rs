//! 同一 Chat 只送达一次、停止即停止：跨实例的 Trigger 租约（flock）与接收状态文件。
//! 同一测试进程中的多个 Agent 实例模拟多个进程，每个实例在自己的内存 Tick 流上观察同一个 Shell
//! Tick；需要真实进程退出时，以本测试可执行文件运行 `lease_holder_child_process` 作为子进程持有
//! 租约。

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ChatId, Content, EventKind, LoopState, LoopTriggerNoticeKind, Message, ModelIdentity,
    PauseReason, Protocol, Receipt, Receiving, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_runtime::tick_stream::InMemoryTickStream;
use tickloop_agent_test_support::shell_ticks::ShellTick;
use tickloop_agent_test_support::{config, next_event};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longer than a held delivery's retry, so a delivery the lease keeps out would have arrived.
const SETTLE: Duration = Duration::from_secs(1);
const LEASE_HOLDER_ENV: &str = "TICKLOOP_TEST_LEASE_HOLDER";
const SHARED_NOTE: &str = "shared check";
/// The delivery of the update a waiting instance holds until it takes the lease over; held, its
/// data is read again, which an in-memory Tick cannot, so only its sequence shows.
const SECOND_UPDATE: &str = "\"seq\":\"2\"";
const STOPPED: Receiving = Receiving::Paused {
    reason: PauseReason::Stopped,
};

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

/// Chat 目录：与 Transcript 同级的 `<projectDir>/<chatId>/`。
fn chat_directory(root: &Path, chat: ChatId) -> PathBuf {
    std::fs::read_dir(root.join("projects"))
        .unwrap()
        .map(|project| project.unwrap().path().join(format!("{chat}.jsonl")))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("Transcript for Chat {chat} does not exist"))
        .with_extension("")
}

fn lock_path(root: &Path, chat: ChatId) -> PathBuf {
    chat_directory(root, chat)
        .join("trigger")
        .join("lease.lock")
}

/// 已保存的 Chat，它的 Loop 订阅 `task` 的 Shell Tick。
async fn saved_chat_with_shared_loop(root: &Path, workdir: &Path, task: Uuid) -> ChatId {
    let creator = Agent::open_embedded(root.into(), config(), EchoModel::new()).unwrap();
    let shell = ShellTick::new(task);
    shell.install(&creator).await;
    let chat = creator.create_chat(workdir.into()).await.unwrap();
    complete_turn(&creator, chat, "shared goal").await;
    shell.subscribe(&creator, chat).await;
    creator.shutdown().await;
    chat
}

/// 一个实例：恢复共享 Chat，以一条用户输入继续它，并在自己的流上观察那个 Shell Tick。
struct Instance {
    agent: Agent,
    model: Arc<EchoModel>,
    shell: ShellTick,
}

async fn resumed_instance(root: &Path, chat: ChatId, task: Uuid, prompt: &str) -> Instance {
    let model = EchoModel::new();
    let agent = Agent::open_embedded(root.into(), config(), model.clone()).unwrap();
    let shell = ShellTick::new(task);
    shell.install(&agent).await;
    agent.resume_chat(chat).await.unwrap();
    complete_turn(&agent, chat, prompt).await;
    shell.wait_until_live(&agent, chat).await;
    Instance {
        agent,
        model,
        shell,
    }
}

async fn wait_for_notice(agent: &Agent, chat: ChatId, kind: LoopTriggerNoticeKind) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !agent
        .loop_trigger_notices()
        .iter()
        .any(|notice| notice.chat_id == chat && notice.kind == kind)
    {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a {kind:?} notice"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_receiving(agent: &Agent, chat: ChatId, expected: Receiving) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while agent.snapshot(chat).await.unwrap().chat_loop.receiving != expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct SharedInstances {
    _directory: tempfile::TempDir,
    root: PathBuf,
    chat: ChatId,
    holder: Instance,
    other: Instance,
}

/// 两个实例恢复同一 Chat；先送达的实例持有租约，另一个实例的送达留在它的订阅里。
async fn two_receiving_instances() -> SharedInstances {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let task = Uuid::new_v4();
    let chat = saved_chat_with_shared_loop(&root, directory.path(), task).await;

    let holder = resumed_instance(&root, chat, task, "resume in the holder").await;
    holder.shell.update(1, SHARED_NOTE);
    wait_until("the holder to deliver", WAIT_TIMEOUT, || {
        holder.model.prompts_containing(SHARED_NOTE) >= 1
    })
    .await;

    // A later update: the holder's watermark keeps the one it delivered from any instance.
    let other = resumed_instance(&root, chat, task, "resume in the other").await;
    other.shell.update(2, SHARED_NOTE);
    wait_for_notice(
        &other.agent,
        chat,
        LoopTriggerNoticeKind::HeldByAnotherProcess,
    )
    .await;
    assert_eq!(other.model.prompts_containing(SHARED_NOTE), 0);
    SharedInstances {
        _directory: directory,
        root,
        chat,
        holder,
        other,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_chat_in_the_lease_holder_keeps_the_other_instance_from_delivering() {
    // Bound whole: a field left out of the pattern would drop the Chat's directory.
    let SharedInstances {
        _directory,
        chat,
        holder,
        other,
        ..
    } = two_receiving_instances().await;
    holder.agent.stop(chat).await.unwrap();
    other.shell.update(3, SHARED_NOTE);
    // 接手租约的实例读到暂停：同步为暂停接收且不送达。
    wait_for_receiving(&other.agent, chat, STOPPED).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(other.model.prompts_containing(SHARED_NOTE), 0);
    holder.agent.shutdown().await;
    other.agent.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_chat_in_the_waiting_instance_pauses_the_lease_holder() {
    let SharedInstances {
        _directory,
        root,
        chat,
        holder,
        other,
    } = two_receiving_instances().await;
    other.agent.stop(chat).await.unwrap();
    // 持有者下一次送达前读到接收状态：同步为暂停接收、归还租约且不送达。
    let delivered = holder.model.prompts_containing(SHARED_NOTE);
    holder.shell.update(3, SHARED_NOTE);
    wait_for_receiving(&holder.agent, chat, STOPPED).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(holder.model.prompts_containing(SHARED_NOTE), delivered);
    assert!(!lock_path(&root, chat).exists());
    holder.agent.shutdown().await;
    other.agent.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_deliveries_leave_exactly_one_holder_and_keep_its_lock() {
    const INSTANCES: usize = 4;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let task = Uuid::new_v4();
    let chat = saved_chat_with_shared_loop(&root, directory.path(), task).await;
    let mut instances = Vec::new();
    for index in 0..INSTANCES {
        instances.push(resumed_instance(&root, chat, task, &format!("resume {index}")).await);
    }
    for instance in &instances {
        instance.shell.update(1, SHARED_NOTE);
    }
    wait_until("one instance to deliver", WAIT_TIMEOUT, || {
        instances
            .iter()
            .any(|instance| instance.model.prompts_containing(SHARED_NOTE) >= 1)
    })
    .await;
    tokio::time::sleep(SETTLE).await;
    let delivered = instances
        .iter()
        .map(|instance| instance.model.prompts_containing(SHARED_NOTE))
        .sum::<usize>();
    assert_eq!(delivered, 1, "the Chat was delivered {delivered} times");
    assert!(
        lock_path(&root, chat).exists(),
        "a failed attempt removed the holder's lock"
    );
    for instance in instances {
        instance.agent.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_chat_stays_paused_after_a_restart_until_a_new_input() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let task = Uuid::new_v4();
    let chat = saved_chat_with_shared_loop(&root, directory.path(), task).await;
    let first = resumed_instance(&root, chat, task, "goal to stop").await;
    // 先写出接收中的状态，暂停写成对话叶子之后的尾随 Loop 条目。
    first.agent.flush_transcripts().await;
    first.agent.stop(chat).await.unwrap();
    first.agent.shutdown().await;
    drop(first);

    let model = EchoModel::new();
    let restarted = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let shell = ShellTick::new(task);
    shell.install(&restarted).await;
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(restored.chat_loop.receiving, STOPPED);
    shell.wait_until_live(&restarted, chat).await;
    shell.update(1, "stopped check");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.prompts_containing("stopped check"), 0);
    complete_turn(&restarted, chat, "continue after restart").await;
    assert!(
        restarted
            .snapshot(chat)
            .await
            .unwrap()
            .chat_loop
            .receiving
            .is_receiving()
    );
    wait_until(
        "the continued Loop to take the update",
        WAIT_TIMEOUT,
        || model.prompts_containing("stopped check") >= 1,
    )
    .await;
    restarted.shutdown().await;
}

#[derive(serde::Deserialize, serde::Serialize)]
struct LeaseHolderPlan {
    root: PathBuf,
    chat: ChatId,
    task: Uuid,
    ready: PathBuf,
}

/// 子进程入口：未设置 `TICKLOOP_TEST_LEASE_HOLDER` 时什么也不做。设置时恢复并继续 Chat，送达
/// 一次 Shell Tick 更新后写出就绪文件，之后一直持有租约直到被杀死。
#[tokio::test(flavor = "multi_thread")]
async fn lease_holder_child_process() {
    let Ok(plan) = std::env::var(LEASE_HOLDER_ENV) else {
        return;
    };
    let plan: LeaseHolderPlan = serde_json::from_str(&plan).unwrap();
    let holder = resumed_instance(&plan.root, plan.chat, plan.task, "resume in the child").await;
    holder.shell.update(1, SHARED_NOTE);
    wait_until("the child to deliver", WAIT_TIMEOUT, || {
        holder.model.prompts_containing(SHARED_NOTE) >= 1
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
    let task = Uuid::new_v4();
    let chat = saved_chat_with_shared_loop(&root, directory.path(), task).await;
    let ready = directory.path().join("holder-ready");
    let plan = LeaseHolderPlan {
        root: root.clone(),
        chat,
        task,
        ready: ready.clone(),
    };
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            tickloop_agent_test_support::test_path!(lease_holder_child_process).as_str(),
            "--nocapture",
        ])
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
    let lock = lock_path(&root, chat);
    assert_eq!(lock_holder_pid(&lock), Some(u64::from(child.id())));

    let other = resumed_instance(&root, chat, task, "resume in the other").await;
    other.shell.update(2, SHARED_NOTE);
    wait_for_notice(
        &other.agent,
        chat,
        LoopTriggerNoticeKind::HeldByAnotherProcess,
    )
    .await;
    assert_eq!(other.model.prompts_containing(SHARED_NOTE), 0);

    // SIGKILL：子进程来不及归还租约，锁文件留在原处，由内核释放 flock；等待中的送达随后重试。
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(lock.exists());
    wait_until("the waiting instance to take over", WAIT_TIMEOUT, || {
        other.model.prompts_containing(SECOND_UPDATE) >= 1
    })
    .await;
    assert_eq!(lock_holder_pid(&lock), Some(u64::from(std::process::id())));
    assert!(other.agent.loop_trigger_notices().is_empty());
    other.agent.shutdown().await;
}

#[derive(Clone, Copy, Debug)]
enum HolderExit {
    Shutdown,
    Drop,
}

/// 在独立的 tokio 运行时中恢复并继续 Chat、送达一次更新，送达后通知 `ready`，收到 `release`
/// 后按 `exit` 退出；`Drop` 时直接释放 Agent 与运行时，模拟实例退出而未调用 shutdown。
fn spawn_lease_holder(
    root: PathBuf,
    chat: ChatId,
    task: Uuid,
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
            let holder = resumed_instance(&root, chat, task, "resume in the holder").await;
            holder.shell.update(1, SHARED_NOTE);
            wait_until("the holder to deliver", WAIT_TIMEOUT, || {
                holder.model.prompts_containing(SHARED_NOTE) >= 1
            })
            .await;
            ready.send(()).unwrap();
            release.await.unwrap();
            match exit {
                HolderExit::Shutdown => holder.agent.shutdown().await,
                HolderExit::Drop => drop(holder),
            }
        });
        drop(runtime);
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_waiting_instance_takes_the_lease_once_the_holder_shuts_down_or_drops() {
    for exit in [HolderExit::Shutdown, HolderExit::Drop] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let task = Uuid::new_v4();
        let chat = saved_chat_with_shared_loop(&root, directory.path(), task).await;
        let (ready_sender, ready) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = tokio::sync::oneshot::channel();
        let holder = spawn_lease_holder(
            root.clone(),
            chat,
            task,
            exit,
            ready_sender,
            release_receiver,
        );
        tokio::time::timeout(WAIT_TIMEOUT, ready)
            .await
            .expect("holder did not deliver")
            .unwrap();

        let other = resumed_instance(&root, chat, task, "resume in the other").await;
        other.shell.update(2, SHARED_NOTE);
        wait_for_notice(
            &other.agent,
            chat,
            LoopTriggerNoticeKind::HeldByAnotherProcess,
        )
        .await;
        assert_eq!(other.model.prompts_containing(SHARED_NOTE), 0, "{exit:?}");

        release.send(()).unwrap();
        tokio::task::spawn_blocking(move || holder.join().unwrap())
            .await
            .unwrap();
        wait_until("the other instance to take over", WAIT_TIMEOUT, || {
            other.model.prompts_containing(SECOND_UPDATE) >= 1
        })
        .await;
        other.agent.shutdown().await;
    }
}

fn write_lock(root: &Path, chat: ChatId, content: &[u8]) {
    let path = lock_path(root, chat);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_lock_files_are_taken_over_whatever_pid_they_name_but_not_from_another_host() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = EchoModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let stream = InMemoryTickStream::new();
    let names = ["corrupt", "exited", "reused", "own", "foreign"];
    let mut chats = Vec::new();
    let mut shells = Vec::new();
    for name in names {
        let shell = ShellTick::on(stream.clone(), Uuid::new_v4());
        if shells.is_empty() {
            shell.install(&agent).await;
        }
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&agent, chat, &format!("{name} goal")).await;
        shell.subscribe(&agent, chat).await;
        chats.push(chat);
        shells.push(shell);
    }
    agent.flush_transcripts().await;

    let mut exited = std::process::Command::new("true").spawn().unwrap();
    let exited_pid = exited.id();
    exited.wait().unwrap();
    // 仍在运行但与 Tickloop 无关的进程：模拟锁文件中的 PID 已被复用。
    let mut unrelated = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let holder = |chat: ChatId, pid: u32| {
        serde_json::json!({"chatId": chat, "pid": pid, "acquiredAt": 1})
            .to_string()
            .into_bytes()
    };
    write_lock(&root, chats[0], b"{\"chatId\":");
    write_lock(&root, chats[1], &holder(chats[1], exited_pid));
    write_lock(&root, chats[2], &holder(chats[2], unrelated.id()));
    write_lock(&root, chats[3], &holder(chats[3], std::process::id()));
    write_lock(
        &root,
        chats[4],
        serde_json::json!({
            "chatId": chats[4],
            "pid": std::process::id(),
            "nonce": Uuid::new_v4(),
            "startedAt": 1,
            "host": "another-host.invalid",
        })
        .to_string()
        .as_bytes(),
    );

    for (shell, name) in shells.iter().zip(names) {
        shell.update(1, &format!("{name} check"));
    }
    wait_until("stale locks to be taken over", WAIT_TIMEOUT, || {
        ["corrupt", "exited", "reused", "own"]
            .iter()
            .all(|name| model.prompts_containing(&format!("{name} check")) >= 1)
    })
    .await;
    wait_for_notice(&agent, chats[4], LoopTriggerNoticeKind::Warning).await;
    assert_eq!(model.prompts_containing("foreign check"), 0);
    let notices = agent.loop_trigger_notices();
    let foreign = notices
        .iter()
        .find(|notice| notice.chat_id == chats[4])
        .expect("the foreign host lock is reported");
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
async fn an_unusable_lease_directory_keeps_the_delivery_back_with_a_visible_warning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = EchoModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let shell = ShellTick::new(Uuid::new_v4());
    shell.install(&agent).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "event goal").await;
    shell.subscribe(&agent, chat).await;
    agent.flush_transcripts().await;
    let chat_directory = chat_directory(&root, chat);
    std::fs::create_dir_all(&chat_directory).unwrap();
    std::fs::write(chat_directory.join("trigger"), b"not a directory").unwrap();

    shell.update(1, "event check");
    wait_for_notice(&agent, chat, LoopTriggerNoticeKind::Warning).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.prompts_containing("event check"), 0);
    agent.shutdown().await;
}
