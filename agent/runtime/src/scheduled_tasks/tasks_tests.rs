use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use tickloop_agent_cron::Schedule;
use tickloop_agent_protocol::{
    MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskCreator,
    ScheduledTaskDecision, ScheduledTaskKind, ScheduledTaskRemovalReason,
};

use uuid::Uuid;

use super::tasks::{
    Creation, CronTask, DecisionOutcome, FALLBACK_WAKEUP_DELAY, NewCronTask, RemovedTask,
    ScheduledTaskError, ScheduledTasks, SelfPacedTurnEnd, TaskEdit, TaskRules, TimingEdit, Wakeup,
};

/// Jitter reads the last 32 bits of a task id as a fraction.
const NO_JITTER_ID: Uuid = Uuid::from_u128(0);
const HALF_ID: Uuid = Uuid::from_u128(0x8000_0000);
const RULES: TaskRules = TaskRules::DEFAULT;
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

/// The task id whose last 32 bits are the eight hexadecimal digits `hex`, which jitter reads.
fn id(hex: &str) -> Uuid {
    Uuid::from_u128(u128::from(u32::from_str_radix(hex, 16).unwrap()))
}

fn new_cron(
    id: Uuid,
    cron_task: CronTask,
    prompt: &str,
    creation: Creation,
    zone: chrono_tz::Tz,
) -> NewCronTask {
    NewCronTask {
        id,
        cron_task,
        prompt: prompt.into(),
        title: None,
        creation,
        zone,
        ends_at_ms: None,
    }
}

fn create(
    tasks: &mut ScheduledTasks,
    id: Uuid,
    cron_task: CronTask,
    creation: Creation,
    now: DateTime<Utc>,
) {
    tasks
        .create_cron(
            new_cron(id, cron_task, "check", creation, ZONE),
            RULES,
            &now,
        )
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

fn ids(tasks: &[ScheduledTask]) -> Vec<Uuid> {
    tasks.iter().map(|task| task.id).collect()
}

fn removed_ids(removed: &[RemovedTask]) -> Vec<Uuid> {
    removed.iter().map(|removed| removed.task.id).collect()
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
    let task = tasks.get(&HALF_ID).unwrap();
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
    let task = tasks.get(&HALF_ID).unwrap();
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 14, 30, 0))));
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 14, 29, 15))));
    assert_eq!(task.expires_at_ms, None);
}

#[test]
fn a_schedule_that_never_runs_is_refused() {
    let mut tasks = ScheduledTasks::default();
    let error = tasks
        .create_cron(
            new_cron(NO_JITTER_ID, one_shot("0 0 30 2 *"), "never", USER, ZONE),
            RULES,
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
            id(&format!("{index:08x}")),
            recurring("*/10 * * * *"),
            USER,
            now,
        );
    }
    tasks
        .start_self_paced(id("aaaaaaaa"), "first loop".into(), USER, ZONE, RULES, &now)
        .unwrap();
    assert_eq!(tasks.tasks().len(), MAX_SCHEDULED_TASKS_PER_CHAT);

    assert_eq!(
        tasks
            .create_cron(
                new_cron(id("bbbbbbbb"), recurring("0 * * * *"), "more", USER, ZONE),
                RULES,
                &now
            )
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
    tasks
        .start_self_paced(
            id("cccccccc"),
            "second loop".into(),
            USER,
            ZONE,
            RULES,
            &now,
        )
        .unwrap();
    assert!(tasks.get(&id("aaaaaaaa")).is_none());
    let rescheduled = tasks
        .schedule_wakeup(id("dddddddd"), wakeup(300), USER, ZONE, RULES, &now)
        .unwrap()
        .task;
    assert_eq!(rescheduled.id, id("cccccccc"));
    assert_eq!(tasks.tasks().len(), MAX_SCHEDULED_TASKS_PER_CHAT);
}

