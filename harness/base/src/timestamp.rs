//! RFC 3339 UTC timestamps with millisecond precision, the form transcripts and Tick observations carry.

use crate::private_fs::unix_time_milliseconds;

const MILLISECONDS_PER_DAY: i64 = 86_400_000;

/// The current UTC time in RFC 3339 with millisecond precision, as Claude Code's
/// `new Date().toISOString()` writes it.
pub fn now_timestamp() -> String {
    format_timestamp(unix_time_milliseconds())
}

pub fn format_timestamp(milliseconds: u64) -> String {
    let milliseconds = i64::try_from(milliseconds).unwrap_or(i64::MAX);
    let days = milliseconds.div_euclid(MILLISECONDS_PER_DAY);
    let within_day = milliseconds.rem_euclid(MILLISECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        within_day / 3_600_000,
        within_day / 60_000 % 60,
        within_day / 1000 % 60,
        within_day % 1000
    )
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff]Z` into Unix milliseconds; any other text is `None`.
pub fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        slice
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| slice.parse().ok())?
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let rest = text.get(19..)?;
    let fraction = match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits = fraction.strip_suffix('Z')?;
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let padded = format!("{digits:0<3}");
            padded[..3].parse::<i64>().ok()?
        }
        None if rest == "Z" => 0,
        None => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * MILLISECONDS_PER_DAY + ((hour * 60 + minute) * 60 + second) * 1000 + fraction)
}

// Howard Hinnant's civil-from-days and days-from-civil algorithms (proleptic Gregorian,
// 1970-01-01 is day 0).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_with_millisecond_precision() {
        for milliseconds in [0, 951_782_400_123, 1_789_000_000_999, 4_102_444_799_000] {
            let text = format_timestamp(milliseconds);
            assert_eq!(
                parse_timestamp_ms(&text),
                Some(milliseconds as i64),
                "{text}"
            );
        }
        assert_eq!(
            format_timestamp(951_782_400_123),
            "2000-02-29T00:00:00.123Z"
        );
        assert_eq!(
            parse_timestamp_ms("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        assert_eq!(parse_timestamp_ms("not a time"), None);
    }
}
