use chrono::{DateTime, Utc};
use tickloop_agent_recurrence::{ParseError, Recurrence};

fn error(text: &str) -> ParseError {
    Recurrence::parse(text).unwrap_err()
}

#[test]
fn rejects_unknown_rules_and_wrong_arity() {
    assert_eq!(error(""), ParseError::Empty);
    assert_eq!(error("   \t "), ParseError::Empty);
    assert_eq!(error("hourly"), ParseError::UnknownRule("hourly".into()));
    assert!(matches!(
        error("every"),
        ParseError::WrongArity { rule: "every", .. }
    ));
    assert!(matches!(
        error("every 5m 6m"),
        ParseError::WrongArity { rule: "every", .. }
    ));
    assert!(matches!(
        error("daily"),
        ParseError::WrongArity { rule: "daily", .. }
    ));
    assert!(matches!(
        error("daily 09:00 +08:00 extra"),
        ParseError::WrongArity { rule: "daily", .. }
    ));
    assert!(matches!(
        error("weekly mon"),
        ParseError::WrongArity { rule: "weekly", .. }
    ));
    assert!(matches!(
        error("at"),
        ParseError::WrongArity { rule: "at", .. }
    ));
    assert!(matches!(
        error("at 2026-10-20 08:00:00Z"),
        ParseError::WrongArity { rule: "at", .. }
    ));
}

#[test]
fn rejects_zero_overflowing_and_malformed_intervals() {
    for text in [
        "0s",
        "0m",
        "31622401s",
        "0h",
        "0d",
        "367d",
        "8785h",
        "527041m",
        "4294967295m",
        "99999999999999999999d",
    ] {
        assert_eq!(
            error(&format!("every {text}")),
            ParseError::IntervalOutOfRange(text.into()),
            "{text}"
        );
    }
    for text in [
        "5", "m", "5x", "5é", "é", "+5m", "-5m", "5.5h", "5 m", "1_0m", "٣m", "5mm",
    ] {
        assert!(
            matches!(
                Recurrence::parse(&format!("every {text}")),
                Err(ParseError::InvalidInterval(_) | ParseError::WrongArity { .. })
            ),
            "{text}"
        );
    }
}

#[test]
fn rejects_invalid_times_offsets_and_weekdays() {
    for time in [
        "24:00",
        "23:60",
        "9:00",
        "09:0",
        "0900",
        "09-00",
        "+9:00",
        "０９:００",
        "09:00:00",
    ] {
        assert_eq!(
            error(&format!("daily {time}")),
            ParseError::InvalidTime(time.into()),
            "{time}"
        );
    }
    for offset in [
        "08:00",
        "+8:00",
        "+08",
        "+0800",
        "+14:01",
        "-15:00",
        "+08:60",
        "++8:00",
        "+-8:00",
        "Z",
        "+08:00:00",
    ] {
        assert_eq!(
            error(&format!("daily 09:00 {offset}")),
            ParseError::InvalidOffset(offset.into()),
            "{offset}"
        );
    }
    assert_eq!(
        error("weekly monday 09:00"),
        ParseError::InvalidWeekday("monday".into())
    );
    assert_eq!(
        error("weekly mon, 09:00"),
        ParseError::InvalidWeekday("".into())
    );
    assert_eq!(
        error("weekly 09:00 mon"),
        ParseError::InvalidWeekday("09:00".into())
    );
    assert_eq!(
        error("weekly mon,fri,Mon 09:00"),
        ParseError::DuplicateWeekday("Mon".into())
    );
    assert_eq!(
        error("weekly mon 25:00"),
        ParseError::InvalidTime("25:00".into())
    );
    assert_eq!(
        error("weekly mon 09:00 +15:00"),
        ParseError::InvalidOffset("+15:00".into())
    );
}

#[test]
fn rejects_invalid_instants() {
    for instant in [
        "tomorrow",
        "2026-10-20",
        "2026-10-20T08:00:00",
        "2026-13-01T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "2026-10-20T08:00:00+25:00",
    ] {
        assert_eq!(
            error(&format!("at {instant}")),
            ParseError::InvalidInstant(instant.into()),
            "{instant}"
        );
    }
}

