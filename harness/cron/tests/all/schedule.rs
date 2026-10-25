use chrono::{DateTime, NaiveDate, TimeDelta, TimeZone, Utc};
use chrono_tz::Africa::Monrovia;
use chrono_tz::America::New_York;
use chrono_tz::Tz;
use tickloop_harness_cron::{Schedule, ScheduleError};

fn schedule(expression: &str) -> Schedule {
    Schedule::parse(expression).unwrap()
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap()
}

fn new_york(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Tz> {
    New_York
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .earliest()
        .unwrap()
}

fn next_runs(schedule: &Schedule, after: DateTime<Utc>, count: usize) -> Vec<DateTime<Utc>> {
    std::iter::successors(schedule.next_after(&after), |run| schedule.next_after(run))
        .take(count)
        .collect()
}

#[test]
fn parses_every_supported_field_syntax() {
    let after = utc(2026, 10, 22, 10, 0);
    assert_eq!(
        schedule("* * * * *").next_after(&after),
        Some(utc(2026, 10, 22, 10, 1))
    );
    assert_eq!(
        schedule("5 * * * *").next_after(&after),
        Some(utc(2026, 10, 22, 10, 5))
    );
    assert_eq!(
        next_runs(&schedule("*/20 * * * *"), after, 3),
        [
            utc(2026, 10, 22, 10, 20),
            utc(2026, 10, 22, 10, 40),
            utc(2026, 10, 22, 11, 0)
        ]
    );
    assert_eq!(
        next_runs(&schedule("58-59,1 * * * *"), utc(2026, 10, 22, 10, 57), 3),
        [
            utc(2026, 10, 22, 10, 58),
            utc(2026, 10, 22, 10, 59),
            utc(2026, 10, 22, 11, 1)
        ]
    );
    assert_eq!(
        next_runs(&schedule("0 1-9/4 * * *"), after, 3),
        [
            utc(2026, 10, 23, 1, 0),
            utc(2026, 10, 23, 5, 0),
            utc(2026, 10, 23, 9, 0)
        ]
    );
}

#[test]
fn normalizes_whitespace_in_the_expression() {
    let parsed = schedule("  */5\t*  * * *  ");
    assert_eq!(parsed.expression(), "*/5 * * * *");
    assert_eq!(parsed.to_string(), "*/5 * * * *");
    assert_eq!(parsed, "*/5 * * * *".parse().unwrap());
}

#[test]
fn rejects_the_wrong_number_of_fields() {
    assert_eq!(Schedule::parse(""), Err(ScheduleError::FieldCount(0)));
    assert_eq!(
        Schedule::parse("* * * *"),
        Err(ScheduleError::FieldCount(4))
    );
    assert_eq!(
        Schedule::parse("0 * * * * *"),
        Err(ScheduleError::FieldCount(6))
    );
    assert_eq!(
        Schedule::parse("0 0 * * * * 2026"),
        Err(ScheduleError::FieldCount(7))
    );
}

#[test]
fn rejects_extensions_and_names() {
    for (expression, field) in [
        ("0 9 L * *", "day-of-month"),
        ("0 9 15W * *", "day-of-month"),
        ("0 9 ? * 1", "day-of-month"),
        ("0 9 * * 5L", "day-of-week"),
        ("0 9 * * 1#2", "day-of-week"),
        ("0 9 * * MON", "day-of-week"),
        ("0 9 * JAN *", "month"),
        ("@hourly * * * *", "minute"),
        ("0 9 * * +1", "day-of-week"),
    ] {
        assert!(
            matches!(
                Schedule::parse(expression),
                Err(ScheduleError::UnsupportedSyntax { field: found, .. }) if found == field
            ),
            "{expression}"
        );
    }
    assert!(matches!(
        Schedule::parse("@hourly"),
        Err(ScheduleError::FieldCount(1))
    ));
}

#[test]
fn rejects_out_of_range_and_malformed_values() {
    for expression in [
        "60 * * * *",
        "* 24 * * *",
        "* * 0 * *",
        "* * 32 * *",
        "* * * 0 *",
        "* * * 13 *",
        "* * * * 8",
        "*/0 * * * *",
        "5-1 * * * *",
        "1- * * * *",
        "-1 * * * *",
        ",5 * * * *",
        "5, * * * *",
        "/5 * * * *",
        "5/ * * * *",
        "*-5 * * * *",
    ] {
        assert!(
            matches!(
                Schedule::parse(expression),
                Err(ScheduleError::Invalid { .. })
            ),
            "{expression}"
        );
    }
}

#[test]
fn day_of_week_seven_is_sunday() {
    let saturday = utc(2026, 10, 24, 12, 0);
    let sunday_nine = utc(2026, 10, 25, 9, 0);
    assert_eq!(
        schedule("0 9 * * 7").next_after(&saturday),
        Some(sunday_nine)
    );
    assert_eq!(
        schedule("0 9 * * 0").next_after(&saturday),
        Some(sunday_nine)
    );
    assert_eq!(
        next_runs(&schedule("0 9 * * 5-7"), utc(2026, 10, 22, 12, 0), 4),
        [
            utc(2026, 10, 23, 9, 0),
            utc(2026, 10, 24, 9, 0),
            utc(2026, 10, 25, 9, 0),
            utc(2026, 10, 30, 9, 0)
        ]
    );
}

#[test]
fn restricted_day_of_month_and_day_of_week_match_either() {
    // 2026-10-22 is a Thursday; November 1st is a Sunday.
    let thursday = utc(2026, 10, 22, 12, 0);
    assert_eq!(
        next_runs(&schedule("0 9 1 * 1"), thursday, 3),
        [
            utc(2026, 10, 26, 9, 0),
            utc(2026, 11, 1, 9, 0),
            utc(2026, 11, 2, 9, 0)
        ]
    );
    assert_eq!(
        next_runs(&schedule("0 9 * * 1"), thursday, 2),
        [utc(2026, 10, 26, 9, 0), utc(2026, 11, 2, 9, 0)]
    );
    assert_eq!(
        next_runs(&schedule("0 9 1 * *"), thursday, 2),
        [utc(2026, 11, 1, 9, 0), utc(2026, 12, 1, 9, 0)]
    );
}

#[test]
fn next_after_is_strict_and_next_at_or_after_is_not() {
    let hourly = schedule("0 * * * *");
    let ten = utc(2026, 10, 22, 10, 0);
    assert_eq!(hourly.next_after(&ten), Some(utc(2026, 10, 22, 11, 0)));
    assert_eq!(hourly.next_at_or_after(&ten), Some(ten));

    let just_after_ten = ten + TimeDelta::milliseconds(500);
    assert_eq!(
        hourly.next_at_or_after(&just_after_ten),
        Some(utc(2026, 10, 22, 11, 0))
    );
    let mid_minute = ten + TimeDelta::seconds(30);
    assert_eq!(
        schedule("* * * * *").next_at_or_after(&mid_minute),
        Some(utc(2026, 10, 22, 10, 1))
    );
}

#[test]
fn an_expression_that_never_matches_has_no_next_time() {
    let never = schedule("0 0 30 2 *");
    assert_eq!(never.next_after(&utc(2026, 10, 22, 10, 0)), None);
    assert_eq!(never.gap_after(&utc(2026, 10, 22, 10, 0)), None);
}

#[test]
fn gap_after_measures_real_time_to_the_following_run() {
    let uneven = schedule("0,15 * * * *");
    assert_eq!(
        uneven.gap_after(&utc(2026, 10, 22, 10, 0)),
        Some(TimeDelta::minutes(15))
    );
    assert_eq!(
        uneven.gap_after(&utc(2026, 10, 22, 10, 15)),
        Some(TimeDelta::minutes(45))
    );
    let daily = schedule("0 9 * * *");
    assert_eq!(
        daily.gap_after(&new_york(2026, 3, 7, 9, 0)),
        Some(TimeDelta::hours(23))
    );
    assert_eq!(
        daily.gap_after(&new_york(2026, 10, 31, 9, 0)),
        Some(TimeDelta::hours(25))
    );
    assert_eq!(
        daily.gap_after(&new_york(2026, 10, 22, 9, 0)),
        Some(TimeDelta::hours(24))
    );
}

#[test]
fn uses_the_time_zone_of_the_given_time() {
    let morning = schedule("0 9 * * *");
    let after = new_york(2026, 10, 22, 10, 0);
    let next = morning.next_after(&after).unwrap();
    assert_eq!(next, new_york(2026, 10, 23, 9, 0));
    assert_eq!(next.with_timezone(&Utc), utc(2026, 10, 23, 13, 0));
}

#[test]
fn a_fixed_time_in_a_spring_forward_gap_runs_right_after_the_gap() {
    // New York skips 02:00-03:00 on 2026-03-08.
    let half_past_two = schedule("30 2 * * *");
    let runs: Vec<DateTime<Tz>> = std::iter::successors(
        half_past_two.next_after(&new_york(2026, 3, 7, 3, 0)),
        |run| half_past_two.next_after(run),
    )
    .take(3)
    .collect();
    assert_eq!(
        runs,
        [
            new_york(2026, 3, 8, 3, 0),
            new_york(2026, 3, 9, 2, 30),
            new_york(2026, 3, 10, 2, 30)
        ]
    );
}

#[test]
fn every_hour_skips_the_missing_hour_and_keeps_real_time() {
    let half_past = schedule("30 * * * *");
    let runs: Vec<DateTime<Utc>> =
        std::iter::successors(half_past.next_after(&new_york(2026, 3, 8, 1, 0)), |run| {
            half_past.next_after(run)
        })
        .take(2)
        .map(|run| run.with_timezone(&Utc))
        .collect();
    assert_eq!(runs, [utc(2026, 3, 8, 6, 30), utc(2026, 3, 8, 7, 30)]);
}

#[test]
fn a_fixed_time_in_a_fall_back_overlap_runs_once() {
    // New York repeats 01:00-02:00 on 2026-11-01.
    let half_past_one = schedule("30 1 * * *");
    let first = half_past_one
        .next_after(&new_york(2026, 10, 31, 12, 0))
        .unwrap();
    assert_eq!(first.with_timezone(&Utc), utc(2026, 11, 1, 5, 30));
    let second = half_past_one.next_after(&first).unwrap();
    assert_eq!(second, new_york(2026, 11, 2, 1, 30));
}

#[test]
fn every_hour_runs_in_both_passes_of_a_fall_back_overlap() {
    let half_past = schedule("30 * * * *");
    let runs: Vec<DateTime<Utc>> =
        std::iter::successors(half_past.next_after(&new_york(2026, 11, 1, 0, 0)), |run| {
            half_past.next_after(run)
        })
        .take(3)
        .map(|run| run.with_timezone(&Utc))
        .collect();
    assert_eq!(
        runs,
        [
            utc(2026, 11, 1, 4, 30),
            utc(2026, 11, 1, 5, 30),
            utc(2026, 11, 1, 6, 30)
        ]
    );
}

#[test]
fn local_dates_follow_the_given_zone_not_utc() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 23).unwrap();
    let late_evening = schedule("0 23 * * *")
        .next_after(&new_york(2026, 10, 23, 12, 0))
        .unwrap();
    assert_eq!(late_evening.date_naive(), date);
    assert_eq!(late_evening.with_timezone(&Utc), utc(2026, 10, 24, 3, 0));
}

