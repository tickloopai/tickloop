//! Scheduled tasks a host manages for the user: it creates a task with its own id, title, zone
//! and end in a Chat without messages, finds every task by that id to pause, resume, edit,
//! delete, confirm or reject it, and hears each task's runs and removal under the same id. The
//! host sets how long tasks live, how often a recurring one may run and whether the Agent's tasks
//! need confirming, and the tools tell the model the same.

use std::sync::Arc;
use std::time::Duration;
use tickloop_code_settings::local_agent::LocalAgent;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use tickloop_harness_model::ModelClient;
use tickloop_harness_protocol::{
    AgentHostOptions, ChatId, EventKind, PromptPolicy, ScheduledTask, ScheduledTaskConfirmation,
    ScheduledTaskCreator, ScheduledTaskDecision, ScheduledTaskNotice, ScheduledTaskRemovalReason,
    TriggerIdentity,
};
use tickloop_harness_runtime::transcript::TranscriptStore;
use tickloop_harness_runtime::{
    Agent, AgentError, HarnessOptions, HarnessSeams, NewScheduledTask, ScheduledTaskEdit,
    ToolPolicy, WaitHosting,
};
use tickloop_harness_test_support::harness::ChosenModels;
use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_harness_test_support::{ScriptedModel, call, config, text};
use tickloop_harness_transcript::Line;
use uuid::Uuid;

use super::wakeup_support::{ContractModel, complete_turn, hold_a_turn};

const DIGEST: &str = "Summarize the world news of the last day.";
const HOUR: Duration = Duration::from_secs(60 * 60);

/// 2026-10-<day> at the given UTC time.
fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, minute, 0)
        .unwrap()
}

fn ms(time: DateTime<Utc>) -> u64 {
    u64::try_from(time.timestamp_millis()).unwrap()
}

/// A stateless host that denies whatever would prompt and admits the scheduled task tools by its
/// own rules, on the clock at 2026-10-23 10:00 UTC, with `options` on top.
fn host(
    store: Arc<dyn TranscriptStore>,
    model: Arc<dyn ModelClient>,
    options: impl FnOnce(&mut HarnessOptions),
) -> Agent {
    let mut harness = HarnessOptions {
        host: AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::product_defaults()
        },
        tools: ToolPolicy {
            scheduled_tasks: true,
            ..ToolPolicy::web_fetch_only()
        },
        host_rules: tickloop_harness_runtime::durable_chat_host_rules(),
        waits: WaitHosting::Durable,
        time_zone: Some("UTC".into()),
        ..HarnessOptions::local(config())
    };
    options(&mut harness);
    let agent = Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        harness,
    )
    .unwrap();
    agent.set_scheduled_now_for_tests(at(23, 10, 0));
    agent
}

fn plain_host() -> Agent {
    host(
        Arc::new(InMemoryTranscriptStore::new()),
        ContractModel::new(),
        |_| {},
    )
}

fn daily_digest() -> NewScheduledTask {
    NewScheduledTask {
        title: Some("  Daily digest ".into()),
        ..NewScheduledTask::new("7 9 * * *", DIGEST, true)
    }
}

async fn tasks(agent: &Agent, chat: ChatId) -> Vec<ScheduledTask> {
    agent.snapshot(chat).await.unwrap().scheduled_tasks
}

fn is_invalid<T: std::fmt::Debug>(result: Result<T, AgentError>, words: &str) -> bool {
    matches!(&result, Err(AgentError::Invalid(message)) if message.contains(words))
}

/// The scheduled task notices the Chat announces while `during` runs.
async fn notices_during<F: std::future::Future>(
    agent: &Agent,
    during: F,
) -> (F::Output, Vec<ScheduledTaskNotice>) {
    let mut events = agent.subscribe();
    let output = during.await;
    let mut notices = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await
    {
        if let EventKind::ScheduledTasksChanged {
            notice: Some(notice),
        } = event.kind
        {
            notices.push(notice);
        }
    }
    (output, notices)
}

fn removed(task_id: Uuid, reason: ScheduledTaskRemovalReason) -> ScheduledTaskNotice {
    ScheduledTaskNotice::Removed { task_id, reason }
}

/// The CronCreate definition a model was offered last.
fn cron_create_description(model: &ScriptedModel) -> String {
    model
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .tools
        .iter()
        .find(|tool| tool.name == "CronCreate")
        .unwrap()
        .description
        .clone()
}

