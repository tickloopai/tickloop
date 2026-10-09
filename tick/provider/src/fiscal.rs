//! Fiscal quarters as the earnings endpoints label and report them.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::api::FiscalPeriodEnd;

/// The bare quarter number, so that a period from one endpoint matches the same period from
/// another whether it is reported as `2` or `Q2`.
/// The bare quarter number behind the labels the endpoints use for the same period: consensus
/// says `2`, the US metrics say `Q2`, and the calendar says `qf2`.
pub fn normalized_fiscal_period(text: &str) -> String {
    let trimmed = text.trim();
    let digits = trimmed.trim_start_matches(|character: char| !character.is_ascii_digit());
    if digits.is_empty() {
        trimmed.to_owned()
    } else {
        digits.to_owned()
    }
}

/// The last calendar day of a fiscal quarter, used to look for its announcement.
pub fn fiscal_period_end(fiscal_year: i32, fiscal_period: &str) -> Option<time::Date> {
    let quarter: u8 = normalized_fiscal_period(fiscal_period).parse().ok()?;
    let month = time::Month::try_from(quarter.checked_mul(3)?).ok()?;
    let start = time::Date::from_calendar_date(fiscal_year, month, 1).ok()?;
    start.replace_day(month.length(fiscal_year)).ok()
}

/// Fiscal period ends from the income statement's raw `list`: every `values[]` entry carries
/// `fp_end` as Unix seconds, `period` as `Q3 2025`, and `year`; the same period repeats under
/// every account, so entries are deduplicated by fiscal year and quarter.
pub fn fiscal_period_ends_from_report(list: &Value) -> Vec<FiscalPeriodEnd> {
    let mut ends: BTreeMap<(i32, String), time::Date> = BTreeMap::new();
    let indicators = list
        .as_object()
        .into_iter()
        .flat_map(|statements| statements.values())
        .filter_map(|statement| statement.get("indicators")?.as_array())
        .flatten();
    for indicator in indicators {
        let accounts = indicator.get("accounts").and_then(Value::as_array);
        for account in accounts.into_iter().flatten() {
            let values = account.get("values").and_then(Value::as_array);
            for entry in values.into_iter().flatten() {
                let Some((fiscal_year, fiscal_period, end)) = fiscal_period_entry(entry) else {
                    continue;
                };
                ends.entry((fiscal_year, fiscal_period)).or_insert(end);
            }
        }
    }
    ends.into_iter()
        .map(|((fiscal_year, fiscal_period), end)| FiscalPeriodEnd {
            fiscal_year,
            fiscal_period,
            end,
        })
        .collect()
}

/// The fiscal year, bare quarter and period end of one income-statement `values[]` entry.
pub fn fiscal_period_entry(entry: &Value) -> Option<(i32, String, time::Date)> {
    let period_text = entry.get("period")?.as_str()?;
    let mut parts = period_text.split_whitespace();
    let quarter = normalized_fiscal_period(parts.next()?);
    let fiscal_year = match entry.get("year") {
        Some(Value::Number(year)) => year.as_i64()? as i32,
        Some(Value::String(year)) => year.parse().ok()?,
        _ => parts.next()?.parse().ok()?,
    };
    let seconds = match entry.get("fp_end")? {
        Value::Number(seconds) => seconds.as_i64()?,
        Value::String(seconds) => seconds.parse().ok()?,
        _ => return None,
    };
    let end = time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .date();
    Some((fiscal_year, quarter, end))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fiscal_period_ends_come_from_the_income_statement_and_dedupe_across_accounts() {
        let list = json!({
            "IS": {"indicators": [{"accounts": [
                {"field": "EPS", "values": [
                    {"fp_end": "1753488000", "period": "Q2 2027", "year": 2027, "value": "2.46"},
                    {"fp_end": "1745625600", "period": "Q1 2027", "year": "2027", "value": "2.39"}
                ]},
                {"field": "Revenue", "values": [
                    {"fp_end": 1753488000, "period": "Q2 2027", "year": 2027, "value": "96221000000"}
                ]}
            ]}]}
        });
        let ends = fiscal_period_ends_from_report(&list);
        assert_eq!(ends.len(), 2);
        assert_eq!(ends[0].fiscal_year, 2027);
        assert_eq!(ends[0].fiscal_period, "1");
        assert_eq!(
            ends[0].end,
            time::Date::from_calendar_date(2025, time::Month::April, 26).unwrap()
        );
        assert_eq!(ends[1].fiscal_period, "2");
        assert_eq!(
            ends[1].end,
            time::Date::from_calendar_date(2025, time::Month::July, 26).unwrap()
        );
    }

    #[test]
    fn fiscal_period_labels_from_every_endpoint_normalize_to_the_quarter_number() {
        for label in ["2", "Q2", "q2", "qf2", " QF2 "] {
            assert_eq!(normalized_fiscal_period(label), "2", "{label:?}");
        }
        assert_eq!(normalized_fiscal_period("af"), "af");
    }
}