fn runs_in_utc(expression: &str, after: DateTime<Tz>, count: usize) -> Vec<DateTime<Utc>> {
    let schedule = schedule(expression);
    std::iter::successors(schedule.next_after(&after), |run| schedule.next_after(run))
        .take(count)
        .map(|run| run.with_timezone(&Utc))
        .collect()
}

#[test]
fn a_fixed_hour_list_runs_once_through_a_fall_back_overlap() {
    // 01:30 EDT is 05:30 UTC; 03:30 EST is 08:30 UTC.
    assert_eq!(
        runs_in_utc("30 1,3 * * *", new_york(2026, 11, 1, 0, 0), 3),
        [
            utc(2026, 11, 1, 5, 30),
            utc(2026, 11, 1, 8, 30),
            utc(2026, 11, 2, 6, 30)
        ]
    );
}

#[test]
fn a_fixed_hour_range_runs_once_through_a_fall_back_overlap() {
    // 01:30 EDT is 05:30 UTC; 02:30 EST is 07:30 UTC.
    assert_eq!(
        runs_in_utc("30 1-2 * * *", new_york(2026, 11, 1, 0, 0), 3),
        [
            utc(2026, 11, 1, 5, 30),
            utc(2026, 11, 1, 7, 30),
            utc(2026, 11, 2, 6, 30)
        ]
    );
}

