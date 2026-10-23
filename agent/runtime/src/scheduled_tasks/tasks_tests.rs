use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use tickloop_agent_cron::Schedule;
use tickloop_agent_protocol::{
    MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskCreator,
    ScheduledTaskDecision, ScheduledTaskKind,
};

use super::tasks::{
    Creation, CronTask, DecisionOutcome, FALLBACK_WAKEUP_DELAY, ScheduledTaskError, ScheduledTasks,
    SelfPacedTurnEnd, Wakeup,
};

const NO_JITTER_ID: &str = "00000000";
const HALF_ID: &str = "80000000";
const USER: Creation = Creation {
    creator: ScheduledTaskCreator::User,
    needs_confirmation: false,
};
const AGENT_NEEDING_CONFIRMATION: Creation = Creation {
    creator: ScheduledTaskCreator::Agent,
    needs_confirmation: true,
};

/// Shanghai keeps UTC+08:00 all year.
const ZONE: chrono_tz::Tz = chrono_tz::Asia::Shanghai;

/// 2026-10-<day> at the given Shanghai time.
fn local(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    ZONE.with_ymd_and_hms(2026, 10, day, hour, minute, second)
        .unwrap()
        .with_timezone(&Utc)
}

fn ms(time: DateTime<Utc>) -> u64 {
    u64::try_from(time.timestamp_millis()).unwrap()
}

fn recurring(expression: &str) -> CronTask {
    CronTask::Recurring(Schedule::parse(expression).unwrap())
}

fn one_shot(expression: &str) -> CronTask {
    CronTask::OneShot(Schedule::parse(expression).unwrap())
}

fn create(
    tasks: &mut ScheduledTasks,
    id: &str,
    cron_task: CronTask,
    creation: Creation,
    now: DateTime<Utc>,
) {
    tasks
        .create_cron(id.into(), cron_task, "check".into(), creation, ZONE, &now)
        .unwrap();
}

fn wakeup(delay_seconds: u64) -> Wakeup {
    Wakeup {
        delay: Duration::from_secs(delay_seconds),
        reason: "build running".into(),
        prompt: "check the build".into(),
    }
}

fn confirmed(outcome: Result<DecisionOutcome<'_>, ScheduledTaskError>) -> ScheduledTask {
    match outcome {
        Ok(DecisionOutcome::Confirmed(task)) => task.clone(),
        other => panic!("expected a confirmed task, got {other:?}"),
    }
}

fn ids(tasks: &[ScheduledTask]) -> Vec<&str> {
    tasks.iter().map(|task| task.id.as_str()).collect()
}

#[test]
fn recurring_task_times_its_first_run_with_jitter_and_expires_in_seven_days() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        recurring("0 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    let task = tasks.get(HALF_ID).unwrap();
    assert_eq!(task.confirmation, None);
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 11, 0, 0))));
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 11, 15, 0))));
    assert_eq!(task.jitter_ms, 15 * 60 * 1000);
    assert_eq!(task.expires_at_ms, Some(ms(local(29, 10, 0, 0))));
}

#[test]
fn one_shot_task_on_the_half_hour_fires_early_and_never_expires() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        one_shot("30 14 * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    let task = tasks.get(HALF_ID).unwrap();
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 14, 30, 0))));
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 14, 29, 15))));
    assert_eq!(task.expires_at_ms, None);
}