/// A task the host creates in a Chat without messages keeps the host's id, title, zone and end,
/// waits for its first time without asking anyone, and outlives the host: another one loads
/// the Chat with only its task and runs it at that time. Creating it again returns it as it is;
/// a task created without an id gets a UUIDv7.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_created_task_keeps_its_id_and_outlives_the_host() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = host(store.clone(), ContractModel::new(), |_| {});
    let chat = first.create_chat_without_folder().await.unwrap();
    let id = Uuid::now_v7();
    let ends_at_ms = ms(at(30, 0, 0));
    let task = first
        .schedule_task(
            chat,
            NewScheduledTask {
                uuid: Some(id),
                time_zone: Some("Asia/Shanghai".into()),
                ends_at_ms: Some(ends_at_ms),
                ..daily_digest()
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, id);
    assert_eq!(task.title.as_deref(), Some("Daily digest"));
    assert_eq!(task.time_zone, "Asia/Shanghai");
    assert_eq!(task.ends_at_ms, Some(ends_at_ms));
    assert_eq!(task.creator, ScheduledTaskCreator::User);
    assert_eq!(task.confirmation, None);
    // 09:07 in Shanghai is 01:07 UTC; daily runs are late by up to 30 minutes.
    let first_time = ms(at(24, 1, 7));
    assert_eq!(task.scheduled_for_ms, Some(first_time));
    assert!(task.is_timing());

    let retried = first
        .schedule_task(
            chat,
            NewScheduledTask {
                uuid: Some(id),
                ..NewScheduledTask::new("*/5 * * * *", "something else", false)
            },
        )
        .await
        .unwrap();
    assert_eq!(retried, task, "a retry returns the task as it is");
    assert_eq!(tasks(&first, chat).await, std::slice::from_ref(&task));
    let generated = first
        .schedule_task(
            chat,
            NewScheduledTask::new("0 12 31 12 *", "year end", true),
        )
        .await
        .unwrap();
    assert_eq!(generated.id.get_version_num(), 7);
    assert_eq!(tasks(&first, chat).await.len(), 2);
    first.sync().await.unwrap();
    first.shutdown().await;

    let model = ContractModel::new();
    let next = host(store.clone(), model.clone(), |_| {});
    next.resume_chat(chat).await.unwrap();
    let loaded = tasks(&next, chat).await;
    assert_eq!(loaded.len(), 2, "the Chat without messages keeps its tasks");
    assert_eq!(loaded[0], task);
    let handed = next
        .wake_scheduled_tasks(chat, task.next_fire_at_ms.unwrap())
        .await
        .unwrap();
    assert_eq!(handed.len(), 1);
    model.wait_seen(DIGEST, 1).await;
    next.shutdown().await;
}

/// One id names the task from its creation through its run, the run's Trigger and ledger
/// record, to the notice of its removal.
#[tokio::test(flavor = "multi_thread")]
async fn one_id_names_the_task_from_creation_through_its_run_to_its_removal() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = ContractModel::new();
    let agent = host(store.clone(), model.clone(), |_| {});
    let chat = agent.create_chat_without_folder().await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    let task = agent
        .schedule_task(
            chat,
            NewScheduledTask::new("13 10 * * *", "check the build", false),
        )
        .await
        .unwrap();
    let due_at = task.next_fire_at_ms.unwrap();
    let (handed, notices) = notices_during(&agent, agent.wake_scheduled_tasks(chat, due_at)).await;
    assert_eq!(handed.unwrap().len(), 1);
    model.wait_seen("check the build", 1).await;
    assert_eq!(
        notices,
        [
            ScheduledTaskNotice::Fired { task_id: task.id },
            removed(task.id, ScheduledTaskRemovalReason::OneShotCompleted)
        ]
    );
    assert!(tasks(&agent, chat).await.is_empty());
    agent.sync().await.unwrap();
    let deliveries = tickloop_harness_transcript::display::trigger_deliveries(&store.lines(chat));
    assert!(
        deliveries.iter().any(|delivery| matches!(
            &delivery.identity,
            TriggerIdentity::ScheduledTask { task_id, scheduled_for_ms, .. }
                if *task_id == task.id && Some(*scheduled_for_ms) == task.scheduled_for_ms
        )),
        "{deliveries:?}"
    );
    agent.shutdown().await;
}

/// A task the host creates is refused when it cannot run: no prompt, a schedule without a
/// future time or one before its end, a blank or long title, an unknown zone, or the id of a
/// task the Chat deleted. A Chat the host has not loaded is not found.
#[tokio::test]
async fn a_host_created_task_that_cannot_run_is_refused() {
    let agent = plain_host();
    let chat = agent.create_chat_without_folder().await.unwrap();
    let deleted = agent.schedule_task(chat, daily_digest()).await.unwrap();
    agent.delete_scheduled_task(chat, deleted.id).await.unwrap();
    let cases = [
        (
            NewScheduledTask {
                prompt: " ".into(),
                ..daily_digest()
            },
            "needs a prompt",
        ),
        (
            NewScheduledTask::new("0 0 30 2 *", DIGEST, false),
            "names no time from now on",
        ),
        (
            NewScheduledTask {
                ends_at_ms: Some(ms(at(24, 0, 0))),
                ..daily_digest()
            },
            "names no time before the task ends",
        ),
        (
            NewScheduledTask {
                title: Some("  ".into()),
                ..daily_digest()
            },
            "a title must contain",
        ),
        (
            NewScheduledTask {
                title: Some(
                    "x".repeat(tickloop_harness_protocol::MAX_SCHEDULED_TASK_TITLE_CHARS + 1),
                ),
                ..daily_digest()
            },
            "a title must contain",
        ),
        (
            NewScheduledTask {
                time_zone: Some("Mars/Olympus".into()),
                ..daily_digest()
            },
            "is not an IANA time zone",
        ),
        (
            NewScheduledTask {
                uuid: Some(deleted.id),
                ..daily_digest()
            },
            "was deleted",
        ),
    ];
    for (task, words) in cases {
        assert!(
            is_invalid(agent.schedule_task(chat, task).await, words),
            "{words}"
        );
    }
    assert!(tasks(&agent, chat).await.is_empty());
    assert!(matches!(
        agent
            .schedule_task(tickloop_ids::Id::from(Uuid::new_v4()), daily_digest())
            .await,
        Err(AgentError::NotFound)
    ));
    agent.shutdown().await;
}