#[test]
fn a_full_chat_refuses_a_first_self_paced_task() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    for index in 0..MAX_SCHEDULED_TASKS_PER_CHAT {
        create(
            &mut tasks,
            id(&format!("{index:08x}")),
            recurring("*/10 * * * *"),
            USER,
            now,
        );
    }
    assert_eq!(
        tasks
            .start_self_paced(id("aaaaaaaa"), "loop".into(), USER, ZONE, RULES, &now)
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
    assert_eq!(
        tasks
            .schedule_wakeup(id("aaaaaaaa"), wakeup(300), USER, ZONE, RULES, &now)
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
    let task = tasks.get(&NO_JITTER_ID).unwrap();
    assert_eq!(task.confirmation, Some(ScheduledTaskConfirmation::Pending));
    assert_eq!(task.next_fire_at_ms, None);
    assert_eq!(tasks.next_wake_at_ms(true), None);
    assert!(tasks.take_due(&local(23, 10, 0, 0)).fired.is_empty());

    let confirmed_at = local(22, 10, 33, 0);
    let confirmed =
        confirmed(tasks.decide(&NO_JITTER_ID, ScheduledTaskDecision::Confirm, &confirmed_at));
    assert_eq!(
        confirmed.confirmation,
        Some(ScheduledTaskConfirmation::Confirmed)
    );
    assert_eq!(confirmed.next_fire_at_ms, Some(ms(local(22, 10, 40, 0))));
    assert_eq!(confirmed.expires_at_ms, Some(ms(local(29, 10, 0, 0))));
    assert_eq!(tasks.next_wake_at_ms(true), Some(ms(local(22, 10, 40, 0))));
    assert_eq!(
        tasks.decide(&NO_JITTER_ID, ScheduledTaskDecision::Confirm, &confirmed_at),
        Err(ScheduledTaskError::NotPending(NO_JITTER_ID))
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
        tasks.decide(&HALF_ID, ScheduledTaskDecision::Reject, &now),
        Ok(DecisionOutcome::Rejected(task)) if task.id == HALF_ID
    ));
    assert!(tasks.tasks().is_empty());
    assert_eq!(
        tasks.decide(&HALF_ID, ScheduledTaskDecision::Reject, &now),
        Err(ScheduledTaskError::NotFound(HALF_ID))
    );
}

#[test]
fn a_pending_wakeup_starts_its_delay_when_confirmed() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            HALF_ID,
            wakeup(600),
            AGENT_NEEDING_CONFIRMATION,
            ZONE,
            RULES,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);
    let confirmed = confirmed(tasks.decide(
        &HALF_ID,
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
    assert!(fired.last_run.is_none());
    assert_eq!(fired.task.skipped, 2);
    assert_eq!(fired.task.last_fired_at_ms, Some(ms(local(22, 10, 35, 0))));
    assert_eq!(
        fired.task.last_scheduled_for_ms,
        Some(ms(local(22, 10, 30, 0)))
    );
    assert!(due.removed.is_empty());
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 10, 40, 0)))
    );

    assert!(tasks.take_due(&local(22, 10, 39, 59)).fired.is_empty());
    let due = tasks.take_due(&local(22, 10, 40, 0));
    assert_eq!(due.fired[0].skipped, 0);
    assert_eq!(tasks.get(&NO_JITTER_ID).unwrap().skipped, 2);
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
        tasks.get(&HALF_ID).unwrap().next_fire_at_ms,
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
    assert!(due.fired[0].last_run.is_some());
    assert_eq!(due.fired[0].task.next_fire_at_ms, None);
    assert_eq!(removed_ids(&due.removed), [NO_JITTER_ID]);
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
        assert!(due.fired[0].last_run.is_none(), "day {day}");
    }
    let due = tasks.take_due(&local(29, 9, 0, 0));
    assert!(due.fired[0].last_run.is_some());
    assert_eq!(removed_ids(&due.removed), [NO_JITTER_ID]);
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
        tasks.get(&NO_JITTER_ID).unwrap().expires_at_ms,
        Some(ms(local(29, 8, 0, 0)))
    );
    assert!(
        tasks.take_due(&local(29, 9, 0, 0)).fired[0]
            .last_run
            .is_some()
    );
}

