//! A schedule the model creates with StartGoal runs only once the user confirms it: locally the
//! call's permission prompt confirms it, and a host without prompts registers it to await the
//! user's decision through `decide_schedule`. Such a schedule expires after 30 days unless the
//! user moves its deadline, a Chat keeps at most 20 active schedules, and the confirmation,
//! rejection and expiry are recorded with the Loop, so another host finds them.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tickloop_agent_protocol::{
    ChatId, Content, EventKind, LoopId, LoopState, ScheduleConfirmation,
};
use tickloop_agent_runtime::{
    Agent, ParentSelection, PersonWait, PersonWaitKind, Quiescence, ScheduleDecision,
};
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tickloop_agent_transcript::{Line, SettledOutcome};

use super::durable_queue::{Script, Step, prompt};
use super::durable_triggers::{
    durable, every, loop_state, milliseconds, start_goal, time, trigger_of, until_settled, wake_at,
};

const DAY_MS: u64 = 86_400_000;
const HOUR_MS: u64 = 3_600_000;
const MODEL_SCHEDULE_LIFETIME_MS: u64 = 30 * DAY_MS;
const MAX_ACTIVE_SCHEDULES: usize = 20;
/// How far apart the test's clock readings may be.
const CLOCK_SLACK_MS: u64 = 60_000;

fn now_ms() -> u64 {
    milliseconds(SystemTime::now())
}

/// A Chat whose model registered an hourly goal on a host without prompts, and that goal's Loop.
async fn chat_with_scheduled_goal(agent: &Agent) -> (ChatId, LoopId) {
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt(
        "Every hour, summarize the news.",
        ParentSelection::CurrentLeaf,
    );
    agent.advance(chat, request.clone()).await.unwrap();
    assert!(matches!(
        until_settled(agent, chat, request.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    let goal = agent.snapshot(chat).await.unwrap().loops[1].id;
    (chat, goal)
}

fn scheduling_model() -> Arc<Script> {
    Script::new(vec![
        start_goal("every 1h"),
        Step::Reply(text("registered")),
    ])
}

/// The confirmation each record of the Loop holds, oldest first.
fn recorded_confirmations(lines: &[Line], loop_id: LoopId) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::System(entry) => entry.loop_record.as_ref(),
            _ => None,
        })
        .filter(|record| record.loop_id == loop_id)
        .filter_map(|record| record.trigger.as_ref())
        .map(|trigger| trigger["confirmation"]["state"].clone())
        .collect()
}

fn recorded_states(lines: &[Line], loop_id: LoopId) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::System(entry) => entry.loop_record.as_ref(),
            _ => None,
        })
        .filter(|record| record.loop_id == loop_id)
        .map(|record| record.state.clone())
        .collect()
}

