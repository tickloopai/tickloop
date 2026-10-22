//! Schedules a host manages for the user: a host may run the model's schedules at once and
//! without a deadline, create a schedule as a new Loop that waits for its first time, and let
//! the user rename, reword, reschedule, pause and resume one. A paused schedule stays paused on
//! the next instance and is told apart from one awaiting confirmation.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, LoopId, LoopState, PromptPolicy, Recurrence, ScheduleConfirmation,
};
use tickloop_agent_runtime::transcript::TranscriptStore;
use tickloop_agent_runtime::{
    Agent, AgentError, HarnessOptions, HarnessSeams, ModelSchedules, NewSchedule, ParentSelection,
    Quiescence, ScheduleEdit, ToolPolicy, WaitHosting,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, text};
use tickloop_agent_transcript::{SettledOutcome, input_ids};
use uuid::Uuid;

use super::durable_queue::{Script, Step, prompt};
use super::durable_triggers::{durable, loop_state, time, trigger_of, until_settled, wake_at};

const HOUR_MS: u64 = 3_600_000;
const DIGEST: &str = "Summarize the world news of the last day.";

fn trusting_host(store: Arc<dyn TranscriptStore>, model: Arc<Script>) -> Agent {
    Agent::with_seams(
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
                goals: true,
                questions: true,
                ..ToolPolicy::web_fetch_only()
            },
            model_schedules: ModelSchedules::TRUSTED,
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

fn hourly_digest() -> NewSchedule {
    NewSchedule {
        title: Some("  Daily digest ".into()),
        prompt: DIGEST.into(),
        schedule: Recurrence::parse("every 1h").unwrap(),
        expires_at: None,
    }
}

async fn chat_with_schedule(agent: &Agent) -> (ChatId, LoopId) {
    let chat = agent.create_chat_without_folder().await.unwrap();
    let loop_id = Uuid::new_v4();
    agent
        .create_schedule(chat, loop_id, hourly_digest())
        .await
        .unwrap();
    (chat, loop_id)
}

fn is_invalid(result: Result<(), AgentError>, words: &str) -> bool {
    matches!(result, Err(AgentError::Invalid(message)) if message.contains(words))
}

/// A host that trusts the model's schedules arms StartGoal's schedule at once, with its title
/// and without a deadline, and the family waits only for its first time.
#[tokio::test]
async fn a_trusting_host_runs_the_models_schedule_at_once_and_keeps_it() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![
        Step::Reply(call(
            "goal",
            tickloop_agent_tools::START_GOAL_TOOL_NAME,
            json!({"goal": DIGEST, "title": "每日全球新闻摘要", "schedule": "daily 09:31 Asia/Shanghai"}),
        )),
        Step::Reply(text("scheduled")),
    ]);
    let agent = trusting_host(store, model.clone());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt(
        "Every morning, digest the news.",
        ParentSelection::CurrentLeaf,
    );
    agent.advance(chat, request.clone()).await.unwrap();
    assert!(matches!(
        until_settled(&agent, chat, request.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    let goal = agent.snapshot(chat).await.unwrap().loops[1].clone();
    assert_eq!(goal.state, LoopState::WaitingTrigger);
    let trigger = goal.trigger.unwrap();
    assert!(trigger.armed);
    assert!(matches!(
        trigger.confirmation,
        Some(ScheduleConfirmation::Confirmed { .. })
    ));
    assert_eq!(trigger.expires_at_ms, None);
    assert_eq!(trigger.title.as_deref(), Some("每日全球新闻摘要"));
    assert_eq!(
        trigger.schedule.unwrap().to_string(),
        "daily 09:31 Asia/Shanghai"
    );
    let requests = model.requests();
    assert!(
        requests[1].contains(r#"\"state\":\"scheduled\""#),
        "{}",
        requests[1]
    );
    assert!(requests[1].contains("每日全球新闻摘要"), "{}", requests[1]);
    let Quiescence::Waiting(waits) = agent.quiesce(chat).await else {
        panic!("the family waits for the schedule");
    };
    assert!(waits.people.is_empty());
    assert_eq!(
        waits.wake_at.map(super::durable_triggers::milliseconds),
        trigger_of(&agent, chat, goal.id).await.next_scheduled_ms()
    );
    agent.shutdown().await;
}

/// A title StartGoal gives must be short; a blank or long one is refused before anything is
/// registered.
#[tokio::test]
async fn start_goal_refuses_a_title_that_is_blank_or_too_long() {
    let long = "x".repeat(tickloop_agent_protocol::MAX_TRIGGER_TITLE_CHARS + 1);
    for title in ["   ", long.as_str()] {
        let model = Script::new(vec![
            Step::Reply(call(
                "goal",
                tickloop_agent_tools::START_GOAL_TOOL_NAME,
                json!({"goal": DIGEST, "title": title, "schedule": "every 1h"}),
            )),
            Step::Reply(text("noted")),
        ]);
        let agent = trusting_host(Arc::new(InMemoryTranscriptStore::new()), model.clone());
        let chat = agent.create_chat_without_folder().await.unwrap();
        let request = prompt("Digest the news hourly.", ParentSelection::CurrentLeaf);
        agent.advance(chat, request.clone()).await.unwrap();
        until_settled(&agent, chat, request.input_id).await;
        assert!(model.requests()[1].contains("title must contain"));
        assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 1);
        agent.shutdown().await;
    }
}

/// A schedule the host creates is a new Loop that waits for its first time without running
/// now or awaiting confirmation; the wake at that time runs its prompt on another instance.
/// Creating the same Loop again changes nothing.
#[tokio::test]
async fn a_host_created_schedule_first_runs_at_its_next_time() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let first = durable(store.clone(), model.clone());
    let (chat, loop_id) = chat_with_schedule(&first).await;
    first
        .create_schedule(chat, loop_id, hourly_digest())
        .await
        .unwrap();
    let snapshot = first.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 1);
    assert_eq!(snapshot.loops[0].state, LoopState::WaitingTrigger);
    assert_eq!(snapshot.loops[0].goal, DIGEST);
    let trigger = trigger_of(&first, chat, loop_id).await;
    assert!(trigger.armed);
    assert_eq!(trigger.confirmation, None);
    assert_eq!(trigger.title.as_deref(), Some("Daily digest"));
    assert_eq!(trigger.expires_at_ms, None);
    assert!(model.requests().is_empty());
    let first_time = trigger.armed_at_ms.unwrap() + HOUR_MS;
    assert_eq!(wake_at(&first, chat).await, Some(first_time));
    first.sync().await.unwrap();
    first.shutdown().await;

    let model = Script::new(Vec::new());
    let next = durable(store, model.clone());
    next.wake(chat, Some(time(first_time))).await.unwrap();
    assert!(matches!(
        until_settled(&next, chat, input_ids::trigger(loop_id, first_time)).await,
        SettledOutcome::Completed { .. }
    ));
    assert!(model.requests()[0].contains(DIGEST));
    next.shutdown().await;
}

