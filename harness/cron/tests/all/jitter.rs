use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Asia::Kolkata;
use tickloop_harness_cron::{Run, Schedule, TaskKind, generate_task_id, next_run, run_for};
use uuid::Uuid;

/// Jitter reads the last 32 bits of the id as a fraction.
const NO_JITTER_ID: Uuid = Uuid::from_u128(0x0198_0000_0000_7000_8000_0000_0000_0000);
const HALF_ID: Uuid = Uuid::from_u128(0x0198_0000_0000_7000_8000_0000_8000_0000);
const MAX_ID: Uuid = Uuid::from_u128(0x0198_0000_0000_7000_8000_0000_ffff_ffff);
const SMALL_ID: Uuid = Uuid::from_u128(0x0198_0000_0000_7000_8000_0000_0100_0000);

fn utc(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, minute, second)
        .unwrap()
}

fn runs(expression: &str, task_id: &Uuid, after: DateTime<Utc>, count: usize) -> Vec<Run<Utc>> {
    let schedule = Schedule::parse(expression).unwrap();
    std::iter::successors(
        next_run(&schedule, TaskKind::Recurring, task_id, &after),
        |previous| {
            next_run(
                &schedule,
                TaskKind::Recurring,
                task_id,
                &previous.scheduled_for,
            )
        },
    )
    .take(count)
    .collect()
}

fn jitters(expression: &str, task_id: &Uuid, count: usize) -> Vec<TimeDelta> {
    runs(expression, task_id, utc(22, 10, 0, 0), count)
        .iter()
        .map(Run::jitter)
        .collect()
}

fn one_shot(expression: &str, task_id: &Uuid, after: DateTime<Utc>) -> Run<Utc> {
    next_run(
        &Schedule::parse(expression).unwrap(),
        TaskKind::OneShot,
        task_id,
        &after,
    )
    .unwrap()
}

#[test]
fn every_five_minutes_fires_fifteen_seconds_early() {
    for task_id in [&NO_JITTER_ID, &HALF_ID, &MAX_ID] {
        let runs = runs("*/5 * * * *", task_id, utc(22, 10, 0, 0), 3);
        assert_eq!(
            runs.iter().map(|run| run.fire_at).collect::<Vec<_>>(),
            [utc(22, 10, 4, 45), utc(22, 10, 9, 45), utc(22, 10, 14, 45)]
        );
    }
    for expression in [
        "0-59/5 * * * *",
        "*/5 * 1-31 * *",
        "*/5 */1 * * *",
        "*/5 * * * 0-7",
        "*/5 * * * 0-6",
        "*/5 * 1-31 * 1",
        "0-55/5 0-23 1-31 1-12 0-7",
    ] {
        assert_eq!(
            jitters(expression, &HALF_ID, 1),
            [TimeDelta::seconds(-15)],
            "{expression}"
        );
    }
}

#[test]
fn five_minute_steps_on_only_some_days_are_delayed_normally() {
    for expression in [
        "*/5 * * * 1-5",
        "*/5 * 1-15 * *",
        "*/5 * 1-15 * 1",
        "*/5 9-17 * * *",
    ] {
        let jitter = jitters(expression, &HALF_ID, 1)[0];
        assert!(jitter > TimeDelta::zero(), "{expression}: {jitter}");
    }
}

#[test]
fn every_five_minutes_never_fires_before_the_given_time() {
    let run = runs("*/5 * * * *", &HALF_ID, utc(22, 10, 4, 50), 1).remove(0);
    assert_eq!(run.scheduled_for, utc(22, 10, 5, 0));
    assert_eq!(run.fire_at, utc(22, 10, 4, 50));
}

