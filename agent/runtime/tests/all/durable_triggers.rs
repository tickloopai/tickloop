//! A Trigger's schedule kept by a durable host: the Transcript holds when the Trigger was armed
//! and the latest scheduled time its Loop dealt with, the host's scheduler wakes the family at
//! the `wake_at` a quiet holding reported, and the due time it gives decides what is due. Each
//! scheduled time fires at most once across instances, and the times missed in between are
//! skipped.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, LoopId, LoopState, PromptPolicy, QuestionAnswer, Recurrence,
    Trigger, TriggerPromptSource,
};
use tickloop_agent_runtime::transcript::{StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, QuestionReply, Quiescence,
    ToolPolicy, TurnOp, TurnRequest, WaitHosting,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, single_question, text};
use tickloop_agent_transcript::{InputOp, Line, SettledOutcome, input_ids};
use uuid::Uuid;

use super::durable_queue::{Script, Step, WAIT, lost_holder, prompt, settled};

const HOUR_MS: u64 = 3_600_000;
const MINUTE_MS: u64 = 60_000;
const CHECK: &str = "scheduled check";

fn harness(store: Arc<dyn TranscriptStore>, model: Arc<Script>, floor: Duration) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            model: config(),
            settings: None,
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                goals: true,
                questions: true,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: Vec::new(),
            min_schedule_interval: floor,
            waits: WaitHosting::Durable,
        },
    )
    .unwrap()
}

fn durable(store: Arc<dyn TranscriptStore>, model: Arc<Script>) -> Agent {
    harness(
        store,
        model,
        tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
    )
}

fn every(text: &str) -> Trigger {
    Trigger {
        id: Uuid::new_v4(),
        prompt: CHECK.into(),
        prompt_source: TriggerPromptSource::Explicit,
        schedule: Some(Recurrence::parse(text).unwrap()),
        expires_at_ms: None,
        accepts_events: false,
        armed: true,
        armed_at_ms: None,
        last_fired_at_ms: None,
        subscriptions: Vec::new(),
    }
}

fn milliseconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn time(milliseconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(milliseconds)
}

/// A Chat whose first goal has completed, and that goal's Loop.
async fn chat_with_goal(agent: &Agent) -> (ChatId, LoopId) {
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("watch the feed", ParentSelection::CurrentLeaf);
    let turn = agent.advance(chat, request.clone()).await.unwrap();
    assert!(matches!(
        settled(agent, chat, request.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    (chat, turn.receipt().loop_id)
}

async fn trigger_of(agent: &Agent, chat: ChatId, loop_id: LoopId) -> Trigger {
    agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .into_iter()
        .find(|goal| goal.id == loop_id)
        .unwrap()
        .trigger
        .unwrap()
}

async fn loop_state(agent: &Agent, chat: ChatId, loop_id: LoopId) -> LoopState {
    agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .into_iter()
        .find(|goal| goal.id == loop_id)
        .unwrap()
        .state
}

async fn wake_at(agent: &Agent, chat: ChatId) -> Option<u64> {
    match agent.quiesce(chat).await {
        Quiescence::Waiting(waits) => waits.wake_at.map(milliseconds),
        Quiescence::Idle => None,
    }
}

/// The Trigger fires the store's ledger holds, by input id.
fn fires(lines: &[Line]) -> HashSet<Uuid> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Input(entry) if entry.op == InputOp::Trigger => Some(entry.input_id),
            _ => None,
        })
        .collect()
}