/// A host without prompts registers the model's schedule to await the user: it does not run at
/// its times, it expires in 30 days, and the family waits for a person to decide it. The user's
/// confirmation on another instance arms it from then, as the records show.
#[tokio::test]
async fn a_schedule_awaiting_confirmation_never_runs_until_the_user_confirms_it() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(store.clone(), scheduling_model());
    let (chat, goal) = chat_with_scheduled_goal(&first).await;
    let registered_at = now_ms();
    let pending = trigger_of(&first, chat, goal).await;
    assert_eq!(pending.confirmation, Some(ScheduleConfirmation::Pending));
    assert!(!pending.armed);
    let deadline = pending.expires_at_ms.unwrap();
    assert!(deadline.abs_diff(registered_at + MODEL_SCHEDULE_LIFETIME_MS) < CLOCK_SLACK_MS);
    let Quiescence::Waiting(waits) = first.quiesce(chat).await else {
        panic!("the family waits for the user's decision");
    };
    assert_eq!(
        waits.people,
        [PersonWait {
            chat,
            request: goal,
            kind: PersonWaitKind::Schedule,
        }]
    );
    assert_eq!(wake_at(&first, chat).await, Some(deadline));
    first.sync().await.unwrap();
    first.shutdown().await;

    // A wake for its first hour runs nothing.
    let model = Script::new(Vec::new());
    let woken = durable(store.clone(), model.clone());
    woken
        .wake(chat, Some(time(registered_at + HOUR_MS)))
        .await
        .unwrap();
    assert_eq!(woken.quiesce(chat).await, Quiescence::Waiting(waits));
    assert!(model.requests().is_empty());
    woken.sync().await.unwrap();
    woken.shutdown().await;

    let model = Script::new(Vec::new());
    let confirming = durable(store.clone(), model.clone());
    confirming
        .decide_schedule(chat, goal, ScheduleDecision::Confirm)
        .await
        .unwrap();
    let confirmed = trigger_of(&confirming, chat, goal).await;
    assert!(confirmed.armed);
    assert!(matches!(
        confirmed.confirmation,
        Some(ScheduleConfirmation::Confirmed { .. })
    ));
    assert_eq!(confirmed.expires_at_ms, Some(deadline));
    let first_time = confirmed.armed_at_ms.unwrap() + HOUR_MS;
    assert_eq!(wake_at(&confirming, chat).await, Some(first_time));
    // Confirming again changes nothing; rejecting a confirmed schedule is refused.
    confirming
        .decide_schedule(chat, goal, ScheduleDecision::Confirm)
        .await
        .unwrap();
    assert!(
        confirming
            .decide_schedule(chat, goal, ScheduleDecision::Reject)
            .await
            .is_err()
    );
    confirming.sync().await.unwrap();
    confirming.shutdown().await;
    assert_eq!(
        recorded_confirmations(&store.lines(chat), goal)
            .last()
            .unwrap(),
        "confirmed"
    );

    let model = Script::new(Vec::new());
    let next = durable(store.clone(), model.clone());
    next.wake(chat, Some(time(first_time))).await.unwrap();
    let fire = tickloop_agent_transcript::input_ids::trigger(goal, first_time);
    assert!(matches!(
        until_settled(&next, chat, fire).await,
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(model.requests().len(), 1);
    next.shutdown().await;
}

/// The user's rejection stops the Loop before it ever runs, and the record keeps it.
#[tokio::test]
async fn a_rejected_schedule_stops_its_loop_and_stays_rejected_on_another_host() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(store.clone(), scheduling_model());
    let (chat, goal) = chat_with_scheduled_goal(&first).await;
    first.sync().await.unwrap();
    first.shutdown().await;

    let rejecting = durable(store.clone(), Script::new(Vec::new()));
    rejecting
        .decide_schedule(chat, goal, ScheduleDecision::Reject)
        .await
        .unwrap();
    assert_eq!(loop_state(&rejecting, chat, goal).await, LoopState::Stopped);
    rejecting
        .decide_schedule(chat, goal, ScheduleDecision::Reject)
        .await
        .unwrap();
    rejecting.sync().await.unwrap();
    rejecting.shutdown().await;
    let lines = store.lines(chat);
    assert_eq!(
        recorded_confirmations(&lines, goal).last().unwrap(),
        "rejected"
    );
    assert_eq!(recorded_states(&lines, goal).last().unwrap(), "Stopped");

    let model = Script::new(Vec::new());
    let next = durable(store.clone(), model.clone());
    next.wake(chat, Some(time(now_ms() + HOUR_MS)))
        .await
        .unwrap();
    assert!(matches!(
        trigger_of(&next, chat, goal).await.confirmation,
        Some(ScheduleConfirmation::Rejected { .. })
    ));
    assert!(
        next.decide_schedule(chat, goal, ScheduleDecision::Confirm)
            .await
            .is_err()
    );
    assert_eq!(next.quiesce(chat).await, Quiescence::Idle);
    assert!(model.requests().is_empty());
    next.shutdown().await;
}

/// A schedule nobody confirmed expires at its deadline, and can no longer be confirmed.
#[tokio::test]
async fn an_unconfirmed_schedule_expires_at_its_deadline() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(store.clone(), scheduling_model());
    let (chat, goal) = chat_with_scheduled_goal(&first).await;
    let deadline = trigger_of(&first, chat, goal).await.expires_at_ms.unwrap();
    first.sync().await.unwrap();
    first.shutdown().await;

    let next = durable(store.clone(), Script::new(Vec::new()));
    next.wake(chat, Some(time(deadline))).await.unwrap();
    assert_eq!(loop_state(&next, chat, goal).await, LoopState::Expired);
    assert!(
        next.decide_schedule(chat, goal, ScheduleDecision::Confirm)
            .await
            .is_err()
    );
    assert_eq!(next.quiesce(chat).await, Quiescence::Idle);
    next.sync().await.unwrap();
    next.shutdown().await;
    assert_eq!(
        recorded_states(&store.lines(chat), goal).last().unwrap(),
        "Expired"
    );
}