/// Pausing, resuming, deleting, confirming and rejecting find the task by its id; a pause holds
/// across hosts and needs no wake; an id the Chat lacks is a missing task, not a missing Chat.
#[tokio::test(flavor = "multi_thread")]
async fn every_host_action_finds_the_task_by_its_id() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = host(store.clone(), ContractModel::new(), |_| {});
    let chat = first.create_chat_without_folder().await.unwrap();
    let task = first.schedule_task(chat, daily_digest()).await.unwrap();
    first
        .set_scheduled_task_paused(chat, task.id, true)
        .await
        .unwrap();
    first
        .set_scheduled_task_paused(chat, task.id, true)
        .await
        .unwrap();
    let paused = tasks(&first, chat).await.remove(0);
    assert!(paused.paused_at_ms.is_some());
    assert_eq!(
        paused.confirmation, None,
        "paused is not awaiting confirmation"
    );
    assert_eq!(
        first.quiesce(chat).await,
        tickloop_harness_runtime::Quiescence::Idle
    );
    first.sync().await.unwrap();
    first.shutdown().await;

    let next = host(store, ContractModel::new(), |_| {});
    next.resume_chat(chat).await.unwrap();
    assert!(tasks(&next, chat).await[0].paused_at_ms.is_some());
    next.set_scheduled_now_for_tests(at(24, 12, 0));
    next.set_scheduled_task_paused(chat, task.id, false)
        .await
        .unwrap();
    let resumed = tasks(&next, chat).await.remove(0);
    assert_eq!(resumed.paused_at_ms, None);
    assert_eq!(
        resumed.scheduled_for_ms,
        Some(ms(at(25, 9, 7))),
        "timed from now"
    );

    let (deleted, notices) = notices_during(&next, next.delete_scheduled_task(chat, task.id)).await;
    deleted.unwrap();
    assert_eq!(
        notices,
        [removed(task.id, ScheduledTaskRemovalReason::UserDeleted)]
    );
    let missing = task.id;
    assert!(matches!(
        next.set_scheduled_task_paused(chat, missing, true).await,
        Err(AgentError::ScheduledTaskNotFound)
    ));
    assert!(matches!(
        next.set_scheduled_task_paused(chat, missing, false).await,
        Err(AgentError::ScheduledTaskNotFound)
    ));
    assert!(matches!(
        next.delete_scheduled_task(chat, missing).await,
        Err(AgentError::ScheduledTaskNotFound)
    ));
    assert!(matches!(
        next.decide_scheduled_task(chat, missing, ScheduledTaskDecision::Confirm)
            .await,
        Err(AgentError::ScheduledTaskNotFound)
    ));
    assert!(matches!(
        next.edit_scheduled_task(chat, missing, ScheduledTaskEdit::default())
            .await,
        Err(AgentError::ScheduledTaskNotFound)
    ));
    assert!(matches!(
        next.delete_scheduled_task(tickloop_ids::Id::from(Uuid::new_v4()), missing)
            .await,
        Err(AgentError::NotFound)
    ));
    next.shutdown().await;
}

