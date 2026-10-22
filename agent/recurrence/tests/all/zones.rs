use chrono::{DateTime, NaiveTime, Utc, Weekday};
use tickloop_agent_recurrence::{ParseError, Recurrence, RecurrenceParts, Tz, Zone};

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
fn iana_zones_print_by_name_and_parse_back() {
    for (input, expected) in [
        ("daily 09:31 Asia/Shanghai", "daily 09:31 Asia/Shanghai"),
        (
            "weekly fri,mon 08:00 America/New_York",
            "weekly mon,fri 08:00 America/New_York",
        ),
        ("daily 09:00 UTC", "daily 09:00 UTC"),
        (
            "at 2026-10-20T00:00:00Z Asia/Shanghai",
            "at 2026-10-20T08:00:00+08:00 Asia/Shanghai",
        ),
        (
            "at 2026-07-01T12:00:00Z America/New_York",
            "at 2026-07-01T08:00:00-04:00 America/New_York",
        ),
    ] {
        let parsed = rule(input);
        assert_eq!(parsed.to_string(), expected, "{input}");
        assert_eq!(rule(expected), parsed, "{input}");
    }
}

#[test]
fn a_daily_time_in_an_iana_zone_runs_at_that_local_time() {
    assert_eq!(
        next("daily 09:31 Asia/Shanghai", "2026-10-22T00:00:00Z"),
        Some(utc("2026-10-22T01:31:00Z"))
    );
    assert_eq!(
        next("daily 09:31 Asia/Shanghai", "2026-10-22T01:31:00Z"),
        Some(utc("2026-10-23T01:31:00Z"))
    );
}

#[test]
fn weekly_times_follow_daylight_saving() {
    // New York leaves daylight saving on Sunday 2026-11-01.
    let weekly = "weekly mon,fri 08:00 America/New_York";
    assert_eq!(
        next(weekly, "2026-10-30T00:00:00Z"),
        Some(utc("2026-10-30T12:00:00Z"))
    );
    assert_eq!(
        next(weekly, "2026-10-30T12:00:00Z"),
        Some(utc("2026-11-02T13:00:00Z"))
    );
    assert_eq!(
        rule(weekly)
            .latest_after_previous(utc("2026-10-30T12:00:00Z"), utc("2026-11-02T14:00:00Z")),
        Some(utc("2026-11-02T13:00:00Z"))
    );
}

#[test]
fn a_skipped_local_time_runs_just_after_the_jump() {
    // New York's clocks jump from 02:00 to 03:00 on 2026-03-08.
    let daily = "daily 02:30 America/New_York";
    assert_eq!(
        next(daily, "2026-03-08T05:00:00Z"),
        Some(utc("2026-03-08T07:30:00Z"))
    );
    assert_eq!(
        next(daily, "2026-03-08T07:30:00Z"),
        Some(utc("2026-03-09T06:30:00Z"))
    );
    assert_eq!(
        rule(daily).latest_after_previous(utc("2026-03-07T07:30:00Z"), utc("2026-03-08T08:00:00Z")),
        Some(utc("2026-03-08T07:30:00Z"))
    );
}

#[test]
fn a_repeated_local_time_runs_once_at_its_first_occurrence() {
    // 01:30 happens twice in New York on 2026-11-01: at 05:30Z and at 06:30Z.
    let daily = "daily 01:30 America/New_York";
    assert_eq!(
        next(daily, "2026-11-01T04:00:00Z"),
        Some(utc("2026-11-01T05:30:00Z"))
    );
    assert_eq!(
        next(daily, "2026-11-01T05:30:00Z"),
        Some(utc("2026-11-02T06:30:00Z"))
    );
}

#[test]
fn fixed_offsets_still_ignore_daylight_saving() {
    assert_eq!(
        next("weekly mon,fri 08:00 -04:00", "2026-10-30T12:00:00Z"),
        Some(utc("2026-11-02T12:00:00Z"))
    );
}

#[test]
fn a_zone_with_daylight_saving_may_shorten_a_day_by_an_hour() {
    use std::time::Duration;
    assert_eq!(
        rule("daily 09:00 America/New_York").shortest_gap(),
        Some(Duration::from_secs(23 * 3_600))
    );
    assert_eq!(
        rule("weekly mon,thu 09:00 Asia/Shanghai").shortest_gap(),
        Some(Duration::from_secs(3 * 86_400 - 3_600))
    );
    assert_eq!(
        rule("daily 09:00 +08:00").shortest_gap(),
        Some(Duration::from_secs(86_400))
    );
}