#[test]
fn a_self_paced_turn_without_a_reschedule_gets_one_fallback_then_ends() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .start_self_paced(
            HALF_ID,
            "watch the deploy".into(),
            USER,
            ZONE,
            RULES,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);

    let first_turn_end = local(22, 10, 2, 0);
    assert_eq!(
        tasks.self_paced_turn_ended(RULES, &first_turn_end),
        SelfPacedTurnEnd::FallbackScheduled
    );
    let fallback_at = first_turn_end + FALLBACK_WAKEUP_DELAY;
    assert_eq!(tasks.next_wake_at_ms(true), Some(ms(fallback_at)));
    assert!(matches!(
        tasks.get(&HALF_ID).unwrap().kind,
        ScheduledTaskKind::SelfPaced { fallback: true, .. }
    ));

    let due = tasks.take_due(&fallback_at);
    assert_eq!(due.fired.len(), 1);
    assert!(due.fired[0].last_run.is_none());
    assert_eq!(tasks.next_wake_at_ms(true), None);

    let ended = tasks.self_paced_turn_ended(RULES, &local(22, 10, 23, 0));
    assert!(matches!(ended, SelfPacedTurnEnd::Ended(task) if task.id == HALF_ID));
    assert!(tasks.tasks().is_empty());
    assert_eq!(
        tasks.self_paced_turn_ended(RULES, &local(22, 10, 24, 0)),
        SelfPacedTurnEnd::Unchanged
    );
}

#[test]
fn a_reschedule_clears_the_fallback_and_clamps_the_delay() {
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    tasks
        .start_self_paced(HALF_ID, "watch".into(), USER, ZONE, RULES, &now)
        .unwrap();
    assert_eq!(
        tasks.self_paced_turn_ended(RULES, &now),
        SelfPacedTurnEnd::FallbackScheduled
    );

    let task = tasks
        .schedule_wakeup(NO_JITTER_ID, wakeup(10), USER, ZONE, RULES, &now)
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
        tasks.self_paced_turn_ended(RULES, &now),
        SelfPacedTurnEnd::Unchanged
    );

    let task = tasks
        .schedule_wakeup(NO_JITTER_ID, wakeup(7200), USER, ZONE, RULES, &now)
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
        .schedule_wakeup(HALF_ID, wakeup(300), USER, ZONE, RULES, &now)
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
            HALF_ID,
            wakeup(3600),
            USER,
            ZONE,
            RULES,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    tasks
        .schedule_wakeup(
            HALF_ID,
            wakeup(3600),
            USER,
            ZONE,
            RULES,
            &local(29, 9, 30, 0),
        )
        .unwrap();
    let due = tasks.take_due(&local(29, 10, 30, 0));
    assert!(due.fired[0].last_run.is_some());
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
        id("11111111"),
        one_shot("20 10 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        id("22222222"),
        one_shot("0 12 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        id("33333333"),
        recurring("0 9 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        created,
    );
    tasks
        .schedule_wakeup(HALF_ID, wakeup(600), USER, ZONE, RULES, &created)
        .unwrap();

    let resumed_at = local(22, 10, 35, 0);
    let removed = tasks.resume(&resumed_at);
    assert_eq!(removed_ids(&removed), [id("11111111"), HALF_ID]);
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 10, 40, 0)))
    );
    assert_eq!(
        tasks.get(&id("22222222")).unwrap().scheduled_for_ms,
        Some(ms(local(22, 12, 0, 0)))
    );
    assert_eq!(tasks.get(&id("33333333")).unwrap().next_fire_at_ms, None);
    assert!(tasks.take_due(&resumed_at).fired.is_empty());
}