#[test]
fn a_schedule_that_never_runs_is_refused() {
    let mut tasks = ScheduledTasks::default();
    let error = tasks
        .create_cron(
            NO_JITTER_ID.into(),
            one_shot("0 0 30 2 *"),
            "never".into(),
            USER,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap_err();
    assert_eq!(error, ScheduledTaskError::NeverRuns("0 0 30 2 *".into()));
    assert!(tasks.tasks().is_empty());
}

#[test]
fn a_chat_holds_at_most_fifty_tasks_and_a_new_self_paced_task_replaces_the_old() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    for index in 0..MAX_SCHEDULED_TASKS_PER_CHAT - 1 {
        create(
            &mut tasks,
            &format!("{index:08x}"),
            recurring("*/10 * * * *"),
            USER,
            now,
        );
    }
    tasks
        .start_self_paced("aaaaaaaa".into(), "first loop".into(), USER, ZONE, &now)
        .unwrap();
    assert_eq!(tasks.tasks().len(), MAX_SCHEDULED_TASKS_PER_CHAT);

    assert_eq!(
        tasks
            .create_cron(
                "bbbbbbbb".into(),
                recurring("0 * * * *"),
                "more".into(),
                USER,
                ZONE,
                &now
            )
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
    tasks
        .start_self_paced("cccccccc".into(), "second loop".into(), USER, ZONE, &now)
        .unwrap();
    assert!(tasks.get("aaaaaaaa").is_none());
    let rescheduled = tasks
        .schedule_wakeup("dddddddd".into(), wakeup(300), USER, ZONE, &now)
        .unwrap()
        .task;
    assert_eq!(rescheduled.id, "cccccccc");
    assert_eq!(tasks.tasks().len(), MAX_SCHEDULED_TASKS_PER_CHAT);
}

#[test]
fn a_full_chat_refuses_a_first_self_paced_task() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    for index in 0..MAX_SCHEDULED_TASKS_PER_CHAT {
        create(
            &mut tasks,
            &format!("{index:08x}"),
            recurring("*/10 * * * *"),
            USER,
            now,
        );
    }
    assert_eq!(
        tasks
            .start_self_paced("aaaaaaaa".into(), "loop".into(), USER, ZONE, &now)
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
    assert_eq!(
        tasks
            .schedule_wakeup("aaaaaaaa".into(), wakeup(300), USER, ZONE, &now)
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
}

#[test]
fn pending_tasks_do_not_time_until_confirmed() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        AGENT_NEEDING_CONFIRMATION,
        local(22, 10, 0, 0),
    );
    let task = tasks.get(NO_JITTER_ID).unwrap();
    assert_eq!(task.confirmation, Some(ScheduledTaskConfirmation::Pending));
    assert_eq!(task.next_fire_at_ms, None);
    assert_eq!(tasks.next_wake_at_ms(true), None);
    assert!(tasks.take_due(&local(23, 10, 0, 0)).fired.is_empty());

    let confirmed_at = local(22, 10, 33, 0);
    let confirmed =
        confirmed(tasks.decide(NO_JITTER_ID, ScheduledTaskDecision::Confirm, &confirmed_at));
    assert_eq!(
        confirmed.confirmation,
        Some(ScheduledTaskConfirmation::Confirmed)
    );
    assert_eq!(confirmed.next_fire_at_ms, Some(ms(local(22, 10, 40, 0))));
    assert_eq!(confirmed.expires_at_ms, Some(ms(local(29, 10, 0, 0))));
    assert_eq!(tasks.next_wake_at_ms(true), Some(ms(local(22, 10, 40, 0))));
    assert_eq!(
        tasks.decide(NO_JITTER_ID, ScheduledTaskDecision::Confirm, &confirmed_at),
        Err(ScheduledTaskError::NotPending(NO_JITTER_ID.into()))
    );
}

#[test]
fn rejecting_a_pending_task_deletes_it() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        one_shot("0 15 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        now,
    );
    assert!(matches!(
        tasks.decide(HALF_ID, ScheduledTaskDecision::Reject, &now),
        Ok(DecisionOutcome::Rejected(task)) if task.id == HALF_ID
    ));
    assert!(tasks.tasks().is_empty());
    assert_eq!(
        tasks.decide(HALF_ID, ScheduledTaskDecision::Reject, &now),
        Err(ScheduledTaskError::NotFound(HALF_ID.into()))
    );
}

#[test]
fn a_pending_wakeup_starts_its_delay_when_confirmed() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            HALF_ID.into(),
            wakeup(600),
            AGENT_NEEDING_CONFIRMATION,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);
    let confirmed = confirmed(tasks.decide(
        HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 11, 0, 0),
    ));
    assert_eq!(confirmed.next_fire_at_ms, Some(ms(local(22, 11, 10, 0))));
}

