//! Wall-clock recurrence rules of Loop Triggers.
//!
//! - `every <N>s|m|h|d`: every N seconds, minutes, hours or days from the previous run.
//! - `daily HH:MM [<zone>]`: every day at that wall-clock time in the zone (default UTC).
//! - `weekly <day>[,<day>…] HH:MM [<zone>]`: on the listed weekdays (`mon`..`sun`) at that time.
//! - `at <RFC 3339 time> [<IANA zone>]`: once; the zone only says where the time is shown.
//!
//! A zone is a fixed UTC offset (`+08:00`) or an IANA time zone (`Asia/Shanghai`) from the
//! time zone database built into this crate, never the host's local zone, so a rule means the
//! same instants on every host. In an IANA zone a wall-clock time follows daylight saving: a
//! time the clocks skip runs as far after the jump as it was after the time before it (02:30
//! becomes 03:30), and a time they repeat runs once, at its first occurrence. Missed runs
//! are skipped, never caught up: the next run is always strictly after the given instant.
//! The rules allow runs one second apart; a host sets its own floor with
//! [`Recurrence::shortest_gap`].

use std::fmt;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, NaiveTime,
    Offset, SecondsFormat, TimeZone, Utc, Weekday,
};
pub use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SECONDS_PER_MINUTE: u32 = 60;
const MINUTES_PER_HOUR: u32 = 60;
const SECONDS_PER_HOUR: u32 = MINUTES_PER_HOUR * SECONDS_PER_MINUTE;
const MINUTES_PER_DAY: u32 = 24 * MINUTES_PER_HOUR;
const SECONDS_PER_DAY: u32 = MINUTES_PER_DAY * SECONDS_PER_MINUTE;
const MIN_INTERVAL_SECONDS: u32 = 1;
const MAX_INTERVAL_DAYS: u32 = 366;
const MAX_INTERVAL_SECONDS: u32 = MAX_INTERVAL_DAYS * SECONDS_PER_DAY;
const MAX_OFFSET_MINUTES: u32 = 14 * MINUTES_PER_HOUR;
const DAYS_PER_WEEK: u64 = 7;
const ALL_WEEKDAYS: WeekdaySet = WeekdaySet(0b111_1111);
const ERROR_EXCERPT_CHARS: usize = 40;
const USAGE: &str = "use `every 30s`, `every 30m`, `every 2h`, `every 1d`, \
                     `daily 09:00 Asia/Shanghai`, `weekly mon,fri 09:00 America/New_York`, \
                     `daily 09:00 +08:00` or `at <RFC 3339 time>`";

const AT_USAGE: &str = "at <RFC 3339 time> [<IANA zone>]";

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
    #[error("`{0}` is not an interval like 30s, 30m, 2h or 1d")]
    InvalidInterval(String),
    #[error(
        "interval `{0}` is out of range; it must be between {MIN_INTERVAL_SECONDS} second and {MAX_INTERVAL_DAYS} days"
    )]
    IntervalOutOfRange(String),
    #[error("`{0}` is not a 24-hour time like 09:00")]
    InvalidTime(String),
    #[error(
        "`{0}` is not a UTC offset between -14:00 and +14:00 like +08:00 or an IANA time zone \
         like Asia/Shanghai"
    )]
    InvalidOffset(String),
    #[error("`{0}` is not an IANA time zone like Asia/Shanghai")]
    InvalidTimeZone(String),
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
        seconds: u32,
    },
    Daily {
        time: NaiveTime,
        zone: Zone,
    },
    Weekly {
        days: WeekdaySet,
        time: NaiveTime,
        zone: Zone,
    },
    At {
        instant: DateTime<FixedOffset>,
        zone: Option<Tz>,
    },
}

/// Where a wall-clock rule reads its times: a fixed UTC offset, or an IANA time zone whose
/// offset follows daylight saving. Its `Display` is the text a rule names it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    Offset(FixedOffset),
    Named(Tz),
}