#[test]
fn every_ten_minutes_is_late_by_the_same_share_of_half_the_gap() {
    assert_eq!(
        jitters("*/10 * * * *", &NO_JITTER_ID, 3),
        [TimeDelta::zero(); 3]
    );
    assert_eq!(
        jitters("*/10 * * * *", &HALF_ID, 3),
        [TimeDelta::milliseconds(150_000); 3]
    );
    let latest = jitters("*/10 * * * *", &MAX_ID, 1)[0];
    assert!(latest < TimeDelta::minutes(5) && latest > TimeDelta::seconds(299));
}

#[test]
fn hourly_is_late_by_at_most_thirty_minutes() {
    assert_eq!(
        jitters("0 * * * *", &HALF_ID, 3),
        [TimeDelta::minutes(15); 3]
    );
    let latest = jitters("0 * * * *", &MAX_ID, 1)[0];
    assert!(latest < TimeDelta::minutes(30) && latest > TimeDelta::seconds(1799));
}

#[test]
fn daily_is_capped_at_thirty_minutes() {
    assert_eq!(
        jitters("0 9 * * *", &MAX_ID, 2),
        [TimeDelta::minutes(30); 2]
    );
    assert_eq!(
        jitters("0 9 * * *", &HALF_ID, 2),
        [TimeDelta::minutes(30); 2]
    );
    assert_eq!(
        jitters("0 9 * * *", &SMALL_ID, 2),
        [TimeDelta::milliseconds(168_750); 2]
    );
    let runs = runs("0 9 * * *", &HALF_ID, utc(22, 10, 0, 0), 2);
    assert_eq!(runs[0].fire_at, utc(23, 9, 30, 0));
    assert_eq!(runs[1].fire_at, utc(24, 9, 30, 0));
}

#[test]
fn uneven_runs_are_late_by_a_share_of_their_own_gap() {
    assert_eq!(
        jitters("0,15 * * * *", &HALF_ID, 4),
        [
            TimeDelta::seconds(675),
            TimeDelta::seconds(225),
            TimeDelta::seconds(675),
            TimeDelta::seconds(225)
        ]
    );
}

#[test]
fn one_shot_on_the_hour_or_half_hour_fires_up_to_ninety_seconds_early() {
    let created = utc(22, 10, 0, 0);
    let on_the_hour = one_shot("0 15 * * *", &HALF_ID, created);
    assert_eq!(on_the_hour.scheduled_for, utc(22, 15, 0, 0));
    assert_eq!(on_the_hour.jitter(), TimeDelta::seconds(-45));
    assert_eq!(on_the_hour.fire_at, utc(22, 14, 59, 15));

    assert_eq!(
        one_shot("30 9 * * *", &HALF_ID, created).jitter(),
        TimeDelta::seconds(-45)
    );
    let earliest = one_shot("0 15 * * *", &MAX_ID, created).jitter();
    assert!(earliest > TimeDelta::seconds(-90) && earliest < TimeDelta::seconds(-89));
    assert_eq!(
        one_shot("0 15 * * *", &NO_JITTER_ID, created).jitter(),
        TimeDelta::zero()
    );
}

#[test]
fn one_shot_on_other_minutes_fires_on_time() {
    let created = utc(22, 10, 0, 0);
    for expression in ["3 9 * * *", "15 15 * * *", "45 10 * * *", "59 23 * * *"] {
        assert_eq!(
            one_shot(expression, &MAX_ID, created).jitter(),
            TimeDelta::zero(),
            "{expression}"
        );
    }
}

#[test]
fn one_shot_never_fires_before_it_was_created() {
    let created = utc(22, 14, 59, 30);
    let run = one_shot("0 15 * * *", &MAX_ID, created);
    assert_eq!(run.scheduled_for, utc(22, 15, 0, 0));
    assert_eq!(run.fire_at, created);
}