#[test]
fn a_late_wake_runs_a_recurring_task_once_and_counts_the_skipped_times() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    assert!(tasks.take_due(&local(22, 10, 9, 59)).fired.is_empty());

    let due = tasks.take_due(&local(22, 10, 35, 0));
    assert_eq!(due.fired.len(), 1);
    let fired = &due.fired[0];
    assert_eq!(fired.scheduled_for_ms, ms(local(22, 10, 30, 0)));
    assert_eq!(fired.skipped, 2);
    assert!(!fired.is_last);
    assert_eq!(fired.task.skipped, 2);
    assert_eq!(fired.task.last_fired_at_ms, Some(ms(local(22, 10, 35, 0))));
    assert_eq!(
        fired.task.last_scheduled_for_ms,
        Some(ms(local(22, 10, 30, 0)))
    );
    assert!(due.removed.is_empty());
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 10, 40, 0)))
    );

    assert!(tasks.take_due(&local(22, 10, 39, 59)).fired.is_empty());
    let due = tasks.take_due(&local(22, 10, 40, 0));
    assert_eq!(due.fired[0].skipped, 0);
    assert_eq!(tasks.get(NO_JITTER_ID).unwrap().skipped, 2);
}

#[test]
fn jittered_runs_fire_at_their_fire_time() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        recurring("0 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    assert!(tasks.take_due(&local(22, 11, 14, 59)).fired.is_empty());
    let due = tasks.take_due(&local(22, 11, 15, 0));
    assert_eq!(due.fired[0].scheduled_for_ms, ms(local(22, 11, 0, 0)));
    assert_eq!(
        tasks.get(HALF_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 12, 15, 0)))
    );
}

#[test]
fn a_one_shot_task_is_deleted_after_it_runs() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        one_shot("45 10 * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    let due = tasks.take_due(&local(22, 10, 45, 0));
    assert_eq!(due.fired.len(), 1);
    assert!(due.fired[0].is_last);
    assert_eq!(due.fired[0].task.next_fire_at_ms, None);
    assert_eq!(ids(&due.removed), [NO_JITTER_ID]);
    assert!(tasks.tasks().is_empty());
}

#[test]
fn a_recurring_task_runs_one_last_time_seven_days_after_creation() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("0 9 * * *"),
        USER,
        local(22, 8, 0, 0),
    );
    for day in 22..=28 {
        let due = tasks.take_due(&local(day, 9, 0, 0));
        assert_eq!(due.fired.len(), 1, "day {day}");
        assert!(!due.fired[0].is_last, "day {day}");
    }
    let due = tasks.take_due(&local(29, 9, 0, 0));
    assert!(due.fired[0].is_last);
    assert_eq!(ids(&due.removed), [NO_JITTER_ID]);
    assert!(tasks.tasks().is_empty());
}

#[test]
fn pausing_does_not_extend_the_seven_days() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("0 9 * * *"),
        USER,
        local(22, 8, 0, 0),
    );
    assert!(tasks.resume(&local(29, 7, 0, 0)).is_empty());
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().expires_at_ms,
        Some(ms(local(29, 8, 0, 0)))
    );
    assert!(tasks.take_due(&local(29, 9, 0, 0)).fired[0].is_last);
}

#[test]
fn a_self_paced_turn_without_a_reschedule_gets_one_fallback_then_ends() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .start_self_paced(
            HALF_ID.into(),
            "watch the deploy".into(),
            USER,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);

    let first_turn_end = local(22, 10, 2, 0);
    assert_eq!(
        tasks.self_paced_turn_ended(&first_turn_end),
        SelfPacedTurnEnd::FallbackScheduled
    );
    let fallback_at = first_turn_end + FALLBACK_WAKEUP_DELAY;
    assert_eq!(tasks.next_wake_at_ms(true), Some(ms(fallback_at)));
    assert!(matches!(
        tasks.get(HALF_ID).unwrap().kind,
        ScheduledTaskKind::SelfPaced { fallback: true, .. }
    ));

    let due = tasks.take_due(&fallback_at);
    assert_eq!(due.fired.len(), 1);
    assert!(!due.fired[0].is_last);
    assert_eq!(tasks.next_wake_at_ms(true), None);

    let ended = tasks.self_paced_turn_ended(&local(22, 10, 23, 0));
    assert!(matches!(ended, SelfPacedTurnEnd::Ended(task) if task.id == HALF_ID));
    assert!(tasks.tasks().is_empty());
    assert_eq!(
        tasks.self_paced_turn_ended(&local(22, 10, 24, 0)),
        SelfPacedTurnEnd::Unchanged
    );
}