#[test]
fn a_history_load_keeps_only_unexpired_recurring_and_upcoming_one_shot_tasks() {
    let created = local(22, 10, 0, 0);
    let pending = AGENT_NEEDING_CONFIRMATION;
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        id("00000001"),
        recurring("*/10 * * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        id("00000002"),
        recurring("0 9 * * *"),
        USER,
        local(15, 9, 0, 0),
    );
    create(
        &mut tasks,
        id("00000003"),
        one_shot("20 10 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        id("00000004"),
        one_shot("0 12 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        id("00000005"),
        one_shot("20 10 * * *"),
        pending,
        created,
    );
    create(
        &mut tasks,
        id("00000006"),
        one_shot("0 12 * * *"),
        pending,
        created,
    );
    tasks
        .start_self_paced(id("00000007"), "loop".into(), USER, ZONE, RULES, &created)
        .unwrap();

    let removed = tasks.after_history_load(&local(22, 11, 0, 0));
    assert_eq!(
        ids(&removed),
        [
            id("00000002"),
            id("00000003"),
            id("00000005"),
            id("00000007")
        ]
    );
    assert_eq!(
        ids(tasks.tasks()),
        [id("00000001"), id("00000004"), id("00000006")]
    );
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
        id("11111111"),
        one_shot("25 10 * * *"),
        USER,
        created,
    );
    create(
        &mut tasks,
        id("22222222"),
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
    assert_eq!(tasks.delete(&NO_JITTER_ID).unwrap().id, NO_JITTER_ID);
    assert_eq!(tasks.delete(&NO_JITTER_ID), None);
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
        &HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 15, 0, 0),
    );
    assert!(
        matches!(outcome, Ok(DecisionOutcome::ExpiredUnrun(removed)) if removed.task.id == HALF_ID)
    );
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
        &HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 10, 0, 0),
    );
    assert!(
        matches!(outcome, Ok(DecisionOutcome::ExpiredUnrun(removed)) if removed.task.id == HALF_ID)
    );
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
        &NO_JITTER_ID,
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
        .start_self_paced(HALF_ID, "watch the deploy".into(), USER, ZONE, RULES, &now)
        .unwrap();

    let agent = Creation {
        creator: ScheduledTaskCreator::Agent,
        needs_confirmation: true,
    };
    let rescheduled = tasks
        .schedule_wakeup(NO_JITTER_ID, wakeup(300), agent, ZONE, RULES, &now)
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
        .schedule_wakeup(NO_JITTER_ID, same_prompt, USER, ZONE, RULES, &now)
        .unwrap();
    assert!(!rescheduled.ignored_prompt);
}

#[test]
fn a_new_wakeup_takes_its_own_instruction() {
    let mut tasks = ScheduledTasks::default();
    let created = tasks
        .schedule_wakeup(
            HALF_ID,
            wakeup(300),
            USER,
            ZONE,
            RULES,
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
        tasks.get(&HALF_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 14, 59, 15)))
    );

    assert!(before_lead.resume(&local(22, 14, 59, 0)).is_empty());
    let removed = tasks.resume(&local(22, 14, 59, 30));
    assert_eq!(removed_ids(&removed), [HALF_ID]);
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
    let task = tasks.get(&HALF_ID).unwrap();
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 10, 10, 0))));
    assert_eq!(task.next_fire_at_ms, Some(ms(local(22, 10, 9, 45))));
    assert!(tasks.take_due(&resumed_at).fired.is_empty());
}

