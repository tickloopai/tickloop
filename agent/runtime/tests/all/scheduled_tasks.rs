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

use super::wakeup_support::{ContractModel, SETTLE, complete_turn, hold_a_turn};

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

/// The store fails while a wake-up hands a run over: the wake-up reports it and the task keeps
/// its time; the next wake-up stores the same run, which runs once.
#[tokio::test(flavor = "multi_thread")]
async fn a_storage_failure_keeps_the_task_due_and_the_retry_runs_it_once() {
    let faults = Arc::new(FaultyTranscriptStore::new(Arc::new(
        InMemoryTranscriptStore::new(),
    )));
    let model = ContractModel::new();
    let agent = stateless_host(faults.clone(), model.clone());
    let (chat, task) = chat_with_a_task(&agent).await;
    let due_at = task.next_fire_at_ms.unwrap();
    // The run waits behind a held turn, so only the ledger stands between it and a second run.
    hold_a_turn(&agent, chat, &model).await;

    faults.fail_appends();
    assert!(agent.wake_scheduled_tasks(chat, due_at).await.is_err());
    assert_eq!(tasks(&agent, chat).await, std::slice::from_ref(&task));

    faults.heal();
    let handed = agent.wake_scheduled_tasks(chat, due_at).await.unwrap();
    assert_eq!(handed.len(), 1);
    assert_ne!(tasks(&agent, chat).await, [task]);
    model.release_held_turn();
    model.wait_seen("check the build", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("check the build"), 1, "{:?}", model.prompts());
    agent.shutdown().await;
}

/// The host goes down after a wake-up stored the ledger of its runs and before it stored the
/// task's next time: the next host loads the run waiting and the task due, and waking it for
/// the same time is answered by the ledger. Both where the run queued behind a busy turn and
/// where it merged into a run waiting already.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_between_the_ledger_and_the_next_time_runs_nothing_twice() {
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
        agent.stop_scheduled_wakeups_before_recording_for_tests();
        let handed = agent.wake_scheduled_tasks(chat, due_at).await.unwrap();
        assert_eq!(handed.len(), 1);
        // The host is gone: the next one loads what the store holds now.
        let image = Arc::new(InMemoryTranscriptStore::new());
        image.seed(chat, store.lines(chat), store.position(chat).unwrap());
        agent.shutdown().await;

        let next_model = ContractModel::new();
        // The turn held when the host went down is continued by the next host; let it end.
        next_model.release_held_turn();
        let next = stateless_host(image, next_model.clone());
        next.resume_chat(chat).await.unwrap();
        let again = next.wake_scheduled_tasks(chat, due_at).await.unwrap();
        assert_eq!(
            again
                .iter()
                .map(|receipt| receipt.run_id)
                .collect::<Vec<_>>(),
            handed
                .iter()
                .map(|receipt| receipt.run_id)
                .collect::<Vec<_>>(),
            "merges: {merges}"
        );
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
