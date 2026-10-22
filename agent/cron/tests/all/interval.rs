use tickloop_agent_cron::{Interval, IntervalError, LoopArguments, Schedule, parse_loop_arguments};

fn minutes(count: u32) -> Interval {
    Interval::from_minutes(count).unwrap()
}

#[test]
fn parses_compact_spaced_and_every_forms() {
    for (text, expected) in [
        ("30s", 1),
        ("45s", 1),
        ("60s", 1),
        ("61s", 2),
        ("90s", 2),
        ("5m", 5),
        ("2h", 120),
        ("1d", 1440),
        ("20min", 20),
        ("2 hours", 120),
        ("every 2 hours", 120),
        ("every 20m", 20),
        ("Every 5 Minutes", 5),
        ("  every 1 day ", 1440),
        ("every 30 seconds", 1),
        ("3 hrs", 180),
    ] {
        assert_eq!(Interval::parse(text), Ok(minutes(expected)), "{text}");
    }
    assert_eq!("15m".parse(), Ok(minutes(15)));
}

#[test]
fn rejects_text_that_is_not_an_interval() {
    for text in [
        "",
        "m",
        "5",
        "5x",
        "every",
        "every PR",
        "5 m m",
        "five minutes",
        "-5m",
        "5.5m",
        "every every 5m",
    ] {
        assert_eq!(
            Interval::parse(text),
            Err(IntervalError::Invalid(text.trim().to_string())),
            "{text}"
        );
    }
    assert_eq!(Interval::parse("0m"), Err(IntervalError::Zero("0m".into())));
    assert_eq!(
        Interval::parse("every 0 hours"),
        Err(IntervalError::Zero("every 0 hours".into()))
    );
    assert_eq!(
        Interval::parse("99999999999999999999d"),
        Err(IntervalError::TooLong("99999999999999999999d".into()))
    );
    assert_eq!(
        Interval::parse("9999999999d"),
        Err(IntervalError::TooLong("9999999999d".into()))
    );
}

#[test]
fn converts_intervals_to_cron_rounding_uneven_ones_to_the_nearest_even_step() {
    for (requested, actual, expression) in [
        ("30s", "1m", "* * * * *"),
        ("45s", "1m", "* * * * *"),
        ("5m", "5m", "*/5 * * * *"),
        ("7m", "6m", "*/6 * * * *"),
        ("8m", "10m", "*/10 * * * *"),
        ("10m", "10m", "*/10 * * * *"),
        ("25m", "30m", "*/30 * * * *"),
        ("30m", "30m", "*/30 * * * *"),
        ("45m", "1h", "0 * * * *"),
        ("60m", "1h", "0 * * * *"),
        ("90m", "2h", "0 */2 * * *"),
        ("every 2 hours", "2h", "0 */2 * * *"),
        ("5h", "6h", "0 */6 * * *"),
        ("7h", "8h", "0 */8 * * *"),
        ("9h", "8h", "0 */8 * * *"),
        ("12h", "12h", "0 */12 * * *"),
        ("18h", "1d", "0 0 * * *"),
        ("24h", "1d", "0 0 * * *"),
        ("1d", "1d", "0 0 * * *"),
        ("3d", "1d", "0 0 * * *"),
    ] {
        let converted = Interval::parse(requested).unwrap().to_cron();
        assert_eq!(converted.actual.to_string(), actual, "{requested}");
        assert_eq!(converted.expression, expression, "{requested}");
        assert_eq!(
            converted.is_rounded(),
            converted.requested != converted.actual,
            "{requested}"
        );
        Schedule::parse(&converted.expression).unwrap();
    }
    let seven = minutes(7).to_cron();
    assert_eq!(seven.requested, minutes(7));
    assert!(seven.is_rounded());
    assert!(!minutes(5).to_cron().is_rounded());
}

#[test]
fn displays_intervals_in_the_largest_whole_unit() {
    assert_eq!(minutes(1).to_string(), "1m");
    assert_eq!(minutes(90).to_string(), "90m");
    assert_eq!(minutes(120).to_string(), "2h");
    assert_eq!(minutes(1440).to_string(), "1d");
    assert_eq!(minutes(2160).to_string(), "36h");
}

fn arguments<'a>(interval: Option<&str>, prompt: &'a str) -> LoopArguments<'a> {
    LoopArguments {
        interval: interval.map(|text| Interval::parse(text).unwrap()),
        prompt,
    }
}

#[test]
fn splits_loop_arguments_like_claude_code() {
    for (input, interval, prompt) in [
        ("5m /babysit-prs", Some("5m"), "/babysit-prs"),
        ("  30m   check the deploy ", Some("30m"), "check the deploy"),
        (
            "check the deploy every 20m",
            Some("20m"),
            "check the deploy",
        ),
        ("run tests every 5 minutes", Some("5m"), "run tests"),
        (
            "check the build every 2 hours",
            Some("2h"),
            "check the build",
        ),
        ("check the deploy", None, "check the deploy"),
        ("check every PR", None, "check every PR"),
        ("5 minutes of checks", None, "5 minutes of checks"),
        ("15M check", None, "15M check"),
        ("30m", Some("30m"), ""),
        ("every 2 hours", Some("2h"), ""),
        ("", None, ""),
        ("5m check every 2 hours", Some("5m"), "check every 2 hours"),
    ] {
        assert_eq!(
            parse_loop_arguments(input),
            Ok(arguments(interval, prompt)),
            "{input}"
        );
    }
}

#[test]
fn loop_arguments_with_a_zero_interval_are_an_error() {
    assert_eq!(
        parse_loop_arguments("0m check"),
        Err(IntervalError::Zero("0m".into()))
    );
    assert_eq!(
        parse_loop_arguments("check every 0 minutes"),
        Err(IntervalError::Zero("every 0 minutes".into()))
    );
}