/// A schedule the host creates is refused when it cannot run: no prompt, a past deadline, a
/// rule below the host's floor or without a future time, or a bad title.
#[tokio::test]
async fn a_host_created_schedule_that_cannot_run_is_refused() {
    let agent = durable(
        Arc::new(InMemoryTranscriptStore::new()),
        Script::new(Vec::new()),
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let cases = [
        (
            NewSchedule {
                prompt: " ".into(),
                ..hourly_digest()
            },
            "requires a prompt",
        ),
        (
            NewSchedule {
                expires_at: Some(std::time::SystemTime::now() - Duration::from_secs(60)),
                ..hourly_digest()
            },
            "deadline has passed",
        ),
        (
            NewSchedule {
                schedule: Recurrence::parse("at 2020-01-01T00:00:00Z").unwrap(),
                ..hourly_digest()
            },
            "no future run",
        ),
        (
            NewSchedule {
                title: Some(String::new()),
                ..hourly_digest()
            },
            "title must contain",
        ),
    ];
    for (schedule, words) in cases {
        let result = agent.create_schedule(chat, Uuid::new_v4(), schedule).await;
        assert!(is_invalid(result, words), "{words}");
    }
    assert!(agent.snapshot(chat).await.unwrap().loops.is_empty());
    agent.shutdown().await;
}

/// A paused schedule does not run and needs no wake; it stays paused on the next instance,
/// apart from one awaiting confirmation, until resuming arms it from then on.
#[tokio::test]
async fn a_paused_schedule_waits_for_the_user_to_resume_it() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(store.clone(), Script::new(Vec::new()));
    let (chat, loop_id) = chat_with_schedule(&first).await;
    first.pause_schedule(chat, loop_id).await.unwrap();
    first.pause_schedule(chat, loop_id).await.unwrap();
    let paused = trigger_of(&first, chat, loop_id).await;
    assert!(!paused.armed);
    assert!(paused.is_paused());
    assert!(!paused.awaits_confirmation());
    assert_eq!(
        loop_state(&first, chat, loop_id).await,
        LoopState::Completed
    );
    assert_eq!(first.quiesce(chat).await, Quiescence::Idle);
    first.sync().await.unwrap();
    first.shutdown().await;

    let next = durable(store, Script::new(Vec::new()));
    next.wake(chat, None).await.unwrap();
    assert!(trigger_of(&next, chat, loop_id).await.is_paused());
    next.resume_schedule(chat, loop_id).await.unwrap();
    next.resume_schedule(chat, loop_id).await.unwrap();
    let resumed = trigger_of(&next, chat, loop_id).await;
    assert!(resumed.armed);
    assert!(!resumed.is_paused());
    assert_eq!(
        loop_state(&next, chat, loop_id).await,
        LoopState::WaitingTrigger
    );
    assert_eq!(
        wake_at(&next, chat).await,
        Some(resumed.armed_at_ms.unwrap() + HOUR_MS)
    );
    next.shutdown().await;
}

