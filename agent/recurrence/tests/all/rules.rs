use chrono::{DateTime, Utc};
use tickloop_agent_recurrence::Recurrence;

fn utc(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn rule(text: &str) -> Recurrence {
    Recurrence::parse(text).unwrap()
}

fn next(text: &str, after: &str) -> Option<DateTime<Utc>> {
    rule(text).next_after(utc(after))
}

#[test]
fn every_adds_the_interval_to_the_previous_run() {
    assert_eq!(
        next("every 30m", "2026-10-19T10:00:00Z"),
        Some(utc("2026-10-19T10:30:00Z"))
    );
    assert_eq!(
        next("every 2h", "2026-10-19T10:07:13Z"),
        Some(utc("2026-10-19T12:07:13Z"))
    );
    assert_eq!(
        next("every 1d", "2026-10-19T23:59:00Z"),
        Some(utc("2026-10-20T23:59:00Z"))
    );
    assert_eq!(
        next("every 366d", "2026-01-01T00:00:00Z"),
        Some(utc("2027-01-02T00:00:00Z"))
    );
}

#[test]
fn every_skips_missed_runs_and_stays_on_the_previous_runs_grid() {
    let every = rule("every 30m");
    let previous = utc("2026-10-19T10:00:00Z");
    assert_eq!(
        every.next_after_previous(previous, utc("2026-10-19T10:10:00Z")),
        Some(utc("2026-10-19T10:30:00Z"))
    );
    // Asleep for hours: one run on the grid, nothing caught up.
    assert_eq!(
        every.next_after_previous(previous, utc("2026-10-19T13:45:00Z")),
        Some(utc("2026-10-19T14:00:00Z"))
    );
    // Exactly on a grid point: strictly after it.
    assert_eq!(
        every.next_after_previous(previous, utc("2026-10-19T14:00:00Z")),
        Some(utc("2026-10-19T14:30:00Z"))
    );
    assert_eq!(
        every.next_after_previous(previous, utc("2026-10-19T14:00:00.5Z")),
        Some(utc("2026-10-19T14:30:00Z"))
    );
    // Clock earlier than the previous run: still one interval after it.
    assert_eq!(
        every.next_after_previous(previous, utc("2026-10-19T09:00:00Z")),
        Some(utc("2026-10-19T10:30:00Z"))
    );
}

#[test]
fn rules_other_than_every_ignore_the_previous_run() {
    let previous = utc("2000-01-01T00:00:00Z");
    for text in [
        "daily 09:00 +08:00",
        "weekly mon,thu 18:30 -05:00",
        "at 2026-10-20T08:00:00+08:00",
    ] {
        let recurrence = rule(text);
        let after = utc("2026-10-19T12:00:00Z");
        assert_eq!(
            recurrence.next_after_previous(previous, after),
            recurrence.next_after(after),
            "{text}"
        );
    }
}

#[test]
fn daily_defaults_to_utc() {
    assert_eq!(
        next("daily 09:00", "2026-10-19T08:59:59Z"),
        Some(utc("2026-10-19T09:00:00Z"))
    );
    assert_eq!(
        next("daily 09:00", "2026-10-19T09:00:00Z"),
        Some(utc("2026-10-20T09:00:00Z"))
    );
    assert_eq!(
        next("daily 23:15", "2026-10-19T23:20:00Z"),
        Some(utc("2026-10-20T23:15:00Z"))
    );
    assert_eq!(
        next("daily 00:00", "2026-12-31T23:59:00Z"),
        Some(utc("2027-01-01T00:00:00Z"))
    );
}

#[test]
fn daily_uses_the_offset_for_the_local_date_and_time() {
    // 00:30 UTC is 08:30 in UTC+8, so today's 09:00 local is still ahead.
    assert_eq!(
        next("daily 09:00 +08:00", "2026-10-19T00:30:00Z"),
        Some(utc("2026-10-19T01:00:00Z"))
    );
    assert_eq!(
        next("daily 09:00 +08:00", "2026-10-19T01:00:00Z"),
        Some(utc("2026-10-20T01:00:00Z"))
    );
    // 20:00 UTC on the 19th is already the 20th at 04:00 in UTC+8.
    assert_eq!(
        next("daily 09:00 +08:00", "2026-10-19T20:00:00Z"),
        Some(utc("2026-10-20T01:00:00Z"))
    );
    // Negative offset whose local time is still the previous day.
    assert_eq!(
        next("daily 22:00 -05:00", "2026-10-20T02:00:00Z"),
        Some(utc("2026-10-20T03:00:00Z"))
    );
    assert_eq!(
        next("daily 05:45 +05:45", "2026-10-19T00:00:00Z"),
        Some(utc("2026-10-19T00:00:00Z") + chrono::Duration::days(1))
    );
    assert_eq!(
        next("daily 00:00 +14:00", "2026-10-19T09:00:00Z"),
        Some(utc("2026-10-19T10:00:00Z"))
    );
    assert_eq!(
        next("daily 23:59 -14:00", "2026-10-19T00:00:00Z"),
        Some(utc("2026-10-19T13:59:00Z"))
    );
}

#[test]
fn daily_skips_missed_days_without_catching_up() {
    assert_eq!(
        next("daily 09:00 +08:00", "2026-10-25T03:00:00Z"),
        Some(utc("2026-10-26T01:00:00Z"))
    );
}

#[test]
fn fixed_offsets_ignore_daylight_saving() {
    // US daylight saving ends on 2026-11-01; a fixed -05:00 rule keeps 14:00 UTC.
    for (after, expected) in [
        ("2026-10-31T12:00:00Z", "2026-10-31T14:00:00Z"),
        ("2026-11-01T12:00:00Z", "2026-11-01T14:00:00Z"),
        ("2026-11-02T12:00:00Z", "2026-11-02T14:00:00Z"),
        ("2026-03-08T12:00:00Z", "2026-03-08T14:00:00Z"),
    ] {
        assert_eq!(next("daily 09:00 -05:00", after), Some(utc(expected)));
    }
}

#[test]
fn weekly_runs_on_listed_days_in_the_offsets_calendar() {
    // 2026-10-19 is a Monday.
    let mon_fri = "weekly mon,fri 09:00 +08:00";
    assert_eq!(
        next(mon_fri, "2026-10-19T00:00:00Z"),
        Some(utc("2026-10-19T01:00:00Z"))
    );
    assert_eq!(
        next(mon_fri, "2026-10-19T01:00:00Z"),
        Some(utc("2026-10-23T01:00:00Z"))
    );
    assert_eq!(
        next(mon_fri, "2026-10-23T01:00:00Z"),
        Some(utc("2026-10-26T01:00:00Z"))
    );
    // Sunday 20:00 UTC is already Monday 04:00 in UTC+8.
    assert_eq!(
        next(mon_fri, "2026-10-25T20:00:00Z"),
        Some(utc("2026-10-26T01:00:00Z"))
    );
    // Monday 02:00 UTC is still Sunday 21:00 in UTC-5.
    assert_eq!(
        next("weekly sun 22:00 -05:00", "2026-10-26T02:00:00Z"),
        Some(utc("2026-10-26T03:00:00Z"))
    );
}

#[test]
fn weekly_on_a_single_day_waits_a_full_week_after_its_run() {
    assert_eq!(
        next("weekly mon 09:00", "2026-10-19T09:00:00Z"),
        Some(utc("2026-10-26T09:00:00Z"))
    );
    assert_eq!(
        next("weekly mon 09:00", "2026-10-19T08:59:00Z"),
        Some(utc("2026-10-19T09:00:00Z"))
    );
    assert_eq!(
        next("weekly sun 23:59", "2026-10-19T00:00:00Z"),
        Some(utc("2026-10-25T23:59:00Z"))
    );
}

#[test]
fn weekly_with_every_day_matches_daily() {
    let weekly = rule("weekly mon,tue,wed,thu,fri,sat,sun 07:30 +03:30");
    let daily = rule("daily 07:30 +03:30");
    let mut after = utc("2026-10-19T00:00:00Z");
    for _ in 0..30 {
        let expected = daily.next_after(after);
        assert_eq!(weekly.next_after(after), expected);
        after = expected.unwrap();
    }
}

#[test]
fn successive_runs_stay_on_the_grid() {
    let mut after = utc("2026-10-19T00:00:00Z");
    let weekly = rule("weekly tue,sat 06:00 -03:00");
    let mut runs = Vec::new();
    for _ in 0..4 {
        after = weekly.next_after(after).unwrap();
        runs.push(after);
    }
    assert_eq!(
        runs,
        [
            utc("2026-10-20T09:00:00Z"),
            utc("2026-10-24T09:00:00Z"),
            utc("2026-10-27T09:00:00Z"),
            utc("2026-10-31T09:00:00Z"),
        ]
    );
}

#[test]
fn at_runs_once() {
    let at = rule("at 2026-10-20T08:00:00+08:00");
    assert_eq!(
        at.next_after(utc("2026-10-19T00:00:00Z")),
        Some(utc("2026-10-20T00:00:00Z"))
    );
    assert_eq!(
        at.next_after(utc("2026-10-19T23:59:59.999Z")),
        Some(utc("2026-10-20T00:00:00Z"))
    );
    assert_eq!(at.next_after(utc("2026-10-20T00:00:00Z")), None);
    assert_eq!(at.next_after(utc("2027-01-01T00:00:00Z")), None);
}

#[test]
fn no_run_beyond_the_representable_range() {
    assert_eq!(rule("every 1d").next_after(DateTime::<Utc>::MAX_UTC), None);
    assert_eq!(
        rule("every 1d").next_after_previous(DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC),
        None
    );
    assert_eq!(
        rule("daily 23:59 -14:00").next_after(DateTime::<Utc>::MAX_UTC),
        None
    );
    assert_eq!(
        rule("weekly mon 00:00 +14:00").next_after(DateTime::<Utc>::MAX_UTC),
        None
    );
    assert!(
        rule("daily 00:00 +14:00")
            .next_after(DateTime::<Utc>::MIN_UTC)
            .is_some()
    );
}

#[test]
fn every_counts_seconds() {
    let every = rule("every 1s");
    let previous = utc("2026-10-19T10:00:00.500Z");
    assert_eq!(
        every.next_after_previous(previous, previous),
        Some(utc("2026-10-19T10:00:01.500Z"))
    );
    assert_eq!(
        every.next_after_previous(previous, utc("2026-10-19T10:00:05.200Z")),
        Some(utc("2026-10-19T10:00:05.500Z"))
    );
    assert_eq!(
        rule("every 90s").next_after(utc("2026-10-19T10:00:00Z")),
        Some(utc("2026-10-19T10:01:30Z"))
    );
}

#[test]
fn the_latest_missed_run_is_the_one_taken() {
    let every = rule("every 30m");
    let previous = utc("2026-10-19T10:00:00Z");
    assert_eq!(
        every.latest_after_previous(previous, utc("2026-10-19T10:29:59Z")),
        None
    );
    assert_eq!(
        every.latest_after_previous(previous, utc("2026-10-19T10:30:00Z")),
        Some(utc("2026-10-19T10:30:00Z"))
    );
    // Three hours late: one run, the latest on the grid.
    assert_eq!(
        every.latest_after_previous(previous, utc("2026-10-19T13:10:00Z")),
        Some(utc("2026-10-19T13:00:00Z"))
    );
    assert_eq!(
        every.latest_after_previous(previous, utc("2026-10-19T09:00:00Z")),
        None
    );

    let daily = rule("daily 09:00 +08:00");
    assert_eq!(
        daily.latest_after_previous(utc("2026-10-17T01:00:00Z"), utc("2026-10-19T00:59:59Z")),
        Some(utc("2026-10-18T01:00:00Z"))
    );
    assert_eq!(
        daily.latest_after_previous(utc("2026-10-18T01:00:00Z"), utc("2026-10-19T00:59:59Z")),
        None
    );
    assert_eq!(
        rule("weekly mon,fri 09:00")
            .latest_after_previous(utc("2026-10-01T00:00:00Z"), utc("2026-10-21T12:00:00Z")),
        Some(utc("2026-10-19T09:00:00Z"))
    );

    let at = rule("at 2026-10-20T08:00:00+08:00");
    assert_eq!(
        at.latest_after_previous(utc("2026-10-19T00:00:00Z"), utc("2026-10-20T00:00:00Z")),
        Some(utc("2026-10-20T00:00:00Z"))
    );
    assert_eq!(
        at.latest_after_previous(utc("2026-10-20T00:00:00Z"), utc("2026-10-21T00:00:00Z")),
        None
    );
}

#[test]
fn the_shortest_gap_is_what_a_host_floor_holds_against() {
    use std::time::Duration;
    assert_eq!(
        rule("every 1s").shortest_gap(),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        rule("every 2h").shortest_gap(),
        Some(Duration::from_secs(7_200))
    );
    assert_eq!(
        rule("daily 09:00").shortest_gap(),
        Some(Duration::from_secs(86_400))
    );
    assert_eq!(
        rule("weekly mon,fri 09:00").shortest_gap(),
        Some(Duration::from_secs(3 * 86_400))
    );
    assert_eq!(
        rule("weekly sun 09:00").shortest_gap(),
        Some(Duration::from_secs(7 * 86_400))
    );
    assert_eq!(rule("at 2026-10-20T08:00:00Z").shortest_gap(), None);
    assert_eq!(rule("every 90s").interval(), Some(Duration::from_secs(90)));
    assert_eq!(rule("daily 09:00").interval(), None);
}