#[test]
fn one_shot_rounding_uses_local_minutes() {
    // Kolkata is UTC+05:30, so local 09:00 is 03:30 UTC and local 09:15 is 03:45 UTC.
    let created = Kolkata.with_ymd_and_hms(2026, 10, 22, 8, 0, 0).unwrap();
    let schedule = Schedule::parse("0 9 * * *").unwrap();
    let local_hour = next_run(&schedule, TaskKind::OneShot, &HALF_ID, &created).unwrap();
    assert_eq!(local_hour.jitter(), TimeDelta::seconds(-45));

    let schedule = Schedule::parse("15 9 * * *").unwrap();
    let local_quarter = next_run(&schedule, TaskKind::OneShot, &HALF_ID, &created).unwrap();
    assert_eq!(local_quarter.jitter(), TimeDelta::zero());
}

#[test]
fn the_same_id_always_gets_the_same_jitter() {
    let task_id = generate_task_id();
    for expression in ["*/10 * * * *", "0 * * * *", "0,15 * * * *", "0 9 * * *"] {
        assert_eq!(
            runs(expression, &task_id, utc(22, 10, 0, 0), 3),
            runs(expression, &task_id, utc(22, 10, 0, 0), 3),
            "{expression}"
        );
    }
    assert_eq!(
        one_shot("0 15 * * *", &task_id, utc(22, 10, 0, 0)),
        one_shot("0 15 * * *", &task_id, utc(22, 10, 0, 0))
    );
}

#[test]
fn jitter_reads_only_the_last_bits_of_the_id() {
    let same_tail = Uuid::from_u128(0xffff_ffff_ffff_7fff_bfff_ffff_8000_0000);
    for expression in ["*/10 * * * *", "0 * * * *", "0 9 * * *"] {
        assert_eq!(
            jitters(expression, &same_tail, 2),
            jitters(expression, &HALF_ID, 2),
            "{expression}"
        );
    }
}

#[test]
fn generated_ids_are_distinct_version_seven_uuids() {
    let first = generate_task_id();
    let second = generate_task_id();
    for task_id in [first, second] {
        assert_eq!(task_id.get_version_num(), 7, "{task_id}");
    }
    assert_ne!(first, second);
}

#[test]
fn run_for_recomputes_a_stored_scheduled_time() {
    let schedule = Schedule::parse("0 9 * * *").unwrap();
    let created = utc(22, 10, 0, 0);
    let next = next_run(&schedule, TaskKind::Recurring, &HALF_ID, &created).unwrap();
    let restored = run_for(
        &schedule,
        TaskKind::Recurring,
        &HALF_ID,
        next.scheduled_for,
        &created,
    )
    .unwrap();
    assert_eq!(restored, next);

    let later = run_for(
        &schedule,
        TaskKind::Recurring,
        &HALF_ID,
        utc(25, 9, 0, 0),
        &created,
    )
    .unwrap();
    assert_eq!(later.fire_at, utc(25, 9, 30, 0));
}

#[test]
fn run_for_never_fires_before_not_before() {
    let schedule = Schedule::parse("0 15 * * *").unwrap();
    let run = run_for(
        &schedule,
        TaskKind::OneShot,
        &HALF_ID,
        utc(22, 15, 0, 0),
        &utc(22, 14, 59, 50),
    )
    .unwrap();
    assert_eq!(run.fire_at, utc(22, 14, 59, 50));

    let resumed = run_for(
        &schedule,
        TaskKind::Recurring,
        &HALF_ID,
        utc(22, 15, 0, 0),
        &utc(22, 15, 20, 0),
    )
    .unwrap();
    assert_eq!(resumed.fire_at, utc(22, 15, 30, 0));
}

#[test]
fn run_for_reports_jitter_outside_the_representable_range() {
    let schedule = Schedule::parse("0 0 * * *").unwrap();
    let earliest = DateTime::<Utc>::MIN_UTC;
    assert_eq!(
        run_for(&schedule, TaskKind::OneShot, &HALF_ID, earliest, &earliest),
        None
    );
    assert_eq!(
        run_for(
            &schedule,
            TaskKind::OneShot,
            &NO_JITTER_ID,
            earliest,
            &earliest
        ),
        Some(Run {
            scheduled_for: earliest,
            fire_at: earliest
        })
    );
}