/// A new title or prompt leaves the task's times alone; a new schedule, recurrence, zone or end
/// times it from now, keeps a paused task paused and withdraws the run it had queued. The id
/// never changes, and a title or an end can be removed.
#[tokio::test(flavor = "multi_thread")]
async fn editing_a_task_keeps_its_id_and_retimes_it_only_for_a_new_schedule() {
    let model = ContractModel::new();
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |_| {},
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let task = agent
        .schedule_task(
            chat,
            NewScheduledTask {
                ends_at_ms: Some(ms(at(30, 0, 0))),
                ..daily_digest()
            },
        )
        .await
        .unwrap();
    let renamed = agent
        .edit_scheduled_task(
            chat,
            task.id,
            ScheduledTaskEdit {
                title: Some(Some("News".into())),
                prompt: Some("Only the headlines.".into()),
                ..ScheduledTaskEdit::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.id, task.id);
    assert_eq!(renamed.title.as_deref(), Some("News"));
    assert_eq!(renamed.prompt, "Only the headlines.");
    assert_eq!(renamed.next_fire_at_ms, task.next_fire_at_ms);

    let rescheduled = agent
        .edit_scheduled_task(
            chat,
            task.id,
            ScheduledTaskEdit {
                cron: Some("37 15 * * *".into()),
                time_zone: Some("Asia/Shanghai".into()),
                title: Some(None),
                ends_at_ms: Some(None),
                ..ScheduledTaskEdit::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rescheduled.id, task.id);
    assert_eq!(rescheduled.time_zone, "Asia/Shanghai");
    // 15:37 in Shanghai is 07:37 UTC, the next one after 10:00 UTC on the 23rd.
    assert_eq!(rescheduled.scheduled_for_ms, Some(ms(at(24, 7, 37))));
    assert_eq!(rescheduled.title, None);
    assert_eq!(rescheduled.ends_at_ms, None);
    assert_eq!(rescheduled.created_at_ms, task.created_at_ms);

    agent
        .set_scheduled_task_paused(chat, task.id, true)
        .await
        .unwrap();
    let once = agent
        .edit_scheduled_task(
            chat,
            task.id,
            ScheduledTaskEdit {
                recurring: Some(false),
                ..ScheduledTaskEdit::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        once.kind,
        tickloop_harness_protocol::ScheduledTaskKind::OneShot { .. }
    ));
    assert_eq!(once.expires_at_ms, None);
    assert!(once.paused_at_ms.is_some(), "a paused task stays paused");
    assert!(is_invalid(
        agent
            .edit_scheduled_task(
                chat,
                task.id,
                ScheduledTaskEdit {
                    ends_at_ms: Some(Some(ms(at(23, 11, 0)))),
                    ..ScheduledTaskEdit::default()
                },
            )
            .await,
        "names no time before the task ends"
    ));
    assert!(is_invalid(
        agent
            .edit_scheduled_task(
                chat,
                task.id,
                ScheduledTaskEdit {
                    title: Some(Some(" ".into())),
                    ..ScheduledTaskEdit::default()
                },
            )
            .await,
        "a title must contain"
    ));

    // A run waiting behind a busy turn belongs to the old time: a new schedule withdraws it.
    agent
        .set_scheduled_task_paused(chat, task.id, false)
        .await
        .unwrap();
    let hourly = agent
        .edit_scheduled_task(
            chat,
            task.id,
            ScheduledTaskEdit {
                cron: Some("7 * * * *".into()),
                recurring: Some(true),
                ..ScheduledTaskEdit::default()
            },
        )
        .await
        .unwrap();
    hold_a_turn(&agent, chat, &model).await;
    agent
        .wake_scheduled_tasks(chat, hourly.next_fire_at_ms.unwrap())
        .await
        .unwrap();
    assert_eq!(agent.snapshot(chat).await.unwrap().pending_inputs.len(), 1);
    agent
        .edit_scheduled_task(
            chat,
            task.id,
            ScheduledTaskEdit {
                cron: Some("47 * * * *".into()),
                ..ScheduledTaskEdit::default()
            },
        )
        .await
        .unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .is_empty()
    );
    model.release_held_turn();
    agent.shutdown().await;
}

/// A task the Agent creates where nobody is asked waits for the user: it is confirmed or
/// rejected, not paused or edited, and rejecting it announces why it left. A host that trusts
/// the Agent's tasks starts them at once, and the tools stop saying they wait.
#[tokio::test]
async fn the_agents_task_waits_for_confirmation_unless_the_host_trusts_it() {
    for trusted in [false, true] {
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "schedule",
                "CronCreate",
                json!({"cron": "7 9 * * *", "prompt": DIGEST, "title": "Digest"}),
            ),
            text("Scheduled."),
        ]));
        let agent = host(
            Arc::new(InMemoryTranscriptStore::new()),
            model.clone(),
            |options| options.trust_model_scheduled_tasks = trusted,
        );
        let chat = agent.create_chat_without_folder().await.unwrap();
        complete_turn(&agent, chat, "Digest the news every morning.").await;
        let task = tasks(&agent, chat).await.remove(0);
        assert_eq!(task.creator, ScheduledTaskCreator::Agent);
        assert_eq!(task.title.as_deref(), Some("Digest"));
        assert_eq!(
            cron_create_description(&model).contains("waits for the user to confirm"),
            !trusted
        );
        if trusted {
            assert_eq!(task.confirmation, None);
            assert!(task.is_timing());
            agent.shutdown().await;
            continue;
        }
        assert_eq!(task.confirmation, Some(ScheduledTaskConfirmation::Pending));
        for result in [
            agent.set_scheduled_task_paused(chat, task.id, true).await,
            agent
                .edit_scheduled_task(
                    chat,
                    task.id,
                    ScheduledTaskEdit {
                        prompt: Some("Only the headlines.".into()),
                        ..ScheduledTaskEdit::default()
                    },
                )
                .await
                .map(|_| ()),
        ] {
            assert!(is_invalid(result, "waiting for confirmation"));
        }
        let (rejected, notices) = notices_during(
            &agent,
            agent.decide_scheduled_task(chat, task.id, ScheduledTaskDecision::Reject),
        )
        .await;
        rejected.unwrap();
        assert_eq!(
            notices,
            [removed(task.id, ScheduledTaskRemovalReason::Rejected)]
        );
        agent.shutdown().await;
    }
}