#[test]
fn rescheduling_a_pending_self_paced_task_keeps_it_pending_with_its_instruction() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            HALF_ID,
            wakeup(600),
            AGENT_NEEDING_CONFIRMATION,
            ZONE,
            RULES,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    let other_prompt = Wakeup {
        prompt: "do something else".into(),
        ..wakeup(1200)
    };
    let rescheduled = tasks
        .schedule_wakeup(
            NO_JITTER_ID,
            other_prompt,
            AGENT_NEEDING_CONFIRMATION,
            ZONE,
            RULES,
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
        &HALF_ID,
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
            new_cron(
                NO_JITTER_ID,
                recurring("30 1,3 * * *"),
                "check",
                USER,
                New_York,
            ),
            RULES,
            &new_york(1, 0, 0),
        )
        .unwrap();
    // 01:30 EDT is 05:30 UTC; the repeated 01:30 EST (06:30 UTC) does not run again.
    let first = tasks.take_due(&new_york(1, 1, 30));
    assert_eq!(first.fired[0].scheduled_for_ms, utc_ms(1, 5, 30));
    let second_pass = Utc.with_ymd_and_hms(2026, 11, 1, 6, 30, 0).unwrap();
    assert!(tasks.take_due(&second_pass).fired.is_empty());
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().next_fire_at_ms,
        Some(utc_ms(1, 8, 30))
    );
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().expires_at_ms,
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
                new_cron(NO_JITTER_ID, recurring("* * * * *"), "check", USER, ZONE),
                RULES,
                &latest
            )
            .unwrap_err(),
        ScheduledTaskError::NeverRuns("* * * * *".into())
    );

    let wakeup_at_end = tasks
        .schedule_wakeup(HALF_ID, wakeup(3600), USER, ZONE, RULES, &latest)
        .unwrap()
        .task
        .clone();
    assert!(wakeup_at_end.next_fire_at_ms > Some(ms_utc(&latest)));
    assert!(tasks.take_due(&latest).fired.is_empty());
    assert!(tasks.resume(&latest).is_empty());

    let corrupt = ScheduledTask {
        paused_at_ms: None,
        id: id("11111111"),
        scheduled_for_ms: Some(u64::MAX),
        next_fire_at_ms: Some(0),
        expires_at_ms: Some(u64::MAX),
        ..tasks
            .create_cron(
                new_cron(id("11111111"), recurring("0 * * * *"), "check", USER, ZONE),
                RULES,
                &local(22, 10, 0, 0),
            )
            .unwrap()
            .clone()
    };
    let mut stored = ScheduledTasks::new(vec![corrupt]);
    let due = stored.take_due(&local(22, 11, 0, 0));
    assert!(due.fired.is_empty());
    assert_eq!(removed_ids(&due.removed), [id("11111111")]);
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
    let expires = tasks.get(&NO_JITTER_ID).unwrap().expires_at_ms;
    tasks
        .set_paused(&NO_JITTER_ID, true, &local(22, 10, 1, 0))
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);
    assert!(tasks.take_due(&local(22, 11, 0, 0)).fired.is_empty());
    // A Continue of the whole Loop cannot clear the user's individual pause.
    tasks.resume(&local(22, 11, 1, 0));
    assert!(tasks.get(&NO_JITTER_ID).unwrap().paused_at_ms.is_some());
    tasks
        .set_paused(&NO_JITTER_ID, false, &local(22, 11, 5, 0))
        .unwrap();
    let task = tasks.get(&NO_JITTER_ID).unwrap();
    assert_eq!(task.scheduled_for_ms, Some(ms(local(22, 11, 10, 0))));
    assert_eq!(task.expires_at_ms, expires);
    assert!(tasks.take_due(&local(22, 11, 5, 0)).fired.is_empty());
    let final_run = tasks.take_due(&local(29, 11, 10, 0));
    assert_eq!(final_run.fired.len(), 1);
    assert!(final_run.fired[0].last_run.is_some());
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
        tasks.set_paused(&NO_JITTER_ID, true, &local(22, 10, 0, 0)),
        Err(ScheduledTaskError::Pending(_))
    ));
    assert_eq!(tasks, before);
}

#[test]
fn self_paced_resume_keeps_future_wake_and_delays_a_missed_wake_one_minute() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(
            NO_JITTER_ID,
            wakeup(600),
            USER,
            ZONE,
            RULES,
            &local(22, 10, 0, 0),
        )
        .unwrap();
    tasks
        .set_paused(&NO_JITTER_ID, true, &local(22, 10, 1, 0))
        .unwrap();
    tasks
        .set_paused(&NO_JITTER_ID, false, &local(22, 10, 2, 0))
        .unwrap();
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().scheduled_for_ms,
        Some(ms(local(22, 10, 10, 0)))
    );
    tasks
        .set_paused(&NO_JITTER_ID, true, &local(22, 10, 3, 0))
        .unwrap();
    let encoded = serde_json::to_value(tasks.tasks()).unwrap();
    let mut tasks = ScheduledTasks::new(serde_json::from_value(encoded).unwrap());
    assert!(tasks.after_history_load(&local(22, 10, 20, 0)).is_empty());
    assert_eq!(tasks.next_wake_at_ms(true), None);
    tasks
        .set_paused(&NO_JITTER_ID, false, &local(22, 10, 20, 0))
        .unwrap();
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().scheduled_for_ms,
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
        .set_paused(&NO_JITTER_ID, true, &local(22, 10, 1, 0))
        .unwrap();
    assert!(tasks.after_history_load(&local(22, 12, 0, 0)).is_empty());
    tasks
        .set_paused(&NO_JITTER_ID, false, &local(22, 12, 0, 0))
        .unwrap();
    assert_eq!(
        tasks.get(&NO_JITTER_ID).unwrap().scheduled_for_ms,
        Some(ms(local(23, 11, 0, 0)))
    );
    assert!(tasks.take_due(&local(22, 12, 0, 0)).fired.is_empty());
}

