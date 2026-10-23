use std::fmt;
use std::str::FromStr;

use std::collections::BTreeSet;

use chrono::{DateTime, LocalResult, NaiveDateTime, TimeDelta, TimeZone};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

const FIELD_NAMES: [&str; 5] = ["minute", "hour", "day-of-month", "month", "day-of-week"];
const FIELD_SYNTAX: &str = "0123456789*/,-";
const WILDCARD_PREFIX: char = '*';
const HOURS_PER_DAY: usize = 24;
const DAYS_PER_MONTH: usize = 31;
const DAYS_PER_WEEK: u16 = 7;
const MONTHS_PER_YEAR: usize = 12;
const MAX_GAP_MINUTES: i64 = 24 * 60;
const SECONDS_PER_MINUTE: i64 = 60;
const FIVE_MINUTE_MARKS: [u16; 12] = [0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55];
/// Where [`Schedule::shortest_gap`] starts reading matching days: 2000-01-01 UTC, a leap year.
const GAP_HORIZON_START: i64 = 946_684_800;
/// How many days of matching days [`Schedule::shortest_gap`] reads: past one leap-year cycle.
const GAP_HORIZON_DAYS: i64 = 5 * 366;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error(
        "cron expression needs 5 fields (minute hour day-of-month month day-of-week), found {0}"
    )]
    FieldCount(usize),
    #[error(
        "{field} field `{value}` is not supported; use `*`, numbers, ranges `1-5`, steps `*/15` or lists `1,15`"
    )]
    UnsupportedSyntax { field: &'static str, value: String },
    #[error("invalid cron expression `{expression}`: {reason}")]
    Invalid { expression: String, reason: String },
}

/// A parsed 5-field cron expression.
///
/// Day of week `0` and `7` both mean Sunday. When both day of month and day of week are
/// restricted, a day matches if either does (vixie cron).
///
/// Daylight-saving transitions follow vixie cron too. When neither the minute nor the hour
/// field starts with `*`, the times are fixed: those skipped by a spring-forward gap run once
/// right after it, and those repeated by a fall-back overlap run only in the first pass.
/// Otherwise times run as the clock shows them: skipped times never run and repeated times
/// run in both passes.
#[derive(Debug, Clone)]
pub struct Schedule {
    expression: String,
    cron: Cron,
    is_fixed_time: bool,
}

impl Schedule {
    pub fn parse(expression: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != FIELD_NAMES.len() {
            return Err(ScheduleError::FieldCount(fields.len()));
        }
        for (field, name) in fields.iter().zip(FIELD_NAMES) {
            if !field
                .chars()
                .all(|character| FIELD_SYNTAX.contains(character))
            {
                return Err(ScheduleError::UnsupportedSyntax {
                    field: name,
                    value: (*field).to_string(),
                });
            }
        }
        let expression = fields.join(" ");
        if fields
            .iter()
            .any(|field| field.split(',').any(str::is_empty))
        {
            return Err(ScheduleError::Invalid {
                expression,
                reason: "a list has an empty item".to_string(),
            });
        }
        let is_fixed_time =
            !fields[0].starts_with(WILDCARD_PREFIX) && !fields[1].starts_with(WILDCARD_PREFIX);
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(&expression)
            .map_err(|error| ScheduleError::Invalid {
                expression: expression.clone(),
                reason: error.to_string(),
            })?;
        Ok(Self {
            expression,
            cron,
            is_fixed_time,
        })
    }

    /// The expression with its fields separated by single spaces.
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// The first scheduled time at or after `time`, or `None` when the expression never
    /// matches again (such as `0 0 30 2 *`).
    pub fn next_at_or_after<Tz: TimeZone>(&self, time: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        self.next(time, true)
    }

    /// The first scheduled time strictly after `time`.
    pub fn next_after<Tz: TimeZone>(&self, time: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        self.next(time, false)
    }

    fn next<Tz: TimeZone>(&self, time: &DateTime<Tz>, inclusive: bool) -> Option<DateTime<Tz>> {
        let is_due = |instant: &DateTime<Tz>| {
            if inclusive {
                instant >= time
            } else {
                instant > time
            }
        };
        if !self.is_fixed_time {
            let candidate = self.cron.find_next_occurrence(time, inclusive).ok()?;
            return if is_due(&candidate) {
                Some(candidate)
            } else {
                self.cron.find_next_occurrence(time, false).ok()
            };
        }

        // croner only treats a single minute and hour as fixed, so fixed lists and ranges
        // walk the wall clock here and resolve each match the vixie way.
        let zone = time.timezone();
        let mut wall_clock = time.naive_local();
        if inclusive {
            // At the first instant after a spring-forward gap, the skipped wall clock times
            // are due now, so the walk starts where the gap began.
            let gap_start = time
                .clone()
                .checked_sub_signed(TimeDelta::seconds(1))
                .and_then(|previous| {
                    previous
                        .naive_local()
                        .checked_add_signed(TimeDelta::seconds(1))
                });
            if let Some(gap_start) = gap_start {
                wall_clock = wall_clock.min(gap_start);
            }
        }
        let mut includes_wall_clock = inclusive;
        loop {
            let candidate = self
                .cron
                .find_next_occurrence(&wall_clock, includes_wall_clock)
                .ok()?;
            let instant = match zone.from_local_datetime(&candidate) {
                LocalResult::Single(instant) => instant,
                LocalResult::Ambiguous(first_pass, _) => first_pass,
                LocalResult::None => first_instant_after_gap(&zone, candidate)?,
            };
            if is_due(&instant) {
                return Some(instant);
            }
            wall_clock = candidate;
            includes_wall_clock = false;
        }
    }