/// The user moves the deadline of the model's schedule, or clears it.
#[tokio::test]
async fn the_user_extends_or_clears_the_deadline_of_the_models_schedule() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = durable(store.clone(), scheduling_model());
    let (chat, goal) = chat_with_scheduled_goal(&agent).await;
    let later = now_ms() + 60 * DAY_MS;
    agent
        .set_schedule_expiry(chat, goal, Some(time(later)))
        .await
        .unwrap();
    assert_eq!(
        trigger_of(&agent, chat, goal).await.expires_at_ms,
        Some(later)
    );
    assert!(
        agent
            .set_schedule_expiry(chat, goal, Some(time(now_ms() - 1)))
            .await
            .is_err()
    );
    agent
        .decide_schedule(chat, goal, ScheduleDecision::Confirm)
        .await
        .unwrap();
    agent.set_schedule_expiry(chat, goal, None).await.unwrap();
    let trigger = trigger_of(&agent, chat, goal).await;
    assert_eq!(trigger.expires_at_ms, None);
    assert!(trigger.armed);
    assert_eq!(
        wake_at(&agent, chat).await,
        Some(trigger.armed_at_ms.unwrap() + HOUR_MS)
    );
    agent.shutdown().await;
}

/// A Chat keeps at most 20 active schedules: the model's 21st is refused, and so is the user's
/// until one of them ends.
#[tokio::test]
async fn a_chat_keeps_at_most_twenty_active_schedules() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let calls = (0..=MAX_ACTIVE_SCHEDULES)
        .flat_map(|index| {
            call(
                &format!("goal-{index}"),
                tickloop_agent_tools::START_GOAL_TOOL_NAME,
                json!({"goal": format!("Check feed {index}."), "schedule": "every 1h"}),
            )
        })
        .collect::<Vec<_>>();
    let model = Script::new(vec![Step::Reply(calls), Step::Reply(text("registered"))]);
    let agent = durable(store.clone(), model.clone());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("Watch every feed.", ParentSelection::CurrentLeaf);
    let turn = agent.advance(chat, request.clone()).await.unwrap();
    until_settled(&agent, chat, request.input_id).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 1 + MAX_ACTIVE_SCHEDULES);
    let refusal = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == &format!("goal-{MAX_ACTIVE_SCHEDULES}") => {
                Some((content.text().into_owned(), *is_error))
            }
            _ => None,
        })
        .unwrap();
    assert!(refusal.1, "{}", refusal.0);
    assert!(refusal.0.contains("20 active schedules"), "{}", refusal.0);

    let own = turn.receipt().loop_id;
    let refused = agent
        .configure_trigger(chat, own, Some(every("every 1h")))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("20 active schedules"),
        "{refused}"
    );
    agent
        .decide_schedule(chat, snapshot.loops[1].id, ScheduleDecision::Reject)
        .await
        .unwrap();
    agent
        .configure_trigger(chat, own, Some(every("every 1h")))
        .await
        .unwrap();
    agent.shutdown().await;
}

/// A schedule awaiting the user's decision is not reconfigured around it.
#[tokio::test]
async fn a_schedule_awaiting_confirmation_is_decided_before_it_is_reconfigured() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = durable(store.clone(), scheduling_model());
    let (chat, goal) = chat_with_scheduled_goal(&agent).await;
    let refused = agent
        .configure_trigger(chat, goal, Some(every("every 2h")))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("confirm"), "{refused}");
    agent
        .decide_schedule(chat, goal, ScheduleDecision::Confirm)
        .await
        .unwrap();
    agent
        .configure_trigger(chat, goal, Some(every("every 2h")))
        .await
        .unwrap();
    // The confirmation stays the user's own decision, whatever the configuration carried.
    assert!(matches!(
        trigger_of(&agent, chat, goal).await.confirmation,
        Some(ScheduleConfirmation::Confirmed { .. })
    ));
    agent.shutdown().await;
}