#[test]
fn a_fixed_time_is_not_repeated_when_starting_in_the_second_pass() {
    let second_pass = Utc
        .with_ymd_and_hms(2026, 11, 1, 6, 10, 0)
        .unwrap()
        .with_timezone(&New_York);
    assert_eq!(
        runs_in_utc("30 1,3 * * *", second_pass, 1),
        [utc(2026, 11, 1, 8, 30)]
    );
    let first_pass = schedule("30 1,3 * * *")
        .next_after(&new_york(2026, 11, 1, 0, 0))
        .unwrap();
    assert_eq!(
        schedule("30 1,3 * * *").next_at_or_after(&first_pass),
        Some(first_pass)
    );
}

#[test]
fn a_fixed_hour_list_runs_a_skipped_time_right_after_a_spring_forward_gap() {
    // 03:00 EDT is 07:00 UTC; 04:30 EDT is 08:30 UTC.
    assert_eq!(
        runs_in_utc("30 2,4 * * *", new_york(2026, 3, 8, 0, 0), 3),
        [
            utc(2026, 3, 8, 7, 0),
            utc(2026, 3, 8, 8, 30),
            utc(2026, 3, 9, 6, 30)
        ]
    );
}

#[test]
fn a_fixed_hour_range_runs_a_skipped_time_right_after_a_spring_forward_gap() {
    // 01:30 EST is 06:30 UTC; 02:30 does not exist and runs at 03:00 EDT, 07:00 UTC.
    assert_eq!(
        runs_in_utc("30 1-2 * * *", new_york(2026, 3, 8, 0, 0), 3),
        [
            utc(2026, 3, 8, 6, 30),
            utc(2026, 3, 8, 7, 0),
            utc(2026, 3, 9, 5, 30)
        ]
    );
}

