//! Scheduled runs at the store's commit boundary, on a stateless host that wakes the Chat at the
//! time the Harness reported.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use tickloop_agent_protocol::{AgentHostOptions, ChatId, PromptPolicy, ScheduledTask};
use tickloop_agent_runtime::transcript::TranscriptStore;
use tickloop_agent_runtime::{Agent, HarnessOptions, HarnessSeams, ToolPolicy, WaitHosting};
use tickloop_agent_test_support::config;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::{FaultyTranscriptStore, InMemoryTranscriptStore};

use super::wakeup_support::{ContractModel, SETTLE, WAIT, complete_turn, hold_a_turn};

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 23, hour, minute, 0).unwrap()
}

fn stateless_host(store: Arc<dyn TranscriptStore>, model: Arc<ContractModel>) -> Agent {
    let agent = Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                scheduled_tasks: true,
                ..ToolPolicy::web_fetch_only()
            },
            waits: WaitHosting::Durable,
            time_zone: Some("UTC".into()),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    agent.set_scheduled_now_for_tests(at(10, 0));
    agent
}

/// A materialized Chat with a task every ten minutes from 10:00.
async fn chat_with_a_task(agent: &Agent) -> (ChatId, ScheduledTask) {
    let chat = agent.create_chat_without_folder().await.unwrap();
    complete_turn(agent, chat, "get ready").await;
    let task = agent
        .schedule_task(chat, "*/10 * * * *", "check the build".into(), true)
        .await
        .unwrap();
    (chat, task)
}

async fn tasks(agent: &Agent, chat: ChatId) -> Vec<ScheduledTask> {
    agent.snapshot(chat).await.unwrap().scheduled_tasks
}

/// The store fails while a wake-up hands a run over: the run is accepted and the task moves on
/// with it, and the run's turn waits for its records; once the store takes them the wake-up
/// returns the run, which runs once.
#[tokio::test(flavor = "multi_thread")]
async fn a_storage_failure_holds_the_run_until_it_and_its_task_are_stored() {
    let faults = Arc::new(FaultyTranscriptStore::new(Arc::new(
        InMemoryTranscriptStore::new(),
    )));
    let model = ContractModel::new();
    let agent = stateless_host(faults.clone(), model.clone());
    let (chat, task) = chat_with_a_task(&agent).await;
    let due_at = task.next_fire_at_ms.unwrap();

    faults.fail_appends();
    let waking = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wake_scheduled_tasks(chat, due_at).await }
    });
    tokio::time::timeout(WAIT, async {
        while tasks(&agent, chat).await == std::slice::from_ref(&task) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the task moves on as its run is accepted");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("check the build"), 0, "{:?}", model.prompts());

    faults.heal();
    let handed = waking.await.unwrap().unwrap();
    assert_eq!(handed.len(), 1);
    model.wait_seen("check the build", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("check the build"), 1, "{:?}", model.prompts());
    agent.shutdown().await;
}

/// The host goes down right after a wake-up handed its run over: the next host loads the run
/// waiting and the task already moved on with it, so waking it for the same time hands nothing
/// over again and the run runs once. Both where the run queued behind a busy turn and
/// where it merged into a run waiting already.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_a_wake_up_finds_the_task_moved_on_with_its_run() {
    for merges in [false, true] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let model = ContractModel::new();
        let agent = stateless_host(store.clone(), model.clone());
        let (chat, task) = chat_with_a_task(&agent).await;
        hold_a_turn(&agent, chat, &model).await;
        let mut due_at = task.next_fire_at_ms.unwrap();
        if merges {
            agent.wake_scheduled_tasks(chat, due_at).await.unwrap();
            due_at = tasks(&agent, chat).await[0].next_fire_at_ms.unwrap();
        }
        let handed = agent.wake_scheduled_tasks(chat, due_at).await.unwrap();
        assert_eq!(handed.len(), 1);
        let moved_on = tasks(&agent, chat).await;
        // The host is gone: the next one loads what the store holds now.
        let image = Arc::new(InMemoryTranscriptStore::new());
        image.seed(chat, store.lines(chat), store.position(chat).unwrap());
        agent.shutdown().await;

        let next_model = ContractModel::new();
        // The turn held when the host went down is continued by the next host; let it end.
        next_model.release_held_turn();
        let next = stateless_host(image, next_model.clone());
        next.resume_chat(chat).await.unwrap();
        assert_eq!(tasks(&next, chat).await, moved_on, "merges: {merges}");
        let again = next.wake_scheduled_tasks(chat, due_at).await.unwrap();
        assert!(again.is_empty(), "merges: {merges}: {again:?}");
        next_model.wait_seen("check the build", 1).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(
            next_model.seen("check the build"),
            1,
            "merges: {merges}: {:?}",
            next_model.prompts()
        );
        next.shutdown().await;
    }
}