impl Zone {
    /// A UTC offset such as `+08:00`, or an IANA time zone name such as `Asia/Shanghai`.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        if text.starts_with(['+', '-']) {
            return parse_offset(text).map(Self::Offset);
        }
        text.parse::<Tz>()
            .map(Self::Named)
            .map_err(|_| ParseError::InvalidOffset(excerpt(text)))
    }

    /// An IANA time zone name such as `Asia/Shanghai`; offsets are refused.
    pub fn parse_named(text: &str) -> Result<Tz, ParseError> {
        text.parse::<Tz>()
            .map_err(|_| ParseError::InvalidTimeZone(excerpt(text)))
    }

    /// The UTC time of the local wall-clock time `local`. A local time the clocks skip moves
    /// forward by the length of the jump; one they repeat is its first occurrence.
    fn to_utc(self, local: NaiveDateTime) -> Option<NaiveDateTime> {
        let offset = match self {
            Self::Offset(offset) => offset,
            Self::Named(zone) => match zone.offset_from_local_datetime(&local) {
                LocalResult::Single(offset) | LocalResult::Ambiguous(offset, _) => offset.fix(),
                // In a gap: the offset in force before it, a day earlier, carries the time past it.
                LocalResult::None => zone
                    .offset_from_local_datetime(&local.checked_sub_signed(Duration::days(1))?)
                    .earliest()?
                    .fix(),
            },
        };
        local.checked_sub_signed(Duration::seconds(i64::from(offset.local_minus_utc())))
    }

    fn to_local(self, utc: NaiveDateTime) -> Option<NaiveDateTime> {
        match self {
            Self::Offset(offset) => {
                utc.checked_add_signed(Duration::seconds(i64::from(offset.local_minus_utc())))
            }
            Self::Named(zone) => Some(zone.from_utc_datetime(&utc).naive_local()),
        }
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offset(offset) => {
                let seconds = offset.local_minus_utc();
                let sign = if seconds < 0 { '-' } else { '+' };
                let minutes = seconds.unsigned_abs() / 60;
                write!(
                    formatter,
                    "{sign}{:02}:{:02}",
                    minutes / MINUTES_PER_HOUR,
                    minutes % MINUTES_PER_HOUR
                )
            }
            Self::Named(zone) => formatter.write_str(zone.name()),
        }
    }
}

impl FromStr for Zone {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// A rule taken apart, for a host that presents or edits its pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceParts {
    Every {
        seconds: u32,
    },
    Daily {
        time: NaiveTime,
        zone: Zone,
    },
    /// The weekdays Monday first.
    Weekly {
        days: Vec<Weekday>,
        time: NaiveTime,
        zone: Zone,
    },
    /// `instant` carries the zone's offset at that time when the rule names a zone.
    At {
        instant: DateTime<FixedOffset>,
        zone: Option<Tz>,
    },
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

