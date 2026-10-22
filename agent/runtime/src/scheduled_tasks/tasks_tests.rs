use std::time::Duration;

use chrono::{DateTime, FixedOffset, TimeZone};
use tickloop_agent_cron::Schedule;
use tickloop_agent_protocol::{
    MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskDecision,
    ScheduledTaskKind, ScheduledTaskPromptSource,
};

use super::tasks::{
    Creation, CronTask, FALLBACK_WAKEUP_DELAY, ScheduledTaskError, ScheduledTasks,
    SelfPacedTurnEnd, Wakeup,
};

const NO_JITTER_ID: &str = "00000000";
const HALF_ID: &str = "80000000";
const USER: Creation = Creation {
    prompt_source: ScheduledTaskPromptSource::User,
    needs_confirmation: false,
};
const AGENT_NEEDING_CONFIRMATION: Creation = Creation {
    prompt_source: ScheduledTaskPromptSource::Agent,
    needs_confirmation: true,
};

/// 2026-10-<day> at the given local time in UTC+08:00.
fn local(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<FixedOffset> {
    FixedOffset::east_opt(8 * 3600)
        .unwrap()
        .with_ymd_and_hms(2026, 10, day, hour, minute, second)
        .unwrap()
}

fn ms(time: DateTime<FixedOffset>) -> u64 {
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
    now: DateTime<FixedOffset>,
) {
    tasks
        .create_cron(id.into(), cron_task, "check".into(), creation, &now)
        .unwrap();
}

fn wakeup(delay_seconds: u64) -> Wakeup {
    Wakeup {
        delay: Duration::from_secs(delay_seconds),
        reason: "build running".into(),
        prompt: "check the build".into(),
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
        .start_self_paced("aaaaaaaa".into(), "first loop".into(), USER, &now)
        .unwrap();
    assert_eq!(tasks.tasks().len(), MAX_SCHEDULED_TASKS_PER_CHAT);

    assert_eq!(
        tasks
            .create_cron(
                "bbbbbbbb".into(),
                recurring("0 * * * *"),
                "more".into(),
                USER,
                &now
            )
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
    tasks
        .start_self_paced("cccccccc".into(), "second loop".into(), USER, &now)
        .unwrap();
    assert!(tasks.get("aaaaaaaa").is_none());
    let rescheduled = tasks
        .schedule_wakeup("dddddddd".into(), wakeup(300), USER, &now)
        .unwrap();
    assert_eq!(rescheduled.id, "cccccccc");
    assert_eq!(rescheduled.prompt, "check the build");
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
            .start_self_paced("aaaaaaaa".into(), "loop".into(), USER, &now)
            .unwrap_err(),
        ScheduledTaskError::TooMany
    );
    assert_eq!(
        tasks
            .schedule_wakeup("aaaaaaaa".into(), wakeup(300), USER, &now)
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
    let confirmed = tasks
        .decide(NO_JITTER_ID, ScheduledTaskDecision::Confirm, &confirmed_at)
        .unwrap()
        .unwrap();
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
    assert_eq!(
        tasks.decide(HALF_ID, ScheduledTaskDecision::Reject, &now),
        Ok(None)
    );
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
            &local(22, 10, 0, 0),
        )
        .unwrap();
    assert_eq!(tasks.next_wake_at_ms(true), None);
    let confirmed = tasks
        .decide(
            HALF_ID,
            ScheduledTaskDecision::Confirm,
            &local(22, 11, 0, 0),
        )
        .unwrap()
        .unwrap();
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
        .start_self_paced(HALF_ID.into(), "watch".into(), USER, &now)
        .unwrap();
    assert_eq!(
        tasks.self_paced_turn_ended(&now),
        SelfPacedTurnEnd::FallbackScheduled
    );

    let task = tasks
        .schedule_wakeup(NO_JITTER_ID.into(), wakeup(10), USER, &now)
        .unwrap();
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
        .schedule_wakeup(NO_JITTER_ID.into(), wakeup(7200), USER, &now)
        .unwrap();
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
        .schedule_wakeup(HALF_ID.into(), wakeup(300), USER, &now)
        .unwrap();
    assert_eq!(tasks.stop_self_paced().unwrap().id, HALF_ID);
    assert_eq!(ids(tasks.tasks()), [NO_JITTER_ID]);
    assert_eq!(tasks.stop_self_paced(), None);
}

#[test]
fn a_self_paced_task_past_its_expiry_runs_once_more_and_ends() {
    let mut tasks = ScheduledTasks::default();
    tasks
        .schedule_wakeup(HALF_ID.into(), wakeup(3600), USER, &local(22, 10, 0, 0))
        .unwrap();
    tasks
        .schedule_wakeup(HALF_ID.into(), wakeup(3600), USER, &local(29, 9, 30, 0))
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
        .schedule_wakeup(HALF_ID.into(), wakeup(600), USER, &created)
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
        .start_self_paced("00000007".into(), "loop".into(), USER, &created)
        .unwrap();

    let removed = tasks.after_history_load(&local(22, 11, 0, 0));
    assert_eq!(
        ids(&removed),
        ["00000002", "00000003", "00000005", "00000007"]
    );
    assert_eq!(ids(tasks.tasks()), ["00000001", "00000004", "00000006"]);
}

#[test]
fn a_branch_inherits_no_tasks() {
    let mut tasks = ScheduledTasks::default();
    create(
        &mut tasks,
        NO_JITTER_ID,
        recurring("*/10 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    assert!(tasks.for_branch().tasks().is_empty());
    assert_eq!(tasks.tasks().len(), 1);
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
fn unused_ids_are_eight_hex_characters_not_taken_by_the_chat() {
    let mut tasks = ScheduledTasks::default();
    let id = tasks.unused_id();
    assert_eq!(id.len(), 8);
    create(
        &mut tasks,
        &id,
        recurring("0 * * * *"),
        USER,
        local(22, 10, 0, 0),
    );
    assert_ne!(tasks.unused_id(), id);
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