/// With a least interval of an hour, a recurring schedule running more often is refused when
/// the host creates or edits a task and when the Agent calls CronCreate, with the limit and a
/// schedule that keeps to it; an hourly schedule and one-shot tasks are not limited, and the
/// tools tell the model the limit.
#[tokio::test]
async fn a_least_interval_refuses_frequent_recurring_schedules_everywhere() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "often",
            "CronCreate",
            json!({"cron": "*/30 * * * *", "prompt": "check"}),
        ),
        text("Refused."),
    ]));
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |options| options.scheduled_task_min_interval = Some(HOUR),
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let too_frequent = |result: Result<ScheduledTask, AgentError>| match result {
        Err(error @ AgentError::ScheduledTaskTooFrequent { min_interval }) => {
            assert_eq!(min_interval, HOUR);
            let message = error.to_string();
            assert!(
                message.contains("at most once per hour") && message.contains("`0 * * * *`"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    };
    for cron in ["*/30 * * * *", "0,30 9 * * *"] {
        too_frequent(
            agent
                .schedule_task(chat, NewScheduledTask::new(cron, "check", true))
                .await,
        );
    }
    let hourly = agent
        .schedule_task(chat, NewScheduledTask::new("0 * * * *", "check", true))
        .await
        .unwrap();
    agent
        .schedule_task(chat, NewScheduledTask::new("30 9 * * *", "once", false))
        .await
        .unwrap();
    for cron in ["*/30 * * * *", "0,30 9 * * *"] {
        too_frequent(
            agent
                .edit_scheduled_task(
                    chat,
                    hourly.id,
                    ScheduledTaskEdit {
                        cron: Some(cron.into()),
                        ..ScheduledTaskEdit::default()
                    },
                )
                .await,
        );
    }
    complete_turn(&agent, chat, "Check every half hour.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let refusal = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            tickloop_harness_protocol::Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "often" => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .unwrap();
    assert!(
        refusal.1 && refusal.0.contains("at most once per hour"),
        "{refusal:?}"
    );
    assert_eq!(snapshot.scheduled_tasks.len(), 2);
    let description = cron_create_description(&model);
    assert!(
        description.contains("runs at most once per hour")
            && !description.contains("(every 5 minutes)")
            && description.contains("\"0 * * * *\" (hourly)"),
        "{description}"
    );
    agent.shutdown().await;
}

/// Without a lifetime a recurring task has no expiry and still runs on its eighth day, and
/// the tools no longer promise seven days; the default host keeps them.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_lifetime_a_task_runs_past_seven_days() {
    for limited in [true, false] {
        let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
        let agent = host(
            Arc::new(InMemoryTranscriptStore::new()),
            model.clone(),
            |options| {
                if !limited {
                    options.scheduled_task_expiry = None;
                }
            },
        );
        let chat = agent.create_chat_without_folder().await.unwrap();
        complete_turn(&agent, chat, "get ready").await;
        let description = cron_create_description(&model);
        assert_eq!(description.contains("7 days"), limited, "{description}");
        let task = agent
            .schedule_task(
                chat,
                NewScheduledTask::new("13 10 * * *", "daily check", true),
            )
            .await
            .unwrap();
        assert_eq!(
            task.expires_at_ms,
            limited.then(|| ms(at(30, 10, 0))),
            "seven days after creation"
        );
        if limited {
            agent.shutdown().await;
            continue;
        }
        for day in 23..=30 {
            model.enqueue(text("Checked."));
            let due_at = tasks(&agent, chat).await[0].next_fire_at_ms.unwrap();
            agent.set_scheduled_now_for_tests(at(day, 11, 0));
            let handed = agent.wake_scheduled_tasks(chat, due_at).await.unwrap();
            assert_eq!(handed.len(), 1, "day {day}");
            agent.quiesce(chat).await;
        }
        let left = tasks(&agent, chat).await;
        assert_eq!(left.len(), 1, "the task outlives seven days");
        assert_eq!(left[0].scheduled_for_ms, Some(ms(at(31, 10, 13))));
        agent.shutdown().await;
    }
}

/// When a task's end comes it is removed, even while paused, and the host is woken for it.
#[tokio::test]
async fn a_task_is_removed_when_its_end_comes() {
    let agent = plain_host();
    let chat = agent.create_chat_without_folder().await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    let ends_at_ms = ms(at(23, 12, 30));
    let task = agent
        .schedule_task(
            chat,
            NewScheduledTask {
                ends_at_ms: Some(ends_at_ms),
                ..NewScheduledTask::new("7 * * * *", "hourly check", true)
            },
        )
        .await
        .unwrap();
    agent
        .set_scheduled_task_paused(chat, task.id, true)
        .await
        .unwrap();
    let tickloop_harness_runtime::Quiescence::Waiting(waits) = agent.quiesce(chat).await else {
        panic!("the host is woken at the task's end");
    };
    assert_eq!(waits.next_wake_at_ms, Some(ends_at_ms));
    agent.set_scheduled_now_for_tests(at(23, 12, 30));
    let (handed, notices) =
        notices_during(&agent, agent.wake_scheduled_tasks(chat, ends_at_ms)).await;
    assert!(handed.unwrap().is_empty());
    assert_eq!(
        notices,
        [removed(task.id, ScheduledTaskRemovalReason::EndsAtReached)]
    );
    assert!(tasks(&agent, chat).await.is_empty());
    agent.shutdown().await;
}

/// A Transcript written before tasks had UUIDs, whose task, run records and CronDelete call
/// name the task by an eight-character id, still loads: the task reads with the same UUIDv5 on
/// every load, and the records still display.
#[tokio::test(flavor = "multi_thread")]
async fn a_transcript_with_eight_character_task_ids_still_loads() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(vec![text("Ready."), text("Ran.")]));
    let agent = host(store.clone(), model.clone(), |_| {});
    let chat = agent.create_chat_without_folder().await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    let task = agent
        .schedule_task(
            chat,
            NewScheduledTask::new("13 10 * * *", "daily check", true),
        )
        .await
        .unwrap();
    agent
        .wake_scheduled_tasks(chat, task.next_fire_at_ms.unwrap())
        .await
        .unwrap();
    agent.quiesce(chat).await;
    agent.sync().await.unwrap();
    let position = store.position(chat).unwrap();
    agent.shutdown().await;

    let legacy: Vec<Line> = store
        .lines(chat)
        .into_iter()
        .map(|line| {
            let written = serde_json::to_string(&line)
                .unwrap()
                .replace(&task.id.to_string(), "0a1b2c3d");
            serde_json::from_str(&written).unwrap()
        })
        .collect();
    assert!(
        legacy
            .iter()
            .any(|line| matches!(line, Line::ScheduledTasks(_)))
    );
    let expected = tickloop_harness_protocol::legacy_scheduled_task_id(
        chat.uuid(),
        "0a1b2c3d",
        task.created_at_ms,
    );
    for _ in 0..2 {
        let image = Arc::new(InMemoryTranscriptStore::new());
        image.seed(chat, legacy.clone(), position);
        let next = host(image, Arc::new(ScriptedModel::new(Vec::new())), |_| {});
        next.resume_chat(chat).await.unwrap();
        let loaded = tasks(&next, chat).await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, expected);
        assert_eq!(loaded[0].prompt, "daily check");
        next.shutdown().await;
    }
    let deliveries = tickloop_harness_transcript::display::trigger_deliveries(&legacy);
    assert!(
        deliveries
            .iter()
            .any(|delivery| matches!(delivery.identity, TriggerIdentity::ScheduledTask { .. })),
        "{deliveries:?}"
    );
    assert!(!tickloop_harness_transcript::display::project(&legacy).is_empty());
}