#[test]
fn a_reschedule_clears_the_fallback_and_clamps_the_delay() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    tasks
        .start_self_paced(HALF_ID.into(), "watch".into(), USER, ZONE, &now)
        .unwrap();
    assert_eq!(
        tasks.self_paced_turn_ended(&now),
        SelfPacedTurnEnd::FallbackScheduled
    );

    let task = tasks
        .schedule_wakeup(NO_JITTER_ID.into(), wakeup(10), USER, ZONE, &now)
        .unwrap()
        .task;
    assert_eq!(task.id, HALF_ID);
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 10, 1, 0))));
    assert_eq!(
        task.kind,
        ScheduledTaskKind::SelfPaced {
            delay_seconds: Some(60),
            reason: Some("build running".into()),
            fallback: false,
        }
    );
    assert_eq!(
        tasks.self_paced_turn_ended(&now),
        SelfPacedTurnEnd::Unchanged
    );

    let task = tasks
        .schedule_wakeup(NO_JITTER_ID.into(), wakeup(7200), USER, ZONE, &now)
        .unwrap()
        .task;
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 11, 0, 0))));
}

#[test]
fn stopping_or_esc_deletes_only_the_self_paced_task() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        now,
    );
    tasks
        .schedule_wakeup(HALF_ID.into(), wakeup(300), USER, ZONE, &now)
        .unwrap();
    assert_eq!(tasks.stop_self_paced().unwrap().id, HALF_ID);
    assert_eq!(ids(tasks.tasks()), [NO_JITTER_ID]);
    assert_eq!(tasks.stop_self_paced(), None);
}

#[test]
fn a_self_paced_task_past_its_expiry_runs_once_more_and_ends() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            HALF_ID.into(),
            wakeup(3600),
            USER,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    tasks
        .schedule_wakeup(
            HALF_ID.into(),
            wakeup(3600),
            USER,
            ZONE,
            &local(29, 9, 30, 0),
        )
        .unwrap();
    let due = tasks.take_due(&local(29, 10, 30, 0));
    assert!(due.fired[0].is_last);
    assert!(tasks.tasks().is_empty());
}

#[test]
fn resuming_waits_for_the_next_time_after_now_without_catching_up() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "11111111",
        one_shot("20 10 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "22222222",
        one_shot("0 12 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "33333333",
        recurring("0 9 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        created,
    );
    tasks
        .schedule_wakeup(HALF_ID.into(), wakeup(600), USER, ZONE, &created)
        .unwrap();

    let resumed_at = local(22, 10, 35, 0);
    let removed = tasks.resume(&resumed_at);
    assert_eq!(ids(&removed), ["11111111", HALF_ID]);
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 10, 40, 0)))
    );
    assert_eq!(
        tasks.get("22222222").unwrap().scheduled_for_ms,
        Some(ms(local(22, 12, 0, 0)))
    );
    assert_eq!(tasks.get("33333333").unwrap().next_fire_at_ms, None);
    assert!(tasks.take_due(&resumed_at).fired.is_empty());
}

#[test]
fn a_history_load_keeps_only_unexpired_recurring_and_upcoming_one_shot_tasks() {
    let created = local(22, 10, 0, 0);
    let pending = AGENT_NEEDING_CONFIRMATION;
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        "00000001",
        recurring("*/10 * * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "00000002",
        recurring("0 9 * * *"),
        USER,
        local(15, 9, 0, 0),
    );
    create(
        &mut tasks,
        "00000003",
        one_shot("20 10 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "00000004",
        one_shot("0 12 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "00000005",
        one_shot("20 10 * * *"),
        pending,
        created,
    );
    create(
        &mut tasks,
        "00000006",
        one_shot("0 12 * * *"),
        pending,
        created,
    );
    tasks
        .start_self_paced("00000007".into(), "loop".into(), USER, ZONE, &created)
        .unwrap();

    let removed = tasks.after_history_load(&local(22, 11, 0, 0));
    assert_eq!(
        ids(&removed),
        ["00000002", "00000003", "00000005", "00000007"]
    );
    assert_eq!(ids(tasks.tasks()), ["00000001", "00000004", "00000006"]);
}

#[test]
fn next_wake_is_the_earliest_confirmed_fire_time_while_receiving() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    assert_eq!(tasks.next_wake_at_ms(true), None);
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("0 * * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "11111111",
        one_shot("25 10 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        "22222222",
        one_shot("5 10 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        created,
    );
    assert_eq!(tasks.next_wake_at_ms(true), Some(ms(local(22, 10, 25, 0))));
    assert_eq!(tasks.next_wake_at_ms(false), None);
}

#[test]
fn deleting_removes_only_the_named_task() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        now,
    );
    create(&mut tasks, HALF_ID, one_shot("0 15 * * *"), USER, now);
    assert_eq!(tasks.delete(NO_JITTER_ID).unwrap().id, NO_JITTER_ID);
    assert_eq!(tasks.delete(NO_JITTER_ID), None);
    assert_eq!(ids(tasks.tasks()), [HALF_ID]);
}

#[test]
fn stored_tasks_keep_timing_where_they_left_off() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    let stored = serde_json::to_string(tasks.tasks()).unwrap();

    let mut restored = ScheduledTasks::new(serde_json::from_str(&stored).unwrap());
    assert_eq!(restored, tasks);
    let due = restored.take_due(&local(22, 10, 10, 0));
    assert_eq!(due.fired[0].scheduled_for_ms, ms(local(22, 10, 10, 0)));
}

