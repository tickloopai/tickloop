use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, TimeDelta, TimeZone};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

const FIELD_NAMES: [&str; 5] = ["minute", "hour", "day-of-month", "month", "day-of-week"];
const FIELD_SYNTAX: &str = "0123456789*/,-";
const HOURS_PER_DAY: usize = 24;
const MONTHS_PER_YEAR: usize = 12;
const FIVE_MINUTE_MARKS: [u16; 12] = [0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55];

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
/// restricted, a day matches if either does (vixie cron). Daylight-saving transitions follow
/// vixie cron as well: a fixed time skipped by a spring-forward gap runs right after the gap,
/// a fixed time repeated by a fall-back overlap runs once, and wildcard or stepped hours run
/// in real time.
#[derive(Debug, Clone)]
pub struct Schedule {
    expression: String,
    cron: Cron,
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
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(&expression)
            .map_err(|error| ScheduleError::Invalid {
                expression: expression.clone(),
                reason: error.to_string(),
            })?;
        Ok(Self { expression, cron })
    }

    /// The expression with its fields separated by single spaces.
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// The first scheduled time at or after `time`, or `None` when the expression never
    /// matches again (such as `0 0 30 2 *`).
    pub fn next_at_or_after<Tz: TimeZone>(&self, time: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let candidate = self.cron.find_next_occurrence(time, true).ok()?;
        if candidate >= *time {
            Some(candidate)
        } else {
            self.next_after(time)
        }
    }

    /// The first scheduled time strictly after `time`.
    pub fn next_after<Tz: TimeZone>(&self, time: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        self.cron.find_next_occurrence(time, false).ok()
    }

    /// The real time from `occurrence` to the scheduled time after it.
    pub fn gap_after<Tz: TimeZone>(&self, occurrence: &DateTime<Tz>) -> Option<TimeDelta> {
        self.next_after(occurrence)
            .map(|following| following - occurrence.clone())
    }

    /// Whether this is `*/5 * * * *` in any spelling: the cadence that keeps a five-minute
    /// prompt cache warm only if it fires slightly early.
    pub(crate) fn is_every_five_minutes(&self) -> bool {
        let pattern = &self.cron.pattern;
        pattern.minutes.values() == FIVE_MINUTE_MARKS
            && pattern.hours.values().len() == HOURS_PER_DAY
            && pattern.months.values().len() == MONTHS_PER_YEAR
            && pattern.is_dom_wildcard()
            && pattern.is_dow_wildcard()
    }
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