/// Locally the call's permission prompt confirms the schedule: approving it registers the
/// schedule armed and confirmed, expiring in 30 days, and the Transcript records it; the deadline
/// still ends the Loop when it comes.
#[tokio::test]
async fn locally_the_approved_prompt_confirms_the_models_schedule_until_it_expires() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "goal",
            tickloop_agent_tools::START_GOAL_TOOL_NAME,
            json!({"goal": "Summarize the news.", "schedule": "every 1h"}),
        ),
        text("Scheduled."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Every hour, summarize the news.".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.name, tickloop_agent_tools::START_GOAL_TOOL_NAME);
                assert!(
                    request
                        .approval_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("confirms")),
                    "{:?}",
                    request.approval_reason
                );
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let goal = agent.snapshot(chat).await.unwrap().loops[1].clone();
    assert_eq!(goal.state, LoopState::WaitingTrigger);
    let trigger = goal.trigger.unwrap();
    assert!(trigger.armed);
    assert!(matches!(
        trigger.confirmation,
        Some(ScheduleConfirmation::Confirmed { .. })
    ));
    assert!(
        trigger
            .expires_at_ms
            .unwrap()
            .abs_diff(now_ms() + MODEL_SCHEDULE_LIFETIME_MS)
            < CLOCK_SLACK_MS
    );

    agent
        .set_schedule_expiry(chat, goal.id, Some(time(now_ms() + 300)))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while agent.snapshot(chat).await.unwrap().loops[1].state != LoopState::Expired {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the deadline ends the Loop");
    agent.shutdown().await;
    let recorded =
        std::fs::read_to_string(tickloop_agent_test_support::transcript_file(&root, chat)).unwrap();
    assert!(recorded.contains(r#""state":"confirmed""#), "{recorded}");
    assert!(recorded.contains(r#""Expired""#));
}

/// Locally, refusing the prompt registers no schedule.
#[tokio::test]
async fn locally_a_refused_prompt_registers_no_schedule() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "goal",
            tickloop_agent_tools::START_GOAL_TOOL_NAME,
            json!({"goal": "Summarize the news.", "schedule": "every 1h"}),
        ),
        text("Not scheduled."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Every hour, summarize the news.".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } | EventKind::Interrupted
                if event.run_id == receipt.run_id =>
            {
                break;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 1);
    agent.shutdown().await;
}

/// A local host without prompts has no one to confirm the model's schedule: it refuses it rather
/// than keep a schedule nobody can decide.
#[tokio::test]
async fn a_local_host_without_prompts_refuses_the_models_schedule() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "goal",
            tickloop_agent_tools::START_GOAL_TOOL_NAME,
            json!({"goal": "Summarize the news.", "schedule": "every 1h"}),
        ),
        text("Not scheduled."),
    ]));
    let agent = Agent::with_seams(
        tickloop_agent_runtime::HarnessSeams {
            transcripts: None,
            environment: Some(Arc::new(
                tickloop_agent_runtime::environment::LocalEnvironment::bare(),
            )),
            models: tickloop_agent_test_support::harness::ChosenModels::new(model.clone()),
        },
        tickloop_agent_runtime::HarnessOptions {
            model: config(),
            settings: None,
            host: tickloop_agent_protocol::AgentHostOptions {
                prompt_policy: tickloop_agent_protocol::PromptPolicy::DenyOnPrompt,
                ..tickloop_agent_protocol::AgentHostOptions::embedded_defaults()
            },
            tools: tickloop_agent_runtime::ToolPolicy::local(),
            host_rules: Vec::new(),
            task_tools: None,
            min_schedule_interval: tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
            waits: tickloop_agent_runtime::WaitHosting::Process,
            system_prompt_addition: None,
            request_gates: Vec::new(),
        },
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Every hour, summarize the news.".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { .. } => panic!("nobody can be asked"),
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 1);
    assert_eq!(agent.recent_denials(chat).await.unwrap().records.len(), 1);
    agent.shutdown().await;
}

/// A one-off schedule whose time passed while it waited is not armed by a late confirmation.
#[tokio::test]
async fn a_one_off_schedule_whose_time_passed_cannot_be_confirmed() {
    let soon = tickloop_agent_base::timestamp::format_timestamp(now_ms() + 1_500);
    let model = Script::new(vec![
        Step::Reply(call(
            "goal",
            tickloop_agent_tools::START_GOAL_TOOL_NAME,
            json!({"goal": "Summarize the news.", "schedule": format!("at {soon}")}),
        )),
        Step::Reply(text("registered")),
    ]);
    let agent = durable(Arc::new(InMemoryTranscriptStore::new()), model);
    let (chat, goal) = chat_with_scheduled_goal(&agent).await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let refused = agent
        .decide_schedule(chat, goal, ScheduleDecision::Confirm)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("no future run"), "{refused}");
    assert!(trigger_of(&agent, chat, goal).await.awaits_confirmation());
    agent
        .decide_schedule(chat, goal, ScheduleDecision::Reject)
        .await
        .unwrap();
    agent.shutdown().await;
}
