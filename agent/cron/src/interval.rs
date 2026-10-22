use std::cmp::Reverse;
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u32 = 60;
const MINUTES_PER_DAY: u32 = 24 * MINUTES_PER_HOUR;
const EVERY_KEYWORD: &str = "every";
const SHORT_UNITS: [char; 4] = ['s', 'm', 'h', 'd'];
/// Minute and hour intervals that divide their unit evenly, so a cron step repeats them
/// without a short gap at the wrap (`*/7` runs at :56 and again at :00). Day steps restart
/// each month like Claude Code's `0 0 */N * *`, which is accepted.
const EVEN_MINUTE_STEPS: [u32; 11] = [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30];
const EVEN_HOUR_STEPS: [u32; 7] = [1, 2, 3, 4, 6, 8, 12];
const MAX_DAY_STEP: u32 = 31;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntervalError {
    #[error("`{0}` is not an interval like 30s, 5m, 2h, 1d or `every 2 hours`")]
    Invalid(String),
    #[error("interval `{0}` must be longer than zero")]
    Zero(String),
    #[error("interval `{0}` is too long")]
    TooLong(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Second,
    Minute,
    Hour,
    Day,
}

impl Unit {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "s" | "sec" | "secs" | "second" | "seconds" => Some(Self::Second),
            "m" | "min" | "mins" | "minute" | "minutes" => Some(Self::Minute),
            "h" | "hr" | "hrs" | "hour" | "hours" => Some(Self::Hour),
            "d" | "day" | "days" => Some(Self::Day),
            _ => None,
        }
    }

    fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => SECONDS_PER_MINUTE,
            Self::Hour => SECONDS_PER_MINUTE * u64::from(MINUTES_PER_HOUR),
            Self::Day => SECONDS_PER_MINUTE * u64::from(MINUTES_PER_DAY),
        }
    }
}

/// A `/loop` interval in whole minutes, cron's resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval {
    minutes: NonZeroU32,
}

impl Interval {
    pub fn from_minutes(minutes: u32) -> Option<Self> {
        NonZeroU32::new(minutes).map(|minutes| Self { minutes })
    }

    pub fn minutes(self) -> u32 {
        self.minutes.get()
    }

    /// Parses `30s`, `5m`, `2h`, `1d`, `2 hours` or `every 2 hours`. Seconds round up to
    /// the next whole minute.
    pub fn parse(text: &str) -> Result<Self, IntervalError> {
        let trimmed = text.trim();
        let invalid = || IntervalError::Invalid(trimmed.to_string());
        let too_long = || IntervalError::TooLong(trimmed.to_string());
        let normalized = trimmed.to_ascii_lowercase();
        let mut words: Vec<&str> = normalized.split_whitespace().collect();
        if words.first() == Some(&EVERY_KEYWORD) {
            words.remove(0);
        }
        let (count, unit) = match words.as_slice() {
            [compact] => {
                let digits_end = compact
                    .find(|character: char| !character.is_ascii_digit())
                    .unwrap_or(compact.len());
                compact.split_at(digits_end)
            }
            [count, unit] => (*count, *unit),
            _ => return Err(invalid()),
        };
        let unit = Unit::parse(unit).ok_or_else(invalid)?;
        if count.is_empty() || !count.chars().all(|character| character.is_ascii_digit()) {
            return Err(invalid());
        }
        let count: u64 = count.parse().map_err(|_| too_long())?;
        let seconds = count.checked_mul(unit.seconds()).ok_or_else(too_long)?;
        let minutes =
            u32::try_from(seconds.div_ceil(SECONDS_PER_MINUTE)).map_err(|_| too_long())?;
        Self::from_minutes(minutes).ok_or_else(|| IntervalError::Zero(trimmed.to_string()))
    }