    /// The days in the set, Monday first.
    fn listed(self) -> impl Iterator<Item = (&'static str, Weekday)> {
        WEEKDAY_NAMES
            .into_iter()
            .filter(move |(_, day)| self.contains(*day))
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
                    seconds: parse_interval(interval)?,
                },
                _ => return Err(wrong_arity("every", "every <N>s|m|h|d")),
            },
            "daily" => match arguments {
                [time] => Rule::Daily {
                    time: parse_time(time)?,
                    zone: utc_zone(),
                },
                [time, zone] => Rule::Daily {
                    time: parse_time(time)?,
                    zone: Zone::parse(zone)?,
                },
                _ => return Err(wrong_arity("daily", "daily HH:MM [±HH:MM|<IANA zone>]")),
            },
            "weekly" => match arguments {
                [days, time] => Rule::Weekly {
                    days: parse_weekdays(days)?,
                    time: parse_time(time)?,
                    zone: utc_zone(),
                },
                [days, time, zone] => Rule::Weekly {
                    days: parse_weekdays(days)?,
                    time: parse_time(time)?,
                    zone: Zone::parse(zone)?,
                },
                _ => {
                    return Err(wrong_arity(
                        "weekly",
                        "weekly <day>[,<day>…] HH:MM [±HH:MM|<IANA zone>]",
                    ));
                }
            },
            "at" => match arguments {
                [instant] => Rule::At {
                    instant: parse_instant(instant)?,
                    zone: None,
                },
                // A first word that is no time is a time split in two, such as a space for `T`.
                [instant, zone] => match DateTime::parse_from_rfc3339(instant) {
                    Ok(instant) => {
                        let zone = Zone::parse_named(zone)?;
                        Rule::At {
                            instant: instant.with_timezone(&zone).fixed_offset(),
                            zone: Some(zone),
                        }
                    }
                    Err(_) => return Err(wrong_arity("at", AT_USAGE)),
                },
                _ => return Err(wrong_arity("at", AT_USAGE)),
            },
            _ => return Err(ParseError::UnknownRule(excerpt(rule_word))),
        };
        Ok(Self(rule))
    }

    /// The first run strictly after `after`, or `None` when the rule has no further runs.
    /// For `every`, `after` is taken as the previous run.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self.0 {
            Rule::Every { seconds } => after.checked_add_signed(interval(seconds)),
            Rule::Daily { time, zone } => next_wall_clock(after, ALL_WEEKDAYS, time, zone),
            Rule::Weekly { days, time, zone } => next_wall_clock(after, days, time, zone),
            Rule::At { instant, .. } => {
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
        let Rule::Every { seconds } = self.0 else {
            return self.next_after(after);
        };
        let step = interval(seconds);
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

    /// The latest run strictly after `previous` and no later than `until`, on the grid
    /// anchored at the `previous` run as in [`Recurrence::next_after_previous`]; `None` when no
    /// run falls in between. When several runs were missed, this is the one a host that skips
    /// missed runs takes.
    pub fn latest_after_previous(
        &self,
        previous: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let latest = match self.0 {
            Rule::Every { seconds } => {
                let step_seconds = i64::from(seconds);
                let elapsed_seconds = until.signed_duration_since(previous).num_seconds();
                if elapsed_seconds < step_seconds {
                    return None;
                }
                let steps = elapsed_seconds / step_seconds;
                previous.checked_add_signed(Duration::try_seconds(steps * step_seconds)?)?
            }
            Rule::Daily { time, zone } => previous_wall_clock(until, ALL_WEEKDAYS, time, zone)?,
            Rule::Weekly { days, time, zone } => previous_wall_clock(until, days, time, zone)?,
            Rule::At { instant, .. } => instant.with_timezone(&Utc),
        };
        (latest > previous && latest <= until).then_some(latest)
    }

    /// The shortest time between two runs of the rule, to hold against a host's floor; `None`
    /// for a rule that runs once. In an IANA zone a daylight-saving jump may shorten a day by
    /// up to an hour.
    pub fn shortest_gap(&self) -> Option<std::time::Duration> {
        let (shortest_days, zone) = match self.0 {
            Rule::Every { seconds } => {
                return Some(std::time::Duration::from_secs(u64::from(seconds)));
            }
            Rule::Daily { zone, .. } => (1, zone),
            Rule::Weekly { days, zone, .. } => {
                let listed: Vec<u32> = days
                    .listed()
                    .map(|(_, day)| day.num_days_from_monday())
                    .collect();
                let wrap = listed.first()? + DAYS_PER_WEEK as u32 - listed.last()?;
                let shortest_days = listed
                    .windows(2)
                    .map(|pair| pair[1] - pair[0])
                    .fold(wrap, u32::min);
                (shortest_days, zone)
            }
            Rule::At { .. } => return None,
        };
        let daylight_saving_slack = match zone {
            Zone::Offset(_) => 0,
            Zone::Named(_) => SECONDS_PER_HOUR,
        };
        Some(std::time::Duration::from_secs(u64::from(
            shortest_days * SECONDS_PER_DAY - daylight_saving_slack,
        )))
    }

    /// The rule's pieces.
    pub fn parts(&self) -> RecurrenceParts {
        match self.0 {
            Rule::Every { seconds } => RecurrenceParts::Every { seconds },
            Rule::Daily { time, zone } => RecurrenceParts::Daily { time, zone },
            Rule::Weekly { days, time, zone } => RecurrenceParts::Weekly {
                days: days.listed().map(|(_, day)| day).collect(),
                time,
                zone,
            },
            Rule::At { instant, zone } => RecurrenceParts::At { instant, zone },
        }
    }

    /// The interval of an `every` rule; `None` for the rules on a wall-clock grid.
    pub fn interval(&self) -> Option<std::time::Duration> {
        match self.0 {
            Rule::Every { seconds } => Some(std::time::Duration::from_secs(u64::from(seconds))),
            _ => None,
        }
    }
}