/// A stateless host whose Chats have a Folder and take the user's Skills, as `/loop` needs.
fn stateless_host_with_a_folder(
    store: Arc<dyn TranscriptStore>,
    model: Arc<ContractModel>,
) -> Agent {
    let agent = Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: Some(Arc::new(
                tickloop_agent_runtime::environment::LocalEnvironment::bare(),
            )),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                directory_trust: tickloop_agent_protocol::DirectoryTrustPolicy::CallerManaged,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                scheduled_tasks: true,
                skills: true,
                user_skills: true,
                ..ToolPolicy::web_fetch_only()
            },
            waits: WaitHosting::Durable,
            time_zone: Some("UTC".into()),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    agent.set_scheduled_now_for_tests(at(10, 0));
    agent
}

/// The first turn of the user's self-paced `/loop` is still running when its host goes away;
/// the next host goes on with it from the Transcript alone, and when it ends without a
/// reschedule the loop still gets its fallback wake-up.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_loop_turn_continued_by_another_host_still_falls_back() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = ContractModel::new();
    let agent = stateless_host_with_a_folder(store.clone(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    agent
        .invoke_skill(chat, "loop", "watch the build, HOLD:turn")
        .await
        .unwrap();
    model.wait_seen("watch the build", 1).await;
    let task = tasks(&agent, chat).await.remove(0);
    assert!(task.started_by_run.is_some());
    assert_eq!(task.next_fire_at_ms, None, "{:?}", model.prompts());
    agent.sync().await.unwrap();
    let image = Arc::new(InMemoryTranscriptStore::new());
    image.seed(chat, store.lines(chat), store.position(chat).unwrap());
    agent.shutdown().await;

    let next_model = ContractModel::new();
    next_model.release_held_turn();
    let next = stateless_host_with_a_folder(image, next_model.clone());
    next.resume_chat(chat).await.unwrap();
    next_model.wait_seen("watch the build", 1).await;
    next.quiesce(chat).await;
    let fallback = tasks(&next, chat).await.remove(0);
    assert_eq!(fallback.id, task.id);
    assert_eq!(
        fallback.next_fire_at_ms,
        Some(u64::try_from(at(10, 20).timestamp_millis()).unwrap())
    );
    next.shutdown().await;
}

/// A process that loaded a task takes the Chat's lease over after another process deleted the
/// task and left: it goes on from what the other recorded, so the task's run is neither taken
/// nor run, and the task is not written back.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_taking_the_chat_over_does_not_run_a_task_deleted_elsewhere() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    holder.set_scheduled_now_for_tests(at(10, 0));
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&holder, chat, "get ready").await;
    let task = holder
        .schedule_task(chat, "*/10 * * * *", "check the build".into(), true)
        .await
        .unwrap();
    // The holder runs the task once, so the Chat's Trigger lease is its own.
    holder
        .wake_scheduled_tasks(chat, task.next_fire_at_ms.unwrap())
        .await
        .unwrap();
    model.wait_seen("check the build", 1).await;
    holder.flush_transcripts().await;

    let taker_model = ContractModel::new();
    let taker = Agent::open_embedded(root.clone(), config(), taker_model.clone()).unwrap();
    taker.set_scheduled_now_for_tests(at(10, 25));
    taker.resume_chat(chat).await.unwrap();
    complete_turn(&taker, chat, "resume here").await;
    let loaded = tasks(&taker, chat).await;
    assert_eq!(loaded.len(), 1, "{loaded:?}");

    holder.delete_scheduled_task(chat, &task.id).await.unwrap();
    holder.shutdown().await;

    taker.set_scheduled_now_for_tests(at(10, 35));
    let woken = taker
        .wake_scheduled_tasks(chat, loaded[0].next_fire_at_ms.unwrap())
        .await;
    assert!(
        woken.is_err(),
        "the run of the deleted task is refused: {woken:?}"
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        taker_model.seen("check the build"),
        0,
        "{:?}",
        taker_model.prompts()
    );
    assert!(tasks(&taker, chat).await.is_empty());
    taker.flush_transcripts().await;
    taker.shutdown().await;

    let reader = Agent::open_embedded(root, config(), ContractModel::new()).unwrap();
    reader.resume_chat(chat).await.unwrap();
    assert!(
        tasks(&reader, chat).await.is_empty(),
        "the task is not written back"
    );
    reader.shutdown().await;
}