#[test]
fn parts_take_a_rule_apart() {
    let nine_thirty = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    assert_eq!(
        rule("every 90m").parts(),
        RecurrenceParts::Every { seconds: 5_400 }
    );
    assert_eq!(
        rule("weekly fri,mon 09:30 America/New_York").parts(),
        RecurrenceParts::Weekly {
            days: vec![Weekday::Mon, Weekday::Fri],
            time: nine_thirty,
            zone: Zone::Named(Tz::America__New_York),
        }
    );
    let RecurrenceParts::Daily { zone, .. } = rule("daily 09:30 +05:45").parts() else {
        panic!("a daily rule");
    };
    assert_eq!(zone.to_string(), "+05:45");
    let RecurrenceParts::Daily { zone, .. } = rule("daily 09:30").parts() else {
        panic!("a daily rule");
    };
    assert_eq!(zone.to_string(), "+00:00");
    let RecurrenceParts::At { instant, zone } =
        rule("at 2026-10-20T00:00:00Z Asia/Shanghai").parts()
    else {
        panic!("a one-off rule");
    };
    assert_eq!(instant.to_rfc3339(), "2026-10-20T08:00:00+08:00");
    assert_eq!(zone, Some(Tz::Asia__Shanghai));
}

#[test]
fn zones_parse_offsets_and_iana_names() {
    assert_eq!(
        Zone::parse("Asia/Shanghai"),
        Ok(Zone::Named(Tz::Asia__Shanghai))
    );
    assert_eq!(Zone::parse("+08:00").unwrap().to_string(), "+08:00");
    assert_eq!(
        Zone::parse("Mars/Olympus"),
        Err(ParseError::InvalidOffset("Mars/Olympus".into()))
    );
    assert_eq!(Zone::parse_named("Europe/London"), Ok(Tz::Europe__London));
    assert_eq!(
        Zone::parse_named("+08:00"),
        Err(ParseError::InvalidTimeZone("+08:00".into()))
    );
    assert_eq!(
        Zone::parse_named("asia/shanghai"),
        Err(ParseError::InvalidTimeZone("asia/shanghai".into()))
    );
}

#[test]
fn unknown_zones_are_refused() {
    assert_eq!(
        Recurrence::parse("daily 09:00 Mars/Olympus"),
        Err(ParseError::InvalidOffset("Mars/Olympus".into()))
    );
    assert_eq!(
        Recurrence::parse("weekly mon 09:00 Asia/Nowhere"),
        Err(ParseError::InvalidOffset("Asia/Nowhere".into()))
    );
    assert_eq!(
        Recurrence::parse("at 2026-10-20T00:00:00Z +08:00"),
        Err(ParseError::InvalidTimeZone("+08:00".into()))
    );
    assert!(matches!(
        Recurrence::parse("daily 09:00 Asia/Shanghai extra"),
        Err(ParseError::WrongArity { rule: "daily", .. })
    ));
}

#[test]
fn a_time_skipped_at_a_jump_over_midnight_runs_after_it_the_next_day() {
    // Nuuk's clocks jump from 23:00 on Saturday 2026-03-28 (-02:00) to 00:00 on Sunday
    // (-01:00): Saturday's 23:30 runs at 00:30 on Sunday, 01:30Z.
    for rule_text in ["daily 23:30 America/Nuuk", "weekly sat 23:30 America/Nuuk"] {
        assert_eq!(
            next(rule_text, "2026-03-29T01:10:00Z"),
            Some(utc("2026-03-29T01:30:00Z")),
            "{rule_text}"
        );
        assert_eq!(
            rule(rule_text)
                .latest_after_previous(utc("2026-03-28T00:00:00Z"), utc("2026-03-29T01:40:00Z")),
            Some(utc("2026-03-29T01:30:00Z")),
            "{rule_text}"
        );
    }
    assert_eq!(
        next("daily 23:30 America/Nuuk", "2026-03-29T01:30:00Z"),
        Some(utc("2026-03-30T00:30:00Z"))
    );
    assert_eq!(
        next("weekly sat 23:30 America/Nuuk", "2026-03-29T01:30:00Z"),
        Some(utc("2026-04-05T00:30:00Z"))
    );
}