    /// Converts to a cron expression. An interval that no cron step repeats evenly, such as
    /// `7m` or `90m`, becomes the nearest one that does (the longer on a tie, so fewer runs).
    /// Multi-day intervals run at local midnight every N days of the month, up to 31.
    pub fn to_cron(self) -> CronInterval {
        let requested = self.minutes();
        let actual = even_intervals()
            .min_by_key(|candidate| (candidate.abs_diff(requested), Reverse(*candidate)))
            .and_then(Self::from_minutes)
            .unwrap_or(self);
        CronInterval {
            requested: self,
            actual,
            expression: cron_expression(actual.minutes()),
        }
    }
}

impl FromStr for Interval {
    type Err = IntervalError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let minutes = self.minutes();
        if minutes.is_multiple_of(MINUTES_PER_DAY) {
            write!(formatter, "{}d", minutes / MINUTES_PER_DAY)
        } else if minutes.is_multiple_of(MINUTES_PER_HOUR) {
            write!(formatter, "{}h", minutes / MINUTES_PER_HOUR)
        } else {
            write!(formatter, "{minutes}m")
        }
    }
}

fn even_intervals() -> impl Iterator<Item = u32> {
    EVEN_MINUTE_STEPS
        .into_iter()
        .chain(EVEN_HOUR_STEPS.map(|hours| hours * MINUTES_PER_HOUR))
        .chain((1..=MAX_DAY_STEP).map(|days| days * MINUTES_PER_DAY))
}

fn cron_expression(minutes: u32) -> String {
    match minutes {
        1 => "* * * * *".to_string(),
        MINUTES_PER_HOUR => "0 * * * *".to_string(),
        MINUTES_PER_DAY => "0 0 * * *".to_string(),
        minutes if minutes < MINUTES_PER_HOUR => format!("*/{minutes} * * * *"),
        minutes if minutes < MINUTES_PER_DAY => {
            format!("0 */{} * * *", minutes / MINUTES_PER_HOUR)
        }
        minutes => format!("0 0 */{} * *", minutes / MINUTES_PER_DAY),
    }
}

/// The cron form of a requested interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronInterval {
    pub requested: Interval,
    /// The interval the expression repeats at, which differs from `requested` when it had
    /// to be rounded; tell the user which one was used.
    pub actual: Interval,
    pub expression: String,
}

impl CronInterval {
    pub fn is_rounded(&self) -> bool {
        self.requested != self.actual
    }
}

/// `/loop` arguments split into an optional interval and the instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopArguments<'a> {
    pub interval: Option<Interval>,
    pub prompt: &'a str,
}

/// Splits `/loop` arguments the way Claude Code does: a leading token like `30m`, otherwise
/// a trailing `every <N><unit>` or `every <N> <unit>` clause, otherwise no interval.
/// `check every PR` has no interval because `PR` is not a time.
pub fn parse_loop_arguments(input: &str) -> Result<LoopArguments<'_>, IntervalError> {
    let words: Vec<(usize, &str)> = input
        .split_whitespace()
        .map(|word| (word.as_ptr().addr() - input.as_ptr().addr(), word))
        .collect();

    if let Some(&(_, first)) = words.first()
        && is_leading_interval(first)
    {
        let interval = Interval::parse(first)?;
        let prompt = input.trim_start()[first.len()..].trim();
        return Ok(LoopArguments {
            interval: Some(interval),
            prompt,
        });
    }

    for clause_words in [2, 3] {
        let Some(clause_start) = words.len().checked_sub(clause_words) else {
            continue;
        };
        let (offset, keyword) = words[clause_start];
        if !keyword.eq_ignore_ascii_case(EVERY_KEYWORD) {
            continue;
        }
        match Interval::parse(&input[offset..]) {
            Ok(interval) => {
                return Ok(LoopArguments {
                    interval: Some(interval),
                    prompt: input[..offset].trim(),
                });
            }
            Err(IntervalError::Invalid(_)) => continue,
            Err(error @ (IntervalError::Zero(_) | IntervalError::TooLong(_))) => return Err(error),
        }
    }

    Ok(LoopArguments {
        interval: None,
        prompt: input.trim(),
    })
}

fn is_leading_interval(word: &str) -> bool {
    let Some(count) = word.strip_suffix(SHORT_UNITS) else {
        return false;
    };
    !count.is_empty() && count.chars().all(|character| character.is_ascii_digit())
}