    /// The real time from `occurrence` to the scheduled time after it.
    pub fn gap_after<Tz: TimeZone>(&self, occurrence: &DateTime<Tz>) -> Option<TimeDelta> {
        self.next_after(occurrence)
            .map(|following| following - occurrence.clone())
    }

    /// The shortest time between two consecutive scheduled times, on the wall clock so that
    /// daylight-saving shifts do not count; `None` when the expression names at most one time
    /// within [`GAP_HORIZON_DAYS`]. Every matching day has the same times of day, so the
    /// shortest gap is one between two of them or one across consecutive matching days.
    pub fn shortest_gap(&self) -> Option<TimeDelta> {
        let pattern = &self.cron.pattern;
        let mut times_of_day: Vec<i64> = pattern
            .hours
            .values()
            .into_iter()
            .flat_map(|hour| {
                pattern
                    .minutes
                    .values()
                    .into_iter()
                    .map(move |minute| i64::from(hour) * 60 + i64::from(minute))
            })
            .collect();
        times_of_day.sort_unstable();
        let within_a_day = times_of_day
            .windows(2)
            .map(|pair| TimeDelta::minutes(pair[1] - pair[0]))
            .min();
        let (Some(first), Some(last)) = (times_of_day.first(), times_of_day.last()) else {
            return None;
        };
        let start = chrono::Utc.timestamp_opt(GAP_HORIZON_START, 0).single()?;
        let horizon = start + TimeDelta::days(GAP_HORIZON_DAYS);
        let mut across_days = None;
        let mut day_start = self.next_at_or_after(&start)? - TimeDelta::minutes(*first);
        while day_start < horizon {
            let last_of_day = day_start + TimeDelta::minutes(*last);
            let Some(next) = self.next_after(&last_of_day) else {
                break;
            };
            let gap = next - last_of_day;
            across_days = Some(across_days.map_or(gap, |shortest: TimeDelta| shortest.min(gap)));
            day_start = next - TimeDelta::minutes(*first);
        }
        within_a_day.into_iter().chain(across_days).min()
    }

    /// Whether this is `*/5 * * * *` in any spelling: the cadence that keeps a five-minute
    /// prompt cache warm only if it fires slightly early.
    pub(crate) fn is_every_five_minutes(&self) -> bool {
        let pattern = &self.cron.pattern;
        pattern.minutes.values() == FIVE_MINUTE_MARKS
            && pattern.hours.values().len() == HOURS_PER_DAY
            && pattern.months.values().len() == MONTHS_PER_YEAR
            && self.matches_every_day()
    }

    fn matches_every_day(&self) -> bool {
        let pattern = &self.cron.pattern;
        let every_day_of_month = pattern.days.values().len() == DAYS_PER_MONTH;
        let weekdays: BTreeSet<u16> = pattern
            .days_of_week
            .values()
            .into_iter()
            .map(|weekday| weekday % DAYS_PER_WEEK)
            .collect();
        let every_day_of_week = weekdays.len() == usize::from(DAYS_PER_WEEK);
        match (pattern.is_dom_wildcard(), pattern.is_dow_wildcard()) {
            (true, true) => true,
            (true, false) => every_day_of_week,
            (false, true) => every_day_of_month,
            (false, false) => every_day_of_month || every_day_of_week,
        }
    }
}

/// The first real instant after the spring-forward gap that swallowed `wall_clock`. Gaps
/// usually end on a minute, but some historical ones end mid-minute, so the minute found
/// first is refined to the second.
fn first_instant_after_gap<Tz: TimeZone>(
    zone: &Tz,
    wall_clock: NaiveDateTime,
) -> Option<DateTime<Tz>> {
    let resolve = |offset: TimeDelta| {
        wall_clock
            .checked_add_signed(offset)
            .and_then(|shifted| zone.from_local_datetime(&shifted).earliest())
    };
    let first_valid_minute =
        (1..=MAX_GAP_MINUTES).find(|&minutes| resolve(TimeDelta::minutes(minutes)).is_some())?;
    let previous_minute_seconds = (first_valid_minute - 1) * SECONDS_PER_MINUTE;
    (previous_minute_seconds + 1..=first_valid_minute * SECONDS_PER_MINUTE)
        .find_map(|seconds| resolve(TimeDelta::seconds(seconds)))
}

impl FromStr for Schedule {
    type Err = ScheduleError;

    fn from_str(expression: &str) -> Result<Self, Self::Err> {
        Self::parse(expression)
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.expression)
    }
}

impl PartialEq for Schedule {
    fn eq(&self, other: &Self) -> bool {
        self.expression == other.expression
    }
}

impl Eq for Schedule {}
