//! Wall-clock recurrence rules of Loop Triggers.
//!
//! - `every <N>m|h|d`: every N minutes, hours or days from the previous run.
//! - `daily HH:MM [±HH:MM]`: every day at that wall-clock time in the given UTC offset (default UTC).
//! - `weekly <day>[,<day>…] HH:MM [±HH:MM]`: on the listed weekdays (`mon`..`sun`) at that time.
//! - `at <RFC 3339 time>`: once.
//!
//! Everything is computed in UTC with fixed offsets: there is no local timezone and no
//! daylight-saving database, so a rule means the same instants on every host. Missed runs
//! are skipped, never caught up: the next run is always strictly after the given instant.

use std::fmt;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Days, Duration, FixedOffset, NaiveTime, SecondsFormat, TimeZone, Utc,
    Weekday,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MINUTES_PER_HOUR: u32 = 60;
const MINUTES_PER_DAY: u32 = 24 * MINUTES_PER_HOUR;
const MIN_INTERVAL_MINUTES: u32 = 1;
const MAX_INTERVAL_DAYS: u32 = 366;
const MAX_INTERVAL_MINUTES: u32 = MAX_INTERVAL_DAYS * MINUTES_PER_DAY;
const MAX_OFFSET_MINUTES: u32 = 14 * MINUTES_PER_HOUR;
const DAYS_PER_WEEK: u64 = 7;
const ALL_WEEKDAYS: WeekdaySet = WeekdaySet(0b111_1111);
const ERROR_EXCERPT_CHARS: usize = 40;
const USAGE: &str = "use `every 30m`, `every 2h`, `every 1d`, `daily 09:00 +08:00`, \
                     `weekly mon,fri 09:00 +08:00` or `at <RFC 3339 time>`";

const WEEKDAY_NAMES: [(&str, Weekday); 7] = [
    ("mon", Weekday::Mon),
    ("tue", Weekday::Tue),
    ("wed", Weekday::Wed),
    ("thu", Weekday::Thu),
    ("fri", Weekday::Fri),
    ("sat", Weekday::Sat),
    ("sun", Weekday::Sun),
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("recurrence is empty; {USAGE}")]
    Empty,
    #[error("`{0}` is not a recurrence rule; {USAGE}")]
    UnknownRule(String),
    #[error("wrong number of words for `{rule}`; expected `{expected}`")]
    WrongArity {
        rule: &'static str,
        expected: &'static str,
    },
    #[error("`{0}` is not an interval like 30m, 2h or 1d")]
    InvalidInterval(String),
    #[error(
        "interval `{0}` is out of range; it must be between {MIN_INTERVAL_MINUTES} minute and {MAX_INTERVAL_DAYS} days"
    )]
    IntervalOutOfRange(String),
    #[error("`{0}` is not a 24-hour time like 09:00")]
    InvalidTime(String),
    #[error("`{0}` is not a UTC offset between -14:00 and +14:00 like +08:00")]
    InvalidOffset(String),
    #[error("`{0}` is not a weekday; use mon, tue, wed, thu, fri, sat or sun")]
    InvalidWeekday(String),
    #[error("weekday `{0}` is listed more than once")]
    DuplicateWeekday(String),
    #[error("`{0}` is not an RFC 3339 time like 2026-10-20T08:00:00+08:00")]
    InvalidInstant(String),
}