fn ending_at(id: Uuid, cron_task: CronTask, ends_at_ms: u64) -> NewCronTask {
    NewCronTask {
        ends_at_ms: Some(ends_at_ms),
        ..new_cron(id, cron_task, "check", USER, ZONE)
    }
}

#[test]
fn a_task_runs_only_before_its_end_and_leaves_with_its_last_run() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    assert_eq!(
        tasks
            .create_cron(
                ending_at(HALF_ID, recurring("0 * * * *"), ms(local(22, 11, 10, 0))),
                RULES,
                &created,
            )
            .unwrap_err(),
        ScheduledTaskError::EndsBeforeNextRun("0 * * * *".into()),
        "its first run would fire at 11:15"
    );
    let ends_at_ms = ms(local(22, 12, 20, 0));
    tasks
        .create_cron(
            ending_at(HALF_ID, recurring("0 * * * *"), ends_at_ms),
            RULES,
            &created,
        )
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), Some(ms(local(22, 11, 15, 0))));
    let first = tasks.take_due(&local(22, 11, 15, 0));
    assert_eq!(first.fired[0].last_run, None);
    let last = tasks.take_due(&local(22, 12, 15, 0));
    assert_eq!(
        last.fired[0].last_run,
        Some(ScheduledTaskRemovalReason::EndsAtReached),
        "13:15 would fire after the end"
    );
    assert_eq!(removed_ids(&last.removed), [HALF_ID]);
    assert!(tasks.tasks().is_empty());

    // A paused task leaves at its end without a run, and the host is woken for it.
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("0 * * * *"),
        USER,
        created,
    );
    let mut paused = tasks.into_tasks();
    paused[0].ends_at_ms = Some(ends_at_ms);
    let mut tasks = ScheduledTasks::new(paused);
    tasks
        .set_paused(&NO_JITTER_ID, true, &local(22, 10, 30, 0))
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), Some(ends_at_ms));
    assert_eq!(tasks.next_wake_at_ms(false), None);
    let ended = tasks.take_due(&local(22, 12, 20, 0));
    assert!(ended.fired.is_empty());
    assert_eq!(
        ended.removed,
        [RemovedTask {
            task: ended.removed[0].task.clone(),
            reason: ScheduledTaskRemovalReason::EndsAtReached,
        }]
    );
    assert!(tasks.tasks().is_empty());
}

#[test]
fn without_a_lifetime_tasks_never_expire() {
    let rules = TaskRules {
        lifetime: None,
        ..RULES
    };
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    tasks
        .create_cron(
            new_cron(HALF_ID, recurring("0 9 * * *"), "daily", USER, ZONE),
            rules,
            &created,
        )
        .unwrap();
    tasks
        .schedule_wakeup(NO_JITTER_ID, wakeup(600), USER, ZONE, rules, &created)
        .unwrap();
    assert!(
        tasks
            .tasks()
            .iter()
            .all(|task| task.expires_at_ms.is_none())
    );
    for day in 23..=31 {
        let due = tasks.take_due(&local(day, 9, 30, 0));
        let daily: Vec<_> = due
            .fired
            .iter()
            .filter(|fired| fired.task.id == HALF_ID)
            .collect();
        assert_eq!(daily.len(), 1, "day {day}");
        assert_eq!(daily[0].last_run, None, "day {day}");
    }
    assert!(
        tasks.after_history_load(&local(31, 12, 0, 0)).len() == 1,
        "only the loop goes"
    );
    assert_eq!(ids(tasks.tasks()), [HALF_ID]);
}