#[test]
fn skipped_fixed_times_and_the_first_real_time_after_the_gap_run_once() {
    // 02:00 and 02:30 do not exist; they and the real 03:00 run once at 03:00 EDT.
    assert_eq!(
        runs_in_utc("0,30 1-3 * * *", new_york(2026, 3, 8, 0, 0), 5),
        [
            utc(2026, 3, 8, 6, 0),
            utc(2026, 3, 8, 6, 30),
            utc(2026, 3, 8, 7, 0),
            utc(2026, 3, 8, 7, 30),
            utc(2026, 3, 9, 5, 0)
        ]
    );
}

#[test]
fn a_query_at_the_end_of_a_spring_forward_gap_includes_the_skipped_times() {
    let gap_end = new_york(2026, 3, 8, 3, 0);
    for expression in ["30 2,4 * * *", "30 2 * * *", "0,30 1-3 * * *"] {
        assert_eq!(
            schedule(expression).next_at_or_after(&gap_end),
            Some(gap_end),
            "{expression}"
        );
    }
    assert_eq!(
        schedule("30 2,4 * * *").next_after(&gap_end),
        Some(new_york(2026, 3, 8, 4, 30))
    );
    assert_eq!(
        schedule("15 4 * * *").next_at_or_after(&gap_end),
        Some(new_york(2026, 3, 8, 4, 15))
    );
}

#[test]
fn a_gap_that_ends_mid_minute_runs_skipped_times_at_its_exact_end() {
    // Monrovia moved from UTC-00:44:30 to UTC at 1972-01-07 00:44:30 UTC, skipping local
    // 00:00:00-00:44:29.
    let gap_end = Utc
        .with_ymd_and_hms(1972, 1, 7, 0, 44, 30)
        .unwrap()
        .with_timezone(&Monrovia);
    let evening = Monrovia
        .with_ymd_and_hms(1972, 1, 6, 12, 0, 0)
        .single()
        .unwrap();
    let midnight = schedule("0 0 * * *");
    assert_eq!(midnight.next_after(&evening), Some(gap_end));
    assert_eq!(midnight.next_at_or_after(&gap_end), Some(gap_end));
}

#[test]
fn shortest_gap_reads_the_closest_two_times() {
    let cases = [
        ("*/5 * * * *", TimeDelta::minutes(5)),
        ("*/30 * * * *", TimeDelta::minutes(30)),
        ("0,30 9 * * *", TimeDelta::minutes(30)),
        ("0 * * * *", TimeDelta::hours(1)),
        ("0 0,23 * * *", TimeDelta::hours(1)),
        ("59 23 * * *", TimeDelta::days(1)),
        ("0 9 * * 1-5", TimeDelta::days(1)),
        ("0 9 * * 1", TimeDelta::days(7)),
        ("0 9 1 * *", TimeDelta::days(28)),
    ];
    for (expression, gap) in cases {
        assert_eq!(
            Schedule::parse(expression).unwrap().shortest_gap(),
            Some(gap),
            "{expression}"
        );
    }
    assert_eq!(Schedule::parse("0 0 30 2 *").unwrap().shortest_gap(), None);
}