#[test]
fn confirming_a_one_shot_task_after_its_time_deletes_it_unrun() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        one_shot("0 15 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        local(22, 10, 0, 0),
    );
    let outcome = tasks.decide(
        HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 15, 0, 0),
    );
    assert!(matches!(outcome, Ok(DecisionOutcome::ExpiredUnrun(task)) if task.id == HALF_ID));
    assert!(tasks.tasks().is_empty());
}

#[test]
fn confirming_a_recurring_task_after_its_seven_days_deletes_it_unrun() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        recurring("0 9 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        local(15, 10, 0, 0),
    );
    let outcome = tasks.decide(
        HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 10, 0, 0),
    );
    assert!(matches!(outcome, Ok(DecisionOutcome::ExpiredUnrun(task)) if task.id == HALF_ID));
    assert!(tasks.tasks().is_empty());
}

#[test]
fn confirming_a_one_shot_task_before_its_time_keeps_that_time() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        one_shot("0 15 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        local(22, 10, 0, 0),
    );
    let confirmed = confirmed(tasks.decide(
        NO_JITTER_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 14, 0, 0),
    ));
    assert_eq!(confirmed.scheduled_for_ms, Some(ms(local(22, 15, 0, 0))));
}

#[test]
fn rescheduling_keeps_the_instruction_the_user_started() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    tasks
        .start_self_paced(HALF_ID.into(), "watch the deploy".into(), USER, ZONE, &now)
        .unwrap();

    let agent = Creation {
        creator: ScheduledTaskCreator::Agent,
        needs_confirmation: true,
    };
    let rescheduled = tasks
        .schedule_wakeup(NO_JITTER_ID.into(), wakeup(300), agent, ZONE, &now)
        .unwrap();
    assert!(rescheduled.ignored_prompt);
    let task = rescheduled.task;
    assert_eq!(task.id, HALF_ID);
    assert_eq!(task.prompt, "watch the deploy");
    assert_eq!(task.creator, ScheduledTaskCreator::User);
    assert_eq!(task.confirmation, None);
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 10, 5, 0))));

    let same_prompt = Wakeup {
        prompt: "watch the deploy".into(),
        ..wakeup(600)
    };
    let rescheduled = tasks
        .schedule_wakeup(NO_JITTER_ID.into(), same_prompt, USER, ZONE, &now)
        .unwrap();
    assert!(!rescheduled.ignored_prompt);
}

#[test]
fn a_new_wakeup_takes_its_own_instruction() {
    let mut tasks = ScheduledTasks::default();
    let created = tasks
        .schedule_wakeup(
            HALF_ID.into(),
            wakeup(300),
            USER,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    assert!(!created.ignored_prompt);
    assert_eq!(created.task.prompt, "check the build");
}

#[test]
fn resuming_deletes_a_one_shot_task_whose_early_fire_time_passed() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(&mut tasks, HALF_ID, one_shot("0 15 * * *"), USER, created);
    let mut before_lead = tasks.clone();
    assert_eq!(
        tasks.get(HALF_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 14, 59, 15)))
    );

    assert!(before_lead.resume(&local(22, 14, 59, 0)).is_empty());
    let removed = tasks.resume(&local(22, 14, 59, 30));
    assert_eq!(ids(&removed), [HALF_ID]);
    assert!(tasks.take_due(&local(22, 14, 59, 30)).fired.is_empty());
}