#[test]
fn messages_explain_the_problem_and_shorten_long_input() {
    assert_eq!(
        error("every 0m").to_string(),
        "interval `0m` is out of range; it must be between 1 second and 366 days"
    );
    assert_eq!(
        error("daily 09:00 +15:00").to_string(),
        "`+15:00` is not a UTC offset between -14:00 and +14:00 like +08:00 or an IANA time zone \
         like Asia/Shanghai"
    );
    assert!(
        error("hourly")
            .to_string()
            .contains("weekly mon,fri 09:00 America/New_York")
    );

    let long_word = "界".repeat(1000);
    let message = error(&long_word).to_string();
    assert!(
        message.contains(&format!("{}…", "界".repeat(40))),
        "{message}"
    );
    assert!(message.chars().count() < 300, "{message}");
}

/// Deterministic xorshift generator, so failures reproduce without extra dependencies.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

const RULE_WORDS: &[&str] = &["every", "daily", "weekly", "at", "EVERY", "hourly", "", "é"];

const ARGUMENTS: &[&str] = &[
    "1s",
    "45s",
    "1m",
    "30m",
    "60m",
    "90h",
    "366d",
    "367d",
    "0m",
    "mon",
    "mon,fri",
    "sun,sat,wed",
    "mon,mon",
    "09:00",
    "23:59",
    "24:00",
    "+08:00",
    "-14:00",
    "+14:01",
    "-05:30",
    "2026-10-20T08:00:00Z",
    "9999-12-31T23:59:59+14:00",
    "0000-01-01T00:00:00-14:00",
    "2016-12-31T23:59:60Z",
];

const FRAGMENTS: &[&str] = &[
    "s",
    "m",
    "h",
    "d",
    "0",
    "1",
    "9",
    "60",
    "99999999999999999999",
    "-",
    "+",
    ":",
    ",",
    "mon",
    "Fri",
    "T",
    "Z",
    ".",
    "é",
    "界",
    "🦀",
    "\u{0}",
    "\u{200b}",
    "٣",
];

const SEPARATORS: &[&str] = &[" ", "  ", "\t", "\n"];

fn pick<'a>(random: &mut Random, choices: &[&'a str]) -> &'a str {
    choices[random.below(choices.len())]
}

fn random_rule_text(random: &mut Random) -> String {
    let mut text = pick(random, RULE_WORDS).to_string();
    for _ in 0..random.below(5) {
        text.push_str(pick(random, SEPARATORS));
        if random.below(2) == 0 {
            text.push_str(pick(random, ARGUMENTS));
        } else {
            for _ in 0..=random.below(3) {
                text.push_str(pick(random, FRAGMENTS));
            }
        }
    }
    text
}

#[test]
fn random_input_never_panics_and_valid_rules_round_trip() {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let probes = [
        DateTime::<Utc>::MIN_UTC,
        DateTime::<Utc>::MAX_UTC,
        DateTime::parse_from_rfc3339("2026-10-19T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc),
    ];
    let mut accepted = 0;
    for _ in 0..50_000 {
        let text = random_rule_text(&mut random);
        let Ok(recurrence) = Recurrence::parse(&text) else {
            continue;
        };
        accepted += 1;
        let printed = recurrence.to_string();
        assert_eq!(Recurrence::parse(&printed), Ok(recurrence), "{text:?}");
        for after in probes {
            if let Some(run) = recurrence.next_after(after) {
                assert!(run > after, "{text:?} after {after}");
            }
            for previous in probes {
                if let Some(run) = recurrence.next_after_previous(previous, after) {
                    assert!(run > after, "{text:?} after {after} from {previous}");
                }
                if let Some(run) = recurrence.latest_after_previous(previous, after) {
                    assert!(
                        run > previous && run <= after,
                        "{text:?} until {after} from {previous}"
                    );
                }
            }
        }
    }
    assert!(accepted > 100, "only {accepted} random rules parsed");

    for _ in 0..20_000 {
        let bytes: Vec<u8> = (0..random.below(24)).map(|_| random.next() as u8).collect();
        let _ = Recurrence::parse(&String::from_utf8_lossy(&bytes));
    }
}