/// The store's Lines as if the Trigger had been armed, and had dealt with its times, `by`
/// earlier: as much time passes for the schedule while no host holds the Chat.
fn earlier_by(
    store: &InMemoryTranscriptStore,
    chat: ChatId,
    by: u64,
) -> Arc<InMemoryTranscriptStore> {
    fn shift(value: &mut Value, by: u64) {
        match value {
            Value::Object(object) => {
                for (key, field) in object.iter_mut() {
                    match (key.as_str(), field.as_u64()) {
                        ("armed_at_ms" | "last_fired_at_ms", Some(at)) => *field = json!(at - by),
                        _ => shift(field, by),
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| shift(item, by)),
            _ => {}
        }
    }
    let lines = store
        .lines(chat)
        .into_iter()
        .map(|line| {
            let mut value = serde_json::to_value(&line).unwrap();
            shift(&mut value, by);
            serde_json::from_value(value).unwrap()
        })
        .collect();
    let shifted = Arc::new(InMemoryTranscriptStore::new());
    shifted.seed(chat, lines, StorePosition(1));
    shifted
}

/// How many model requests the Trigger's fires made: each carries its prompt once, and a second
/// fire's request would carry it twice.
fn checks(model: &Script) -> usize {
    model
        .requests()
        .iter()
        .filter(|request| request.contains(CHECK))
        .count()
}

async fn until_settled(agent: &Agent, chat: ChatId, input: Uuid) -> SettledOutcome {
    tokio::time::timeout(WAIT, settled(agent, chat, input))
        .await
        .expect("the fire settles")
}

/// The first holding reports when the Trigger is next due; the scheduler wakes the family then,
/// the fire is queued behind a run that holds the Chat, and the holder is lost before the fire
/// starts. The next holding, woken for the same time or for the next one, runs that fire and
/// fires nothing more: a time due again is the same input, and one due while a fire waits in
/// the queue merges into it.
#[tokio::test]
async fn a_due_time_fires_once_across_instances_and_merges_into_a_queued_fire() {
    for wakes_for_the_next_time in [false, true] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let model = Script::new(vec![Step::Reply(text("watching")), Step::Hang]);
        let first = durable(store.clone(), model.clone());
        let (chat, loop_id) = chat_with_goal(&first).await;
        first
            .configure_trigger(chat, loop_id, Some(every("every 1h")))
            .await
            .unwrap();
        let armed_at = trigger_of(&first, chat, loop_id).await.armed_at_ms.unwrap();
        let due = wake_at(&first, chat).await.unwrap();
        assert_eq!(due, armed_at + HOUR_MS);

        first
            .advance(chat, prompt("a long task", ParentSelection::CurrentLeaf))
            .await
            .unwrap();
        model.until_holding().await;
        // The host's clock says it is due; this node's clock is an hour behind.
        first.wake(chat, Some(time(due))).await.unwrap();
        let fire = input_ids::trigger(loop_id, due);
        let store = lost_holder(&first, &store, chat).await;
        assert_eq!(fires(&store.lines(chat)), HashSet::from([fire]));

        let model = Script::new(Vec::new());
        let next = durable(store.clone(), model.clone());
        let woken_for = match wakes_for_the_next_time {
            true => due + HOUR_MS,
            false => due,
        };
        next.wake(chat, Some(time(woken_for))).await.unwrap();
        assert!(matches!(
            until_settled(&next, chat, fire).await,
            SettledOutcome::Completed { .. }
        ));
        next.wake(chat, Some(time(woken_for))).await.unwrap();
        assert_eq!(wake_at(&next, chat).await, Some(woken_for + HOUR_MS));
        next.sync().await.unwrap();
        assert_eq!(fires(&store.lines(chat)), HashSet::from([fire]));
        assert_eq!(checks(&model), 1, "{:?}", model.requests());
        let trigger = trigger_of(&next, chat, loop_id).await;
        assert_eq!(trigger.last_fired_at_ms, Some(woken_for));
        next.shutdown().await;
        first.shutdown().await;
    }
}

/// A takeover that finds nothing due, and the scheduler waking the family for a time from
/// before, fire nothing: a Loop whose `every` schedule never fired keeps counting from when it
/// was armed, so its time does not slide away with each wake.
#[tokio::test]
async fn an_every_schedule_that_never_fired_counts_from_when_it_was_armed() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(store.clone(), Script::new(Vec::new()));
    let (chat, loop_id) = chat_with_goal(&first).await;
    first
        .configure_trigger(chat, loop_id, Some(every("every 1h")))
        .await
        .unwrap();
    let armed_at = trigger_of(&first, chat, loop_id).await.armed_at_ms.unwrap();
    first.sync().await.unwrap();
    first.shutdown().await;

    let model = Script::new(Vec::new());
    for due_at in [None, Some(time(armed_at + HOUR_MS - 1))] {
        let early = durable(store.clone(), model.clone());
        early.wake(chat, due_at).await.unwrap();
        assert_eq!(wake_at(&early, chat).await, Some(armed_at + HOUR_MS));
        assert_eq!(
            loop_state(&early, chat, loop_id).await,
            LoopState::WaitingTrigger
        );
        early.sync().await.unwrap();
        early.shutdown().await;
    }
    assert_eq!(checks(&model), 0);
    assert!(fires(&store.lines(chat)).is_empty());

    let on_time = durable(store.clone(), model.clone());
    on_time
        .wake(chat, Some(time(armed_at + HOUR_MS)))
        .await
        .unwrap();
    let fire = input_ids::trigger(loop_id, armed_at + HOUR_MS);
    assert!(matches!(
        until_settled(&on_time, chat, fire).await,
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(checks(&model), 1);
    on_time.shutdown().await;
}

/// Hours pass with no host holding the Chat, and the scheduler's wake comes with a time from
/// long before this node's clock: the Trigger fires once, for the latest time due, and its
/// schedule goes on from there.
#[tokio::test]
async fn hours_of_missed_times_fire_once_for_the_latest_one() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(store.clone(), Script::new(Vec::new()));
    let (chat, loop_id) = chat_with_goal(&first).await;
    first
        .configure_trigger(chat, loop_id, Some(every("every 1m")))
        .await
        .unwrap();
    let late = 3 * HOUR_MS + 30_000;
    let armed_at = trigger_of(&first, chat, loop_id).await.armed_at_ms.unwrap() - late;
    first.sync().await.unwrap();
    first.shutdown().await;
    let store = earlier_by(&store, chat, late);

    let model = Script::new(Vec::new());
    let next = durable(store.clone(), model.clone());
    // The row's time is the first one missed, three hours ago.
    next.wake(chat, Some(time(armed_at + MINUTE_MS)))
        .await
        .unwrap();
    let latest = armed_at + 180 * MINUTE_MS;
    let fire = input_ids::trigger(loop_id, latest);
    assert!(matches!(
        until_settled(&next, chat, fire).await,
        SettledOutcome::Completed { .. }
    ));
    next.sync().await.unwrap();
    assert_eq!(fires(&store.lines(chat)), HashSet::from([fire]));
    assert_eq!(checks(&model), 1);
    assert_eq!(
        trigger_of(&next, chat, loop_id).await.last_fired_at_ms,
        Some(latest)
    );
    assert_eq!(wake_at(&next, chat).await, Some(latest + MINUTE_MS));
    next.shutdown().await;
}

fn continue_loop(loop_id: LoopId, text: &str) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            target_loop: Some(loop_id),
            message: tickloop_agent_runtime::UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

fn answer(request: Uuid) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Answer {
            request,
            reply: QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")]),
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

fn asks() -> Step {
    Step::Reply(call(
        "ask",
        "AskUserQuestion",
        single_question("Which report?"),
    ))
}

/// The Loop of `chat_with_goal` asks a question, and the Chat waits for the answer.
async fn ask_on(agent: &Agent, chat: ChatId, loop_id: LoopId) -> Uuid {
    agent
        .advance(chat, continue_loop(loop_id, "check now"))
        .await
        .unwrap();
    match agent.quiesce(chat).await {
        Quiescence::Waiting(waits) => waits.people[0].request,
        Quiescence::Idle => panic!("the Loop waits for its answer"),
    }
}

/// A Loop that waits on a question for five hours cannot take its Trigger's fires: each time
/// that came due is dealt with as refused, so answering runs no catch-up fire, and the schedule
/// goes on from the latest time.
#[tokio::test]
async fn a_loop_waiting_on_a_question_for_hours_runs_no_catch_up_fire() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(
        store.clone(),
        Script::new(vec![Step::Reply(text("watching")), asks()]),
    );
    let (chat, loop_id) = chat_with_goal(&first).await;
    first
        .configure_trigger(chat, loop_id, Some(every("every 1h")))
        .await
        .unwrap();
    let request = ask_on(&first, chat, loop_id).await;
    first.sync().await.unwrap();
    first.shutdown().await;
    let store = earlier_by(&store, chat, 5 * HOUR_MS);

    let model = Script::new(Vec::new());
    let next = durable(store.clone(), model.clone());
    next.wake(chat, None).await.unwrap();
    let trigger = trigger_of(&next, chat, loop_id).await;
    let latest = trigger.armed_at_ms.unwrap() + 5 * HOUR_MS;
    assert_eq!(trigger.last_fired_at_ms, Some(latest));
    match next.quiesce(chat).await {
        Quiescence::Waiting(waits) => {
            assert_eq!(waits.people[0].request, request);
            assert_eq!(waits.wake_at, None, "a Loop that cannot fire is not due");
        }
        Quiescence::Idle => panic!("the question still waits"),
    }
    next.advance(chat, answer(request)).await.unwrap();
    assert_eq!(wake_at(&next, chat).await, Some(latest + HOUR_MS));
    assert_eq!(
        loop_state(&next, chat, loop_id).await,
        LoopState::WaitingTrigger
    );
    next.sync().await.unwrap();
    assert!(fires(&store.lines(chat)).is_empty());
    assert_eq!(checks(&model), 0, "{:?}", model.requests());
    next.shutdown().await;
}

/// A deadline is due whatever its Loop's state: one waiting on a question reports it, and the
/// scheduler's wake for it ends the Loop even on a node whose clock has not got there yet.
#[tokio::test]
async fn a_loop_waiting_on_a_question_expires_when_its_deadline_comes() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = durable(
        store.clone(),
        Script::new(vec![Step::Reply(text("watching")), asks()]),
    );
    let (chat, loop_id) = chat_with_goal(&first).await;
    let deadline = milliseconds(SystemTime::now()) + HOUR_MS;
    first
        .configure_trigger(
            chat,
            loop_id,
            Some(Trigger {
                schedule: None,
                accepts_events: true,
                expires_at_ms: Some(deadline),
                ..every("every 1h")
            }),
        )
        .await
        .unwrap();
    ask_on(&first, chat, loop_id).await;
    assert_eq!(wake_at(&first, chat).await, Some(deadline));
    first.sync().await.unwrap();
    first.shutdown().await;

    let next = durable(store.clone(), Script::new(Vec::new()));
    next.wake(chat, Some(time(deadline))).await.unwrap();
    assert_eq!(loop_state(&next, chat, loop_id).await, LoopState::Expired);
    assert_eq!(next.quiesce(chat).await, Quiescence::Idle);
    next.shutdown().await;
}