/// A parsed recurrence rule. Its `Display` and serde form is the canonical text, which
/// parses back to an equal value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Recurrence(Rule);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Rule {
    Every {
        minutes: u32,
    },
    Daily {
        time: NaiveTime,
        offset: FixedOffset,
    },
    Weekly {
        days: WeekdaySet,
        time: NaiveTime,
        offset: FixedOffset,
    },
    At(DateTime<FixedOffset>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WeekdaySet(u8);

impl WeekdaySet {
    fn bit(day: Weekday) -> u8 {
        1 << day.num_days_from_monday()
    }

    fn contains(self, day: Weekday) -> bool {
        self.0 & Self::bit(day) != 0
    }
}

impl Recurrence {
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let Some(rule_word) = words.first() else {
            return Err(ParseError::Empty);
        };
        let arguments = &words[1..];
        let rule = match rule_word.to_ascii_lowercase().as_str() {
            "every" => match arguments {
                [interval] => Rule::Every {
                    minutes: parse_interval(interval)?,
                },
                _ => return Err(wrong_arity("every", "every <N>m|h|d")),
            },
            "daily" => match arguments {
                [time] => Rule::Daily {
                    time: parse_time(time)?,
                    offset: utc_offset(),
                },
                [time, offset] => Rule::Daily {
                    time: parse_time(time)?,
                    offset: parse_offset(offset)?,
                },
                _ => return Err(wrong_arity("daily", "daily HH:MM [±HH:MM]")),
            },
            "weekly" => match arguments {
                [days, time] => Rule::Weekly {
                    days: parse_weekdays(days)?,
                    time: parse_time(time)?,
                    offset: utc_offset(),
                },
                [days, time, offset] => Rule::Weekly {
                    days: parse_weekdays(days)?,
                    time: parse_time(time)?,
                    offset: parse_offset(offset)?,
                },
                _ => {
                    return Err(wrong_arity(
                        "weekly",
                        "weekly <day>[,<day>…] HH:MM [±HH:MM]",
                    ));
                }
            },
            "at" => match arguments {
                [instant] => Rule::At(
                    DateTime::parse_from_rfc3339(instant)
                        .map_err(|_| ParseError::InvalidInstant(excerpt(instant)))?,
                ),
                _ => return Err(wrong_arity("at", "at <RFC 3339 time>")),
            },
            _ => return Err(ParseError::UnknownRule(excerpt(rule_word))),
        };
        Ok(Self(rule))
    }

    /// The first run strictly after `after`, or `None` when the rule has no further runs.
    /// For `every`, `after` is taken as the previous run.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self.0 {
            Rule::Every { minutes } => after.checked_add_signed(interval(minutes)),
            Rule::Daily { time, offset } => next_wall_clock(after, ALL_WEEKDAYS, time, offset),
            Rule::Weekly { days, time, offset } => next_wall_clock(after, days, time, offset),
            Rule::At(instant) => {
                let instant = instant.with_timezone(&Utc);
                (instant > after).then_some(instant)
            }
        }
    }

    /// The first run strictly after `after` on the grid anchored at the `previous` run.
    /// For `every`, runs missed between `previous` and `after` are skipped and the
    /// result stays a whole number of intervals after `previous`; every other rule has
    /// its own grid and behaves as [`Recurrence::next_after`].
    pub fn next_after_previous(
        &self,
        previous: DateTime<Utc>,
        after: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let Rule::Every { minutes } = self.0 else {
            return self.next_after(after);
        };
        let step = interval(minutes);
        let first = previous.checked_add_signed(step)?;
        if first > after {
            return Some(first);
        }
        let step_seconds = step.num_seconds();
        let elapsed_seconds = after.signed_duration_since(previous).num_seconds();
        let steps = elapsed_seconds / step_seconds + 1;
        let offset = Duration::try_seconds(steps.checked_mul(step_seconds)?)?;
        previous.checked_add_signed(offset)
    }
}

impl fmt::Display for Recurrence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Rule::Every { minutes } => {
                if minutes % MINUTES_PER_DAY == 0 {
                    write!(formatter, "every {}d", minutes / MINUTES_PER_DAY)
                } else if minutes % MINUTES_PER_HOUR == 0 {
                    write!(formatter, "every {}h", minutes / MINUTES_PER_HOUR)
                } else {
                    write!(formatter, "every {minutes}m")
                }
            }
            Rule::Daily { time, offset } => {
                write!(formatter, "daily {}", time.format("%H:%M"))?;
                write_offset(formatter, offset)
            }
            Rule::Weekly { days, time, offset } => {
                let names: Vec<&str> = WEEKDAY_NAMES
                    .iter()
                    .filter(|(_, day)| days.contains(*day))
                    .map(|(name, _)| *name)
                    .collect();
                write!(
                    formatter,
                    "weekly {} {}",
                    names.join(","),
                    time.format("%H:%M")
                )?;
                write_offset(formatter, offset)
            }
            Rule::At(instant) => write!(
                formatter,
                "at {}",
                instant.to_rfc3339_opts(SecondsFormat::AutoSi, true)
            ),
        }
    }
}