/// A Chat without messages that held only scheduled tasks stays loadable once its last task
/// leaves, whether the host deleted it or its end came: another holder loads it empty, and it
/// schedules and runs a new task, after whose one-shot run it loads again.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_without_messages_stays_after_its_last_task_leaves() {
    for ends in [false, true] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let first = host(store.clone(), ContractModel::new(), |_| {});
        let chat = first.create_chat_without_folder().await.unwrap();
        let ends_at_ms = ms(at(23, 12, 30));
        let task = first
            .schedule_task(
                chat,
                NewScheduledTask {
                    ends_at_ms: ends.then_some(ends_at_ms),
                    ..NewScheduledTask::new("7 * * * *", "hourly check", true)
                },
            )
            .await
            .unwrap();
        if ends {
            end_while_paused(&first, chat, task.id, ends_at_ms).await;
        } else {
            first.delete_scheduled_task(chat, task.id).await.unwrap();
        }
        assert!(tasks(&first, chat).await.is_empty(), "ends: {ends}");
        first.sync().await.unwrap();
        first.shutdown().await;

        let model = ContractModel::new();
        let next = host(store.clone(), model.clone(), |_| {});
        let loaded = next.resume_chat(chat).await.unwrap();
        assert!(loaded.scheduled_tasks.is_empty(), "ends: {ends}");
        assert!(loaded.messages.is_empty(), "ends: {ends}");
        let again = next
            .schedule_task(
                chat,
                NewScheduledTask::new("13 10 * * *", "check again", false),
            )
            .await
            .unwrap();
        let handed = next
            .wake_scheduled_tasks(chat, again.next_fire_at_ms.unwrap())
            .await
            .unwrap();
        assert_eq!(handed.len(), 1, "ends: {ends}");
        model.wait_seen("check again", 1).await;
        next.quiesce(chat).await;
        assert!(tasks(&next, chat).await.is_empty());
        next.sync().await.unwrap();
        next.shutdown().await;

        let last = host(store, ContractModel::new(), |_| {});
        let reloaded = last.resume_chat(chat).await.unwrap();
        assert!(reloaded.scheduled_tasks.is_empty());
        assert!(
            !reloaded.messages.is_empty(),
            "the one-shot run's turn stays"
        );
        last.shutdown().await;
    }
}

/// A Chat with a Folder and no messages keeps its Folder after its last task is deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_with_a_folder_and_no_messages_keeps_its_folder_without_tasks() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let holder = Agent::open_embedded(root.clone(), config(), ContractModel::new()).unwrap();
    holder.set_scheduled_now_for_tests(at(23, 10, 0));
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    let task = holder.schedule_task(chat, daily_digest()).await.unwrap();
    holder.delete_scheduled_task(chat, task.id).await.unwrap();
    holder.flush_transcripts().await;
    holder.shutdown().await;

    let reader = Agent::open_embedded(root, config(), ContractModel::new()).unwrap();
    let loaded = reader.resume_chat(chat).await.unwrap();
    assert_eq!(
        loaded.folder.map(|folder| folder.canonicalize().unwrap()),
        Some(directory.path().canonicalize().unwrap())
    );
    assert!(loaded.scheduled_tasks.is_empty());
    assert!(loaded.messages.is_empty());
    reader.shutdown().await;
}