fn start_goal(schedule: &str) -> Step {
    Step::Reply(call(
        "goal",
        tickloop_agent_tools::START_GOAL_TOOL_NAME,
        json!({"goal": "Summarize the news.", "schedule": schedule}),
    ))
}

/// StartGoal with a schedule registers a Loop that waits for its first time instead of running
/// now; the scheduler's wake at that time runs it on another instance.
#[tokio::test]
async fn start_goal_with_a_schedule_waits_for_its_first_time() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![
        start_goal("every 1h"),
        Step::Reply(text("registered")),
    ]);
    let first = durable(store.clone(), model.clone());
    let chat = first.create_chat_without_folder().await.unwrap();
    let request = prompt(
        "Every hour, summarize the news.",
        ParentSelection::CurrentLeaf,
    );
    first.advance(chat, request.clone()).await.unwrap();
    until_settled(&first, chat, request.input_id).await;
    let snapshot = first.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 2);
    let scheduled = &snapshot.loops[1];
    assert_eq!(scheduled.state, LoopState::WaitingTrigger);
    assert_eq!(scheduled.run_id, None);
    let trigger = scheduled.trigger.clone().unwrap();
    assert!(trigger.armed);
    assert_eq!(trigger.prompt, "Summarize the news.");
    assert_eq!(trigger.schedule.unwrap().to_string(), "every 1h");
    let first_time = trigger.armed_at_ms.unwrap() + HOUR_MS;
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].contains(r#"\"state\":\"scheduled\""#),
        "{}",
        requests[1]
    );
    assert!(requests[1].contains(r#"\"schedule\":\"every 1h\""#));
    assert_eq!(wake_at(&first, chat).await, Some(first_time));
    first.sync().await.unwrap();
    first.shutdown().await;

    let model = Script::new(Vec::new());
    let next = durable(store.clone(), model.clone());
    next.wake(chat, Some(time(first_time))).await.unwrap();
    let fire = input_ids::trigger(scheduled.id, first_time);
    assert!(matches!(
        until_settled(&next, chat, fire).await,
        SettledOutcome::Completed { .. }
    ));
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("Summarize the news."));
    next.shutdown().await;
}