#[test]
fn resuming_skips_a_five_minute_run_whose_early_fire_time_passed() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        HALF_ID,
        recurring("*/5 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    let resumed_at = local(22, 10, 4, 50);
    assert!(tasks.resume(&resumed_at).is_empty());
    let task = tasks.get(HALF_ID).unwrap();
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 10, 10, 0))));
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 10, 9, 45))));
    assert!(tasks.take_due(&resumed_at).fired.is_empty());
}

#[test]
fn rescheduling_a_pending_self_paced_task_keeps_it_pending_with_its_instruction() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            HALF_ID.into(),
            wakeup(600),
            AGENT_NEEDING_CONFIRMATION,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    let other_prompt = Wakeup {
        prompt: "do something else".into(),
        ..wakeup(1200)
    };
    let rescheduled = tasks
        .schedule_wakeup(
            NO_JITTER_ID.into(),
            other_prompt,
            AGENT_NEEDING_CONFIRMATION,
            ZONE,
            &local(22, 10, 5, 0),
        )
        .unwrap();
    assert!(rescheduled.ignored_prompt);
    let task = rescheduled.task;
    assert_eq!(task.id, HALF_ID);
    assert_eq!(task.prompt, "check the build");
    assert_eq!(task.confirmation, Some(ScheduledTaskConfirmation::Pending));
    assert_eq!(task.next_fire_at_ms, None);
    assert_eq!(tasks.next_wake_at_ms(true), None);

    let confirmed = confirmed(tasks.decide(
        HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 11, 0, 0),
    ));
    assert_eq!(confirmed.next_fire_at_ms, Some(ms(local(22, 11, 20, 0))));
}

#[test]
fn tasks_follow_the_local_clock_through_a_fall_back_overlap() {
    use chrono_tz::America::New_York;

    let new_york = |day: u32, hour: u32, minute: u32| {
        New_York
            .with_ymd_and_hms(2026, 11, day, hour, minute, 0)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    };
    let utc_ms = |day: u32, hour: u32, minute: u32| {
        u64::try_from(
            chrono::Utc
                .with_ymd_and_hms(2026, 11, day, hour, minute, 0)
                .unwrap()
                .timestamp_millis(),
        )
        .unwrap()
    };
    let mut tasks = ScheduledTasks::default();
    tasks
        .create_cron(
            NO_JITTER_ID.into(),
            recurring("30 1,3 * * *"),
            "check".into(),
            USER,
            New_York,
            &new_york(1, 0, 0),
        )
        .unwrap();
    // 01:30 EDT is 05:30 UTC; the repeated 01:30 EST (06:30 UTC) does not run again.
    let first = tasks.take_due(&new_york(1, 1, 30));
    assert_eq!(first.fired[0].scheduled_for_ms, utc_ms(1, 5, 30));
    let second_pass = Utc.with_ymd_and_hms(2026, 11, 1, 6, 30, 0).unwrap();
    assert!(tasks.take_due(&second_pass).fired.is_empty());
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().next_fire_at_ms,
        Some(utc_ms(1, 8, 30))
    );
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().expires_at_ms,
        Some(utc_ms(8, 4, 0))
    );
}

#[test]
fn extreme_times_do_not_overflow() {
    let latest = chrono::DateTime::<chrono::Utc>::MAX_UTC;
    let mut tasks = ScheduledTasks::default();
    assert_eq!(
        tasks
            .create_cron(
                NO_JITTER_ID.into(),
                recurring("* * * * *"),
                "check".into(),
                USER,
                ZONE,
                &latest
            )
            .unwrap_err(),
        ScheduledTaskError::NeverRuns("* * * * *".into())
    );

    let wakeup_at_end = tasks
        .schedule_wakeup(HALF_ID.into(), wakeup(3600), USER, ZONE, &latest)
        .unwrap()
        .task
        .clone();
    assert!(wakeup_at_end.next_fire_at_ms > Some(ms_utc(&latest)));
    assert!(tasks.take_due(&latest).fired.is_empty());
    assert!(tasks.resume(&latest).is_empty());

    let corrupt = ScheduledTask {
        paused_at_ms: None,
        id: "11111111".into(),
        scheduled_for_ms: Some(u64::MAX),
        next_fire_at_ms: Some(0),
        expires_at_ms: Some(u64::MAX),
        ..tasks
            .create_cron(
                "11111111".into(),
                recurring("0 * * * *"),
                "check".into(),
                USER,
                ZONE,
                &local(22, 10, 0, 0),
            )
            .unwrap()
            .clone()
    };
    let mut stored = ScheduledTasks::new(vec![corrupt]);
    let due = stored.take_due(&local(22, 11, 0, 0));
    assert!(due.fired.is_empty());
    assert_eq!(ids(&due.removed), ["11111111"]);
}