impl FromStr for Recurrence {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for Recurrence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Recurrence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

fn next_wall_clock(
    after: DateTime<Utc>,
    days: WeekdaySet,
    time: NaiveTime,
    offset: FixedOffset,
) -> Option<DateTime<Utc>> {
    let offset_duration = Duration::seconds(i64::from(offset.local_minus_utc()));
    let after_utc = after.naive_utc();
    let local_date = after_utc.checked_add_signed(offset_duration)?.date();
    // The candidate on the local date of `after` may already be past, so a full week plus
    // one day always contains the next allowed weekday.
    (0..=DAYS_PER_WEEK).find_map(|day_index| {
        let date = local_date.checked_add_days(Days::new(day_index))?;
        if !days.contains(date.weekday()) {
            return None;
        }
        let candidate = date.and_time(time).checked_sub_signed(offset_duration)?;
        (candidate > after_utc).then(|| Utc.from_utc_datetime(&candidate))
    })
}

fn interval(minutes: u32) -> Duration {
    Duration::minutes(i64::from(minutes))
}

fn utc_offset() -> FixedOffset {
    FixedOffset::east_opt(0).expect("zero is a valid offset")
}

fn wrong_arity(rule: &'static str, expected: &'static str) -> ParseError {
    ParseError::WrongArity { rule, expected }
}

fn excerpt(text: &str) -> String {
    let mut characters = text.chars();
    let mut shortened: String = characters.by_ref().take(ERROR_EXCERPT_CHARS).collect();
    if characters.next().is_some() {
        shortened.push('…');
    }
    shortened
}

fn parse_interval(text: &str) -> Result<u32, ParseError> {
    let invalid = || ParseError::InvalidInterval(excerpt(text));
    let unit_minutes = match text.chars().last() {
        Some('m' | 'M') => 1,
        Some('h' | 'H') => MINUTES_PER_HOUR,
        Some('d' | 'D') => MINUTES_PER_DAY,
        _ => return Err(invalid()),
    };
    let count = &text[..text.len() - 1];
    if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let out_of_range = || ParseError::IntervalOutOfRange(excerpt(text));
    let minutes = count
        .parse::<u32>()
        .ok()
        .and_then(|count| count.checked_mul(unit_minutes))
        .ok_or_else(out_of_range)?;
    if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&minutes) {
        return Err(out_of_range());
    }
    Ok(minutes)
}

/// Two ASCII digits each side of a colon: `HH:MM`.
fn split_two_digit_pair(text: &str) -> Option<(u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let two_digits = |high: u8, low: u8| {
        (high.is_ascii_digit() && low.is_ascii_digit())
            .then(|| u32::from(high - b'0') * 10 + u32::from(low - b'0'))
    };
    Some((
        two_digits(bytes[0], bytes[1])?,
        two_digits(bytes[3], bytes[4])?,
    ))
}

fn parse_time(text: &str) -> Result<NaiveTime, ParseError> {
    split_two_digit_pair(text)
        .and_then(|(hours, minutes)| NaiveTime::from_hms_opt(hours, minutes, 0))
        .ok_or_else(|| ParseError::InvalidTime(excerpt(text)))
}

fn parse_offset(text: &str) -> Result<FixedOffset, ParseError> {
    let invalid = || ParseError::InvalidOffset(excerpt(text));
    let (sign, rest) = if let Some(rest) = text.strip_prefix('+') {
        (1, rest)
    } else if let Some(rest) = text.strip_prefix('-') {
        (-1, rest)
    } else {
        return Err(invalid());
    };
    let (hours, minutes) = split_two_digit_pair(rest).ok_or_else(invalid)?;
    let total_minutes = hours * MINUTES_PER_HOUR + minutes;
    if minutes >= MINUTES_PER_HOUR || total_minutes > MAX_OFFSET_MINUTES {
        return Err(invalid());
    }
    let seconds = sign * i32::try_from(total_minutes * 60).map_err(|_| invalid())?;
    FixedOffset::east_opt(seconds).ok_or_else(invalid)
}

fn parse_weekdays(text: &str) -> Result<WeekdaySet, ParseError> {
    let mut days = WeekdaySet(0);
    for name in text.split(',') {
        let day = WEEKDAY_NAMES
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, day)| *day)
            .ok_or_else(|| ParseError::InvalidWeekday(excerpt(name)))?;
        if days.contains(day) {
            return Err(ParseError::DuplicateWeekday(excerpt(name)));
        }
        days.0 |= WeekdaySet::bit(day);
    }
    Ok(days)
}

fn write_offset(formatter: &mut fmt::Formatter<'_>, offset: FixedOffset) -> fmt::Result {
    let seconds = offset.local_minus_utc();
    if seconds == 0 {
        return Ok(());
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    write!(
        formatter,
        " {sign}{:02}:{:02}",
        minutes / MINUTES_PER_HOUR,
        minutes % MINUTES_PER_HOUR
    )
}