/// A schedule that runs more often than the host allows is refused, to StartGoal and to a
/// configured Trigger alike, and the refusal names the floor.
#[tokio::test]
async fn a_schedule_below_the_hosts_floor_is_refused() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![start_goal("every 30s"), Step::Reply(text("noted"))]);
    let agent = harness(store, model.clone(), Duration::from_secs(60));
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("Poll every 30 seconds.", ParentSelection::CurrentLeaf);
    let turn = agent.advance(chat, request.clone()).await.unwrap();
    until_settled(&agent, chat, request.input_id).await;
    let requests = model.requests();
    assert!(
        requests[1].contains("runs must be at least 1 minute apart"),
        "{}",
        requests[1]
    );
    assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 1);
    let loop_id = turn.receipt().loop_id;
    let refused = agent
        .configure_trigger(chat, loop_id, Some(every("every 30s")))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("1 minute"), "{refused}");
    agent
        .configure_trigger(chat, loop_id, Some(every("daily 09:00")))
        .await
        .unwrap();
    agent
        .configure_trigger(chat, loop_id, Some(every("every 1m")))
        .await
        .unwrap();
    agent.shutdown().await;
}

/// The host is lost after StartGoal registered its Loop and before the call's result was
/// committed: the next holder settles the call by the Loop it finds, so the goal is neither
/// lost nor registered twice.
#[tokio::test]
async fn a_start_goal_whose_result_was_lost_is_settled_by_the_loop_it_registered() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![start_goal("every 1h"), Step::Hang]);
    let first = durable(store.clone(), model.clone());
    let chat = first.create_chat_without_folder().await.unwrap();
    first
        .advance(
            chat,
            prompt(
                "Every hour, summarize the news.",
                ParentSelection::CurrentLeaf,
            ),
        )
        .await
        .unwrap();
    model.until_holding().await;
    first.sync().await.unwrap();
    let lines = store.lines(chat);
    let result_at = lines
        .iter()
        .position(|line| {
            matches!(line, Line::User(entry) if entry.message.content.iter().any(|content| {
                matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == "goal")
            }))
        })
        .expect("the call's result is on the chain");
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, lines[..result_at].to_vec(), StorePosition(1));

    let model = Script::new(vec![Step::Reply(text("registered"))]);
    let next = durable(crashed.clone(), model.clone());
    next.wake(chat, None).await.unwrap();
    assert!(wake_at(&next, chat).await.is_some());
    let snapshot = next.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 2, "{:?}", snapshot.loops);
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains(r#"\"state\":\"scheduled\""#),
        "{}",
        requests[0]
    );
    next.shutdown().await;
    first.shutdown().await;
}