/// Pauses the task, so it reaches its end without a run, and wakes the host at that end.
async fn end_while_paused(agent: &Agent, chat: ChatId, task: Uuid, end_ms: u64) {
    agent
        .set_scheduled_task_paused(chat, task, true)
        .await
        .unwrap();
    agent.set_scheduled_now_for_tests(
        DateTime::from_timestamp_millis(i64::try_from(end_ms).unwrap()).unwrap(),
    );
    assert!(
        agent
            .wake_scheduled_tasks(chat, end_ms)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The host switches the user's time zone while it holds the Agent: tasks the model creates
/// from then on read their schedule in the new zone, and the tools name it, while a task
/// created before keeps its zone and next run. An unknown zone is refused and the zone stays.
#[tokio::test]
async fn switching_the_time_zone_applies_to_tasks_created_after_it() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "before",
            "CronCreate",
            json!({"cron": "7 9 * * *", "prompt": "morning in UTC"}),
        ),
        text("Scheduled."),
    ]));
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |options| options.trust_model_scheduled_tasks = true,
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    complete_turn(&agent, chat, "Every morning at 9:07, please.").await;
    assert!(cron_create_description(&model).contains("local time zone, UTC:"));
    let before = tasks(&agent, chat).await.remove(0);
    assert_eq!(before.time_zone, "UTC");

    assert!(matches!(
        agent.set_time_zone("Mars/Olympus"),
        Err(AgentError::Invalid(message)) if message.contains("is not an IANA time zone")
    ));
    agent.set_time_zone("Asia/Shanghai").unwrap();
    model.enqueue(call(
        "after",
        "CronCreate",
        json!({"cron": "7 9 * * *", "prompt": "morning in Shanghai"}),
    ));
    model.enqueue(text("Scheduled."));
    complete_turn(
        &agent,
        chat,
        "I moved to Shanghai; also every morning there.",
    )
    .await;
    assert!(cron_create_description(&model).contains("local time zone, Asia/Shanghai:"));
    let created = tasks(&agent, chat).await;
    assert_eq!(
        created[0], before,
        "the earlier task keeps its zone and next run"
    );
    let after = &created[1];
    assert_eq!(after.time_zone, "Asia/Shanghai");
    // 09:07 in Shanghai is 01:07 UTC, the next one after 10:00 UTC on the 23rd.
    assert_eq!(after.scheduled_for_ms, Some(ms(at(24, 1, 7))));
    let hosted = agent
        .schedule_task(chat, NewScheduledTask::new("0 12 * * *", "lunch", true))
        .await
        .unwrap();
    assert_eq!(hosted.time_zone, "Asia/Shanghai");
    agent.shutdown().await;
}

/// What a tool call the model made returned: its JSON, or its error text, and whether it failed.
async fn call_result(agent: &Agent, chat: ChatId, call_id: &str) -> (String, bool) {
    agent
        .snapshot(chat)
        .await
        .unwrap()
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            tickloop_harness_protocol::Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == call_id => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result of {call_id}"))
}

/// The model changes a task in place with CronUpdate: the id stays, a new schedule is timed
/// from now in the user's zone, and a new prompt and title are what the next run uses; the
/// result has the shape CronCreate's has.
#[tokio::test(flavor = "multi_thread")]
async fn cron_update_changes_a_task_in_place() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "CronCreate",
            json!({"cron": "30 9 * * *", "prompt": "check the build", "title": "Build"}),
        ),
        text("Scheduled."),
    ]));
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |options| options.trust_model_scheduled_tasks = true,
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    complete_turn(&agent, chat, "Check the build at 9:30.").await;
    let created = tasks(&agent, chat).await.remove(0);
    let (created_result, _) = call_result(&agent, chat, "create").await;

    model.enqueue(call(
        "update",
        "CronUpdate",
        json!({"id": created.id, "cron": "20 9 * * *", "prompt": "check the release", "title": "Release"}),
    ));
    model.enqueue(text("Moved to 9:20."));
    complete_turn(&agent, chat, "Make it 9:20, and check the release instead.").await;
    let (updated_result, is_error) = call_result(&agent, chat, "update").await;
    assert!(!is_error, "{updated_result}");
    let fields = |text: &str| {
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(fields(&updated_result), fields(&created_result));
    let updated = tasks(&agent, chat).await.remove(0);
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.prompt, "check the release");
    assert_eq!(updated.title.as_deref(), Some("Release"));
    assert_eq!(updated.scheduled_for_ms, Some(ms(at(24, 9, 20))));
    assert_eq!(updated.created_at_ms, created.created_at_ms);

    model.enqueue(text("Checked the release."));
    agent
        .wake_scheduled_tasks(chat, updated.next_fire_at_ms.unwrap())
        .await
        .unwrap();
    agent.quiesce(chat).await;
    let last_prompt = model
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages
        .clone();
    assert!(
        format!("{last_prompt:?}").contains("check the release"),
        "the next run uses the new prompt"
    );
    agent.shutdown().await;
}