#[test]
fn a_least_interval_refuses_only_recurring_schedules_running_more_often() {
    let rules = TaskRules {
        min_interval: Some(Duration::from_secs(60 * 60)),
        ..RULES
    };
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    // `*/45` runs at :00 and :45, fifteen minutes before the next hour's :00.
    for cron in ["*/30 * * * *", "0,30 9 * * *", "*/45 * * * *"] {
        assert_eq!(
            tasks
                .create_cron(
                    new_cron(HALF_ID, recurring(cron), "often", USER, ZONE),
                    rules,
                    &now
                )
                .unwrap_err(),
            ScheduledTaskError::TooFrequent {
                cron: cron.into(),
                min_interval: Duration::from_secs(60 * 60),
            },
            "{cron}"
        );
    }
    for cron_task in [
        recurring("0 * * * *"),
        recurring("0 0,23 * * *"),
        recurring("7 9 * * 1"),
        one_shot("*/30 * * * *"),
    ] {
        tasks
            .create_cron(
                new_cron(Uuid::now_v7(), cron_task, "fine", USER, ZONE),
                rules,
                &now,
            )
            .unwrap();
    }
    assert!(RULES.check_interval(&recurring("* * * * *")).is_ok());
    let message = ScheduledTaskError::TooFrequent {
        cron: "*/30 * * * *".into(),
        min_interval: Duration::from_secs(60 * 60),
    }
    .to_string();
    assert_eq!(
        message,
        "`*/30 * * * *` runs too often: recurring tasks may run at most once per hour; use a schedule such as `0 * * * *`"
    );
}

#[test]
fn interval_limits_read_naturally_with_a_schedule_that_keeps_to_them() {
    let limit = |seconds| super::tasks::interval_limit(Duration::from_secs(seconds));
    assert_eq!(
        limit(30 * 60),
        "recurring tasks may run at most once every 30 minutes; use a schedule such as `0 * * * *`"
    );
    assert!(limit(24 * 60 * 60).contains("once per day; use a schedule such as `0 9 * * *`"));
    assert!(
        limit(2 * 24 * 60 * 60).contains("once every 2 days; use a schedule such as `0 9 * * 1`")
    );
    assert!(limit(90).contains("once every 90 seconds"));
}

#[test]
fn editing_keeps_the_task_and_times_a_new_schedule_from_now() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(&mut tasks, HALF_ID, recurring("0 * * * *"), USER, created);
    let before = tasks.get(&HALF_ID).unwrap().clone();
    let edited = tasks
        .edit(
            &HALF_ID,
            TaskEdit {
                prompt: Some("check again".into()),
                title: Some(Some("Checks".into())),
                timing: None,
                reconfirm: false,
            },
            RULES,
            &local(22, 10, 30, 0),
        )
        .unwrap()
        .clone();
    assert_eq!(edited.prompt, "check again");
    assert_eq!(edited.title.as_deref(), Some("Checks"));
    assert_eq!(edited.next_fire_at_ms, before.next_fire_at_ms);

    let retimed = tasks
        .edit(
            &HALF_ID,
            TaskEdit {
                timing: Some(TimingEdit {
                    cron_task: one_shot("0 15 * * *"),
                    zone: ZONE,
                    ends_at_ms: None,
                }),
                ..TaskEdit::default()
            },
            RULES,
            &local(22, 10, 30, 0),
        )
        .unwrap()
        .clone();
    assert_eq!(retimed.id, HALF_ID);
    assert_eq!(retimed.created_at_ms, before.created_at_ms);
    assert_eq!(retimed.expires_at_ms, None);
    assert_eq!(retimed.scheduled_for_ms, Some(ms(local(22, 15, 0, 0))));
    assert_eq!(retimed.next_fire_at_ms, Some(ms(local(22, 14, 59, 15))));
    let recurring_again = tasks
        .edit(
            &HALF_ID,
            TaskEdit {
                timing: Some(TimingEdit {
                    cron_task: recurring("0 * * * *"),
                    zone: ZONE,
                    ends_at_ms: None,
                }),
                ..TaskEdit::default()
            },
            RULES,
            &local(22, 11, 0, 0),
        )
        .unwrap()
        .clone();
    assert_eq!(
        recurring_again.expires_at_ms,
        Some(ms(local(29, 11, 0, 0))),
        "a schedule that becomes recurring lives from the edit"
    );

    tasks
        .start_self_paced(NO_JITTER_ID, "loop".into(), USER, ZONE, RULES, &created)
        .unwrap();
    assert_eq!(
        tasks
            .edit(
                &NO_JITTER_ID,
                TaskEdit {
                    timing: Some(TimingEdit {
                        cron_task: recurring("0 * * * *"),
                        zone: ZONE,
                        ends_at_ms: None,
                    }),
                    ..TaskEdit::default()
                },
                RULES,
                &created,
            )
            .unwrap_err(),
        ScheduledTaskError::SelfPaced(NO_JITTER_ID)
    );
    let missing = Uuid::now_v7();
    assert_eq!(
        tasks
            .edit(&missing, TaskEdit::default(), RULES, &created)
            .unwrap_err(),
        ScheduledTaskError::NotFound(missing)
    );
}