/// A new title or prompt leaves the schedule's times alone; a new rule counts from now and
/// keeps a paused schedule paused. An ended schedule cannot change.
#[tokio::test]
async fn editing_a_schedule_changes_its_times_only_with_a_new_rule() {
    let agent = durable(
        Arc::new(InMemoryTranscriptStore::new()),
        Script::new(Vec::new()),
    );
    let (chat, loop_id) = chat_with_schedule(&agent).await;
    let before = trigger_of(&agent, chat, loop_id).await;
    agent
        .edit_schedule(
            chat,
            loop_id,
            ScheduleEdit {
                title: Some("News".into()),
                prompt: Some("Only the headlines.".into()),
                schedule: Some(Recurrence::parse("every 1h").unwrap()),
            },
        )
        .await
        .unwrap();
    let renamed = trigger_of(&agent, chat, loop_id).await;
    assert_eq!(renamed.title.as_deref(), Some("News"));
    assert_eq!(renamed.prompt, "Only the headlines.");
    assert_eq!(renamed.armed_at_ms, before.armed_at_ms);
    assert_eq!(renamed.id, before.id);

    agent.pause_schedule(chat, loop_id).await.unwrap();
    let daily = Recurrence::parse("weekly mon,fri 08:00 America/New_York").unwrap();
    agent
        .edit_schedule(
            chat,
            loop_id,
            ScheduleEdit {
                schedule: Some(daily),
                ..ScheduleEdit::default()
            },
        )
        .await
        .unwrap();
    let rescheduled = trigger_of(&agent, chat, loop_id).await;
    assert_eq!(rescheduled.schedule, Some(daily));
    assert!(rescheduled.is_paused());
    assert!(!rescheduled.armed);
    assert_eq!(rescheduled.title.as_deref(), Some("News"));

    agent.resume_schedule(chat, loop_id).await.unwrap();
    assert!(trigger_of(&agent, chat, loop_id).await.armed);
    assert!(is_invalid(
        agent
            .edit_schedule(
                chat,
                loop_id,
                ScheduleEdit {
                    title: Some(" ".into()),
                    ..ScheduleEdit::default()
                },
            )
            .await,
        "title must contain"
    ));

    agent.stop(chat, loop_id).await.unwrap();
    assert!(is_invalid(
        agent.pause_schedule(chat, loop_id).await,
        "has ended"
    ));
    assert!(is_invalid(
        agent
            .edit_schedule(
                chat,
                loop_id,
                ScheduleEdit {
                    prompt: Some("again".into()),
                    ..ScheduleEdit::default()
                },
            )
            .await,
        "has ended"
    ));
    agent.shutdown().await;
}

/// A schedule awaiting the user's confirmation is confirmed or rejected, not paused, resumed
/// or edited.
#[tokio::test]
async fn a_schedule_awaiting_confirmation_cannot_be_paused_or_edited() {
    let model = Script::new(vec![
        super::durable_triggers::start_goal("every 1h"),
        Step::Reply(text("registered")),
    ]);
    let agent = durable(Arc::new(InMemoryTranscriptStore::new()), model);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("Hourly news, please.", ParentSelection::CurrentLeaf);
    agent.advance(chat, request.clone()).await.unwrap();
    until_settled(&agent, chat, request.input_id).await;
    let loop_id = agent.snapshot(chat).await.unwrap().loops[1].id;
    assert!(
        trigger_of(&agent, chat, loop_id)
            .await
            .awaits_confirmation()
    );
    for result in [
        agent.pause_schedule(chat, loop_id).await,
        agent.resume_schedule(chat, loop_id).await,
        agent
            .edit_schedule(
                chat,
                loop_id,
                ScheduleEdit {
                    title: Some("News".into()),
                    ..ScheduleEdit::default()
                },
            )
            .await,
    ] {
        assert!(is_invalid(result, "awaits the user's confirmation"));
    }
    agent.shutdown().await;
}