impl fmt::Display for Recurrence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Rule::Every { seconds } => {
                if seconds.is_multiple_of(SECONDS_PER_DAY) {
                    write!(formatter, "every {}d", seconds / SECONDS_PER_DAY)
                } else if seconds.is_multiple_of(SECONDS_PER_HOUR) {
                    write!(formatter, "every {}h", seconds / SECONDS_PER_HOUR)
                } else if seconds.is_multiple_of(SECONDS_PER_MINUTE) {
                    write!(formatter, "every {}m", seconds / SECONDS_PER_MINUTE)
                } else {
                    write!(formatter, "every {seconds}s")
                }
            }
            Rule::Daily { time, zone } => {
                write!(formatter, "daily {}", time.format("%H:%M"))?;
                write_zone(formatter, zone)
            }
            Rule::Weekly { days, time, zone } => {
                let names: Vec<&str> = days.listed().map(|(name, _)| name).collect();
                write!(
                    formatter,
                    "weekly {} {}",
                    names.join(","),
                    time.format("%H:%M")
                )?;
                write_zone(formatter, zone)
            }
            Rule::At { instant, zone } => {
                write!(
                    formatter,
                    "at {}",
                    instant.to_rfc3339_opts(SecondsFormat::AutoSi, true)
                )?;
                match zone {
                    Some(zone) => write!(formatter, " {}", zone.name()),
                    None => Ok(()),
                }
            }
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
    zone: Zone,
) -> Option<DateTime<Utc>> {
    let after_utc = after.naive_utc();
    let local_date = zone.to_local(after_utc)?.date();
    // The candidate on the local date of `after` may already be past, so a full week plus
    // one day always contains the next allowed weekday. The day before counts too: a time it
    // skipped at a daylight-saving jump over midnight runs on the local date of `after`.
    wall_clock_candidates(local_date, -1..=DAYS_PER_WEEK as i64, days, time, zone)
        .filter(|candidate| *candidate > after_utc)
        .min()
        .map(|candidate| Utc.from_utc_datetime(&candidate))
}

/// The latest wall-clock run at or before `until`.
fn previous_wall_clock(
    until: DateTime<Utc>,
    days: WeekdaySet,
    time: NaiveTime,
    zone: Zone,
) -> Option<DateTime<Utc>> {
    let until_utc = until.naive_utc();
    let local_date = zone.to_local(until_utc)?.date();
    // The candidate on the local date of `until` may still be ahead, so a full week plus one
    // day back always contains the previous allowed weekday; the day after counts for clocks
    // set back over midnight.
    wall_clock_candidates(local_date, -(DAYS_PER_WEEK as i64)..=1, days, time, zone)
        .filter(|candidate| *candidate <= until_utc)
        .max()
        .map(|candidate| Utc.from_utc_datetime(&candidate))
}

/// The UTC times of `time` on the allowed local dates `offsets` days from `local_date`.
fn wall_clock_candidates(
    local_date: NaiveDate,
    offsets: std::ops::RangeInclusive<i64>,
    days: WeekdaySet,
    time: NaiveTime,
    zone: Zone,
) -> impl Iterator<Item = NaiveDateTime> {
    offsets.filter_map(move |offset| {
        let date = local_date.checked_add_signed(Duration::days(offset))?;
        days.contains(date.weekday())
            .then(|| zone.to_utc(date.and_time(time)))
            .flatten()
    })
}

fn interval(seconds: u32) -> Duration {
    Duration::seconds(i64::from(seconds))
}

fn utc_zone() -> Zone {
    Zone::Offset(FixedOffset::east_opt(0).expect("zero is a valid offset"))
}

fn parse_instant(text: &str) -> Result<DateTime<FixedOffset>, ParseError> {
    DateTime::parse_from_rfc3339(text).map_err(|_| ParseError::InvalidInstant(excerpt(text)))
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
    let unit_seconds = match text.chars().last() {
        Some('s' | 'S') => 1,
        Some('m' | 'M') => SECONDS_PER_MINUTE,
        Some('h' | 'H') => SECONDS_PER_HOUR,
        Some('d' | 'D') => SECONDS_PER_DAY,
        _ => return Err(invalid()),
    };
    let count = &text[..text.len() - 1];
    if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let out_of_range = || ParseError::IntervalOutOfRange(excerpt(text));
    let seconds = count
        .parse::<u32>()
        .ok()
        .and_then(|count| count.checked_mul(unit_seconds))
        .ok_or_else(out_of_range)?;
    if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&seconds) {
        return Err(out_of_range());
    }
    Ok(seconds)
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

/// The zone after a rule's time; UTC, the default, is left out.
fn write_zone(formatter: &mut fmt::Formatter<'_>, zone: Zone) -> fmt::Result {
    match zone {
        Zone::Offset(offset) if offset.local_minus_utc() == 0 => Ok(()),
        zone => write!(formatter, " {zone}"),
    }
}