#[test]
fn every_way_a_task_leaves_names_its_reason() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    create(&mut tasks, HALF_ID, one_shot("0 15 * * *"), USER, created);
    let due = tasks.take_due(&local(22, 15, 0, 0));
    assert_eq!(
        due.fired[0].last_run,
        Some(ScheduledTaskRemovalReason::OneShotCompleted)
    );

    create(
        &mut tasks,
        NO_JITTER_ID,
        one_shot("0 15 * * *"),
        AGENT_NEEDING_CONFIRMATION,
        created,
    );
    let outcome = tasks.decide(
        &NO_JITTER_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 15, 1, 0),
    );
    assert!(matches!(
        outcome,
        Ok(DecisionOutcome::ExpiredUnrun(RemovedTask {
            reason: ScheduledTaskRemovalReason::Expired,
            ..
        }))
    ));

    tasks
        .schedule_wakeup(HALF_ID, wakeup(600), USER, ZONE, RULES, &created)
        .unwrap();
    let removed = tasks.resume(&local(22, 10, 20, 0));
    assert_eq!(
        removed[0].reason,
        ScheduledTaskRemovalReason::SelfPacedEnded,
        "a wake-up missed while paused ends the loop"
    );
}

#[test]
fn a_host_waking_after_a_tasks_end_runs_nothing_of_it() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    tasks
        .create_cron(
            ending_at(HALF_ID, recurring("0 * * * *"), ms(local(22, 12, 20, 0))),
            RULES,
            &created,
        )
        .unwrap();
    // The 11:15 run was due before the end, but the host only wakes at 13:00.
    let late = tasks.take_due(&local(22, 13, 0, 0));
    assert!(late.fired.is_empty());
    assert_eq!(
        late.removed[0].reason,
        ScheduledTaskRemovalReason::EndsAtReached
    );
    assert!(tasks.tasks().is_empty());
}

#[test]
fn confirming_a_task_with_no_run_left_before_its_end_deletes_it_unrun() {
    let created = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    tasks
        .create_cron(
            NewCronTask {
                creation: AGENT_NEEDING_CONFIRMATION,
                ..ending_at(HALF_ID, recurring("0 * * * *"), ms(local(22, 12, 20, 0)))
            },
            RULES,
            &created,
        )
        .unwrap();
    let outcome = tasks.decide(
        &HALF_ID,
        ScheduledTaskDecision::Confirm,
        &local(22, 12, 10, 0),
    );
    assert!(matches!(
        outcome,
        Ok(DecisionOutcome::ExpiredUnrun(RemovedTask {
            reason: ScheduledTaskRemovalReason::EndsAtReached,
            ..
        }))
    ));
    assert!(tasks.tasks().is_empty());
}

#[test]
fn a_least_interval_also_spaces_the_self_paced_loop() {
    let rules = TaskRules {
        min_interval: Some(Duration::from_secs(2 * 60 * 60)),
        ..RULES
    };
    let now = local(22, 10, 0, 0);
    let mut tasks = ScheduledTasks::default();
    let woken = tasks
        .schedule_wakeup(HALF_ID, wakeup(60), USER, ZONE, rules, &now)
        .unwrap()
        .task
        .clone();
    assert_eq!(woken.next_fire_at_ms, Some(ms(local(22, 12, 0, 0))));
    tasks.take_due(&local(22, 12, 0, 0));
    assert_eq!(
        tasks.self_paced_turn_ended(rules, &local(22, 12, 5, 0)),
        SelfPacedTurnEnd::FallbackScheduled
    );
    assert_eq!(
        tasks.get(&HALF_ID).unwrap().next_fire_at_ms,
        Some(ms(local(22, 14, 5, 0)))
    );
}