/// CronUpdate is refused for a schedule running more often than the host allows, a task the
/// Chat does not have (deleted, or another Chat's), and a call that changes nothing.
#[tokio::test]
async fn cron_update_refuses_what_cron_create_would_and_missing_tasks() {
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |options| options.scheduled_task_min_interval = Some(HOUR),
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let other = agent.create_chat_without_folder().await.unwrap();
    let task = agent
        .schedule_task(chat, NewScheduledTask::new("0 * * * *", "check", true))
        .await
        .unwrap();
    let elsewhere = agent
        .schedule_task(other, NewScheduledTask::new("0 * * * *", "check", true))
        .await
        .unwrap();
    for reply in [
        call(
            "often",
            "CronUpdate",
            json!({"id": task.id, "cron": "*/30 * * * *"}),
        ),
        call(
            "foreign",
            "CronUpdate",
            json!({"id": elsewhere.id, "prompt": "not mine"}),
        ),
        call("nothing", "CronUpdate", json!({"id": task.id})),
        text("Done."),
    ] {
        model.enqueue(reply);
    }
    complete_turn(&agent, chat, "Change things.").await;
    let (often, is_error) = call_result(&agent, chat, "often").await;
    assert!(
        is_error && often.contains("at most once per hour"),
        "{often}"
    );
    let (foreign, is_error) = call_result(&agent, chat, "foreign").await;
    assert!(
        is_error
            && foreign.contains(&tickloop_ids::not_found::<
                tickloop_harness_protocol::ScheduledTaskIdKind,
            >(&elsewhere.id.to_string())),
        "{foreign}"
    );
    let (nothing, is_error) = call_result(&agent, chat, "nothing").await;
    assert!(is_error && nothing.contains("at least one of"), "{nothing}");
    assert_eq!(tasks(&agent, chat).await, std::slice::from_ref(&task));
    assert_eq!(tasks(&agent, other).await, [elsewhere]);

    agent.delete_scheduled_task(chat, task.id).await.unwrap();
    model.enqueue(call(
        "deleted",
        "CronUpdate",
        json!({"id": task.id, "prompt": "again"}),
    ));
    model.enqueue(text("Gone."));
    complete_turn(&agent, chat, "Change the deleted one.").await;
    let (deleted, is_error) = call_result(&agent, chat, "deleted").await;
    assert!(
        is_error
            && deleted.contains(&tickloop_ids::not_found::<
                tickloop_harness_protocol::ScheduledTaskIdKind,
            >(&task.id.to_string())),
        "{deleted}"
    );
    agent.shutdown().await;
}

/// Where nobody is asked and the host does not trust the Agent's tasks, a task the model
/// changes waits for the user to confirm it again, cannot change again while it waits, and
/// times by its new schedule once confirmed.
#[tokio::test]
async fn a_task_the_model_changes_waits_for_confirmation_again() {
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |_| {},
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let task = agent
        .schedule_task(
            chat,
            NewScheduledTask::new("7 * * * *", "hourly check", true),
        )
        .await
        .unwrap();
    model.enqueue(call(
        "update",
        "CronUpdate",
        json!({"id": task.id, "cron": "37 * * * *"}),
    ));
    // The user is shown the task to confirm; it does not change under them.
    model.enqueue(call(
        "again",
        "CronUpdate",
        json!({"id": task.id, "prompt": "something the user did not see"}),
    ));
    model.enqueue(text("Changed; please confirm."));
    complete_turn(&agent, chat, "Run it at :37 instead.").await;
    let (again, is_error) = call_result(&agent, chat, "again").await;
    assert!(
        is_error && again.contains("waiting for confirmation"),
        "{again}"
    );
    let changed = tasks(&agent, chat).await.remove(0);
    assert_eq!(changed.prompt, "hourly check");
    assert_eq!(changed.id, task.id);
    assert_eq!(
        changed.confirmation,
        Some(ScheduledTaskConfirmation::Pending)
    );
    assert_eq!(changed.next_fire_at_ms, None);
    assert_eq!(
        changed.kind,
        tickloop_harness_protocol::ScheduledTaskKind::Recurring {
            cron: "37 * * * *".into()
        }
    );
    agent
        .decide_scheduled_task(chat, task.id, ScheduledTaskDecision::Confirm)
        .await
        .unwrap();
    assert_eq!(
        tasks(&agent, chat).await[0].scheduled_for_ms,
        Some(ms(at(23, 10, 37)))
    );
    agent.shutdown().await;
}

/// In the run a task started, the model changes that task: it keeps its id and times by the
/// new schedule, and the time its old schedule named next hands nothing over.
#[tokio::test(flavor = "multi_thread")]
async fn the_run_a_task_started_can_change_it() {
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = host(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        |options| options.trust_model_scheduled_tasks = true,
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let task = agent
        .schedule_task(
            chat,
            NewScheduledTask::new("7 * * * *", "hourly check", true),
        )
        .await
        .unwrap();
    model.enqueue(call(
        "slower",
        "CronUpdate",
        json!({"id": task.id, "cron": "0 */2 * * *"}),
    ));
    model.enqueue(text("Every two hours from now on."));
    let handed = agent
        .wake_scheduled_tasks(chat, task.next_fire_at_ms.unwrap())
        .await
        .unwrap();
    assert_eq!(handed.len(), 1);
    agent.quiesce(chat).await;
    let (result, is_error) = call_result(&agent, chat, "slower").await;
    assert!(!is_error, "{result}");
    let changed = tasks(&agent, chat).await.remove(0);
    assert_eq!(changed.id, task.id);
    assert_eq!(changed.scheduled_for_ms, Some(ms(at(23, 12, 0))));
    let old_next = ms(at(23, 11, 59));
    assert!(
        agent
            .wake_scheduled_tasks(chat, old_next)
            .await
            .unwrap()
            .is_empty(),
        "the old schedule's 11:07 run does not come"
    );
    agent.shutdown().await;
}