fn ms_utc(time: &chrono::DateTime<chrono::Utc>) -> u64 {
    u64::try_from(time.timestamp_millis()).unwrap()
}

#[test]
fn individual_pause_skips_missed_cron_times_and_preserves_expiry() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    let expires = tasks.get(NO_JITTER_ID).unwrap().expires_at_ms;
    tasks
        .set_paused(NO_JITTER_ID, true, &local(22, 10, 1, 0))
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);
    assert!(tasks.take_due(&local(22, 11, 0, 0)).fired.is_empty());
    // A Continue of the whole Loop cannot clear the user's individual pause.
    tasks.resume(&local(22, 11, 1, 0));
    assert!(tasks.get(NO_JITTER_ID).unwrap().paused_at_ms.is_some());
    tasks
        .set_paused(NO_JITTER_ID, false, &local(22, 11, 5, 0))
        .unwrap();
    let task = tasks.get(NO_JITTER_ID).unwrap();
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 11, 10, 0))));
    assert_eq!(task.expires_at_ms, expires);
    assert!(tasks.take_due(&local(22, 11, 5, 0)).fired.is_empty());
    let final_run = tasks.take_due(&local(29, 11, 10, 0));
    assert_eq!(final_run.fired.len(), 1);
    assert!(final_run.fired[0].is_last);
    assert!(tasks.tasks().is_empty());
}

#[test]
fn pending_tasks_refuse_individual_pause() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("* * * * *"),
        AGENT_NEEDING_CONFIRMATION,
        local(22, 10, 0, 0),
    );
    let before = tasks.clone();
    assert!(matches!(
        tasks.set_paused(NO_JITTER_ID, true, &local(22, 10, 0, 0)),
        Err(ScheduledTaskError::Pending(_))
    ));
    assert_eq!(tasks, before);
}

#[test]
fn self_paced_resume_keeps_future_wake_and_delays_a_missed_wake_one_minute() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            NO_JITTER_ID.into(),
            wakeup(600),
            USER,
            ZONE,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    tasks
        .set_paused(NO_JITTER_ID, true, &local(22, 10, 1, 0))
        .unwrap();
    tasks
        .set_paused(NO_JITTER_ID, false, &local(22, 10, 2, 0))
        .unwrap();
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().scheduled_for_ms,
        Some(ms(local(22, 10, 10, 0)))
    );
    tasks
        .set_paused(NO_JITTER_ID, true, &local(22, 10, 3, 0))
        .unwrap();
    let encoded = serde_json::to_value(tasks.tasks()).unwrap();
    let mut tasks = ScheduledTasks::new(serde_json::from_value(encoded).unwrap());
    assert!(tasks.after_history_load(&local(22, 10, 20, 0)).is_empty());
    assert_eq!(tasks.next_wake_at_ms(true), None);
    tasks
        .set_paused(NO_JITTER_ID, false, &local(22, 10, 20, 0))
        .unwrap();
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().scheduled_for_ms,
        Some(ms(local(22, 10, 21, 0)))
    );
}

#[test]
fn paused_one_shot_survives_history_and_resumes_at_a_future_time() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        one_shot("0 11 * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    tasks
        .set_paused(NO_JITTER_ID, true, &local(22, 10, 1, 0))
        .unwrap();
    assert!(tasks.after_history_load(&local(22, 12, 0, 0)).is_empty());
    tasks
        .set_paused(NO_JITTER_ID, false, &local(22, 12, 0, 0))
        .unwrap();
    assert_eq!(
        tasks.get(NO_JITTER_ID).unwrap().scheduled_for_ms,
        Some(ms(local(23, 11, 0, 0)))
    );
    assert!(tasks.take_due(&local(22, 12, 0, 0)).fired.is_empty());
}
