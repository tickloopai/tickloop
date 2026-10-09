//! Assembly of the Earnings table from the fundamentals, calendar and price responses.
//!
//! Everything in this module is a pure function of already-fetched responses: the trading-day
//! arithmetic, the pre-market versus after-hours earnings-day rule, and the two summary rows are
//! decided here so that they can be exercised without any I/O.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use crate::output::TableData;
use tick_provider::{
    api::{
        Candle, ConsensusPeriod, DateRange, FiscalPeriodEnd, ReleaseSession, ReportDate,
        ReportEvent,
    },
    dates::iso_date,
    fiscal::fiscal_period_end,
};

pub mod review;

pub const DEFAULT_PERIOD_COUNT: usize = 10;
pub const MAX_PERIOD_COUNT: usize = 20;

const SHORT_WINDOW: usize = 5;
const LONG_WINDOW: usize = 20;

/// Calendar days of prices to load around a report date: twenty trading days plus weekends and
/// holidays on either side of the announcement.
const PRICE_WINDOW_DAYS: i64 = 42;
/// `trading_days` rejects a long range with `301600 too many query days`, so the windows are
/// requested about a month at a time.
const TRADING_DAY_CHUNK_DAYS: i64 = 28;

/// The finance calendar is market-wide and pages roughly one calendar date at a time, so each
/// period is searched in a short window around the date US issuers usually announce.
pub const REPORT_LAG_DAYS: i64 = 21;
const REPORT_SEARCH_BEFORE_DAYS: i64 = 12;
const REPORT_SEARCH_AFTER_DAYS: i64 = 45;
const NEXT_REPORT_AHEAD_DAYS: i64 = 120;
/// Thirteen weeks: where the next fiscal period ends when the issuer has not reported it yet.
const FISCAL_QUARTER_DAYS: i64 = 91;

/// The table's columns, named so that the priority order and the summary rows address them by
/// meaning instead of by a bare index.
const PERIOD: usize = 0;
const REPORT_DATE: usize = 1;
const EPS_ESTIMATE: usize = 2;
const EPS_ACTUAL: usize = 3;
const EPS_SURPRISE: usize = 4;
const REVENUE_ESTIMATE: usize = 5;
const REVENUE_ACTUAL: usize = 6;
const REVENUE_SURPRISE: usize = 7;
const BEFORE_20D: usize = 8;
const BEFORE_5D: usize = 9;
const DAY: usize = 10;
const AFTER_5D: usize = 11;
const AFTER_20D: usize = 12;
const DAY_BAR: usize = 13;

pub const HEADERS: &[&str] = &[
    "Period",
    "Report Date",
    "EPS Estimate",
    "EPS Actual",
    "EPS Surprise",
    "Revenue Estimate",
    "Revenue Actual",
    "Revenue Surprise",
    "Before 20d",
    "Before 5d",
    "Day",
    "After 5d",
    "After 20d",
    "Day Bar",
];

/// Most useful first: the order in which a narrow Panel keeps columns.
pub const COLUMN_PRIORITY: &[usize] = &[
    PERIOD,
    DAY,
    DAY_BAR,
    EPS_SURPRISE,
    REPORT_DATE,
    BEFORE_5D,
    AFTER_5D,
    BEFORE_20D,
    AFTER_20D,
    EPS_ESTIMATE,
    EPS_ACTUAL,
    REVENUE_SURPRISE,
    REVENUE_ESTIMATE,
    REVENUE_ACTUAL,
];

const MEAN_LABEL: &str = "Mean";
const COUNTS_LABEL: &str = "Counts";

/// Everything the table is assembled from, as returned by the five fundamentals and calendar calls.
#[derive(Clone, Debug, Default)]
pub struct EarningsInputs {
    pub symbol: String,
    pub count: usize,
    pub periods: Vec<ConsensusPeriod>,
    pub report_dates: Vec<ReportDate>,
    pub period_ends: Vec<FiscalPeriodEnd>,
    pub events: Vec<ReportEvent>,
    pub next_report: Option<ReportEvent>,
    pub trading_days: Vec<time::Date>,
    pub candles: Vec<Candle>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EarningsTable {
    pub symbol: String,
    pub source: &'static str,
    pub observed_at: Option<String>,
    pub availability: Vec<review::DataAvailability>,
    pub company_filings: Vec<Value>,
    pub next_report: Option<NextReport>,
    pub upcoming: Option<review::UpcomingPeriod>,
    pub rows: Vec<EarningsRow>,
    pub summary: EarningsSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NextReport {
    pub date: String,
    pub session: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EarningsRow {
    pub period: String,
    pub period_id: String,
    pub fiscal_year: i32,
    pub fiscal_period: String,
    pub fiscal_period_end: Option<String>,
    pub metrics_source: &'static str,
    pub statement_items: Vec<review::StatementItem>,
    pub segments: Vec<review::Segment>,
    pub report_date: Option<String>,
    pub session: Option<&'static str>,
    pub eps_estimate: Option<Decimal>,
    pub eps_actual: Option<Decimal>,
    pub eps_surprise: Option<Decimal>,
    pub revenue_estimate: Option<Decimal>,
    pub revenue_actual: Option<Decimal>,
    pub revenue_surprise: Option<Decimal>,
    pub before_20d: Option<Decimal>,
    pub before_5d: Option<Decimal>,
    pub day: Option<Decimal>,
    pub after_5d: Option<Decimal>,
    pub after_20d: Option<Decimal>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EarningsSummary {
    pub periods: usize,
    pub before_20d: Option<Decimal>,
    pub before_5d: Option<Decimal>,
    pub day: Option<Decimal>,
    pub after_5d: Option<Decimal>,
    pub after_20d: Option<Decimal>,
    pub mean_absolute_day: Option<Decimal>,
    pub eps_beats: usize,
    pub revenue_beats: usize,
    pub up_days: usize,
    pub eps_beats_and_up: usize,
}

/// The released periods the table covers, newest first.
pub fn released_periods(periods: &[ConsensusPeriod], count: usize) -> Vec<&ConsensusPeriod> {
    periods
        .iter()
        .filter(|period| period.is_released)
        .take(count)
        .collect()
}

/// The end of `period` as the company reports it, or the calendar quarter end when the income
/// statement did not list the period.
pub fn period_end(period_ends: &[FiscalPeriodEnd], period: &ConsensusPeriod) -> Option<time::Date> {
    period_ends
        .iter()
        .find(|known| {
            known.fiscal_year == period.fiscal_year && known.fiscal_period == period.fiscal_period
        })
        .map(|known| known.end)
        .or_else(|| fiscal_period_end(period.fiscal_year, &period.fiscal_period))
}

/// Reported periods older than every period in `covered`, newest first and at most `missing`
/// of them: the consensus endpoint carries only the latest eight releases, so the calendar
/// fills in the rest of a longer request.
pub fn older_period_ends(
    period_ends: &[FiscalPeriodEnd],
    covered: &[ConsensusPeriod],
    missing: usize,
    as_of: time::Date,
) -> Vec<FiscalPeriodEnd> {
    let oldest_covered = covered
        .iter()
        .filter_map(|period| period_end(period_ends, period))
        .min();
    let mut older: Vec<FiscalPeriodEnd> = period_ends
        .iter()
        .filter(|known| known.end <= as_of && oldest_covered.is_none_or(|limit| known.end < limit))
        .cloned()
        .collect();
    older.sort_by_key(|known| std::cmp::Reverse(known.end));
    older.truncate(missing);
    older
}

/// A period the consensus endpoint no longer carries, rebuilt from the announcement the
/// calendar found for it and the estimate and actual attached to that announcement.
pub fn calendar_period(known: &FiscalPeriodEnd, event: &ReportEvent) -> ConsensusPeriod {
    let fiscal_year = event.fiscal_year.unwrap_or(known.fiscal_year);
    let fiscal_period = event
        .fiscal_period
        .clone()
        .unwrap_or_else(|| known.fiscal_period.clone());
    ConsensusPeriod {
        label: format!("Q{fiscal_period} {fiscal_year}"),
        fiscal_year,
        fiscal_period,
        is_released: true,
        eps: event.eps,
        revenue: event.revenue,
    }
}

/// The calendar dates to search for the announcement of a period that ended on `period_end`,
/// given how many days after a period end this issuer usually reports; `None` once the whole
/// window lies after `as_of`.
pub fn announcement_window(
    period_end: time::Date,
    lag: i64,
    as_of: time::Date,
) -> Option<DateRange> {
    let expected = period_end + time::Duration::days(lag);
    let window = DateRange {
        start: expected - time::Duration::days(REPORT_SEARCH_BEFORE_DAYS),
        end: (expected + time::Duration::days(REPORT_SEARCH_AFTER_DAYS)).min(as_of),
    };
    (window.start <= window.end).then_some(window)
}

/// Where to look for the next announcement: around the date the period after the newest
/// released one should be reported with the issuer's usual lag, or the next four months when
/// no released period end is known to count from.
pub fn next_report_window(
    periods: &[ConsensusPeriod],
    period_ends: &[FiscalPeriodEnd],
    lag: i64,
    as_of: time::Date,
) -> DateRange {
    let ahead = DateRange {
        start: as_of,
        end: as_of + time::Duration::days(NEXT_REPORT_AHEAD_DAYS),
    };
    let Some(newest_end) = released_periods(periods, 1)
        .first()
        .and_then(|period| period_end(period_ends, period))
    else {
        return ahead;
    };
    let next_end = period_ends
        .iter()
        .map(|known| known.end)
        .filter(|end| *end > newest_end)
        .min()
        .unwrap_or(newest_end + time::Duration::days(FISCAL_QUARTER_DAYS));
    let expected = next_end + time::Duration::days(lag);
    let window = DateRange {
        start: (expected - time::Duration::days(REPORT_SEARCH_BEFORE_DAYS)).max(as_of),
        end: expected + time::Duration::days(REPORT_SEARCH_AFTER_DAYS),
    };
    if window.end < window.start {
        ahead
    } else {
        window
    }
}

/// The announcement date and release session of one period, preferring the US key financial
/// metrics report date and falling back to the calendar event for the same fiscal period. The
/// session comes only from a calendar event: without one it stays unknown rather than being
/// assumed, because the assumption would decide which session reacted to the announcement.
pub fn resolve_announcement(
    period: &ConsensusPeriod,
    report_dates: &[ReportDate],
    period_ends: &[FiscalPeriodEnd],
    events: &[ReportEvent],
) -> Option<(time::Date, Option<ReleaseSession>)> {
    let matching_event = events.iter().find(|event| {
        event.fiscal_year == Some(period.fiscal_year)
            && event.fiscal_period.as_deref() == Some(period.fiscal_period.as_str())
    });
    let reported = report_dates
        .iter()
        .find(|reported| {
            reported.fiscal_year == period.fiscal_year
                && reported.fiscal_period == period.fiscal_period
        })
        .map(|reported| reported.date);
    if let Some(event) = matching_event {
        return Some((reported.unwrap_or(event.date), Some(event.session)));
    }
    if let Some(date) = reported {
        return Some((date, None));
    }
    // Without a fiscal period on the event, the announcement is the first one after the period end.
    let period_end = self::period_end(period_ends, period)?;
    events
        .iter()
        .filter(|event| event.fiscal_year.is_none() && event.date >= period_end)
        .min_by_key(|event| event.date)
        .map(|event| (event.date, Some(event.session)))
}

/// The price windows the table needs, one per announcement, merged where they overlap.
pub fn price_windows(announcements: &[time::Date], as_of: time::Date) -> Vec<DateRange> {
    let mut windows: Vec<DateRange> = Vec::new();
    let mut dates: Vec<time::Date> = announcements.to_vec();
    dates.sort_unstable();
    for date in dates {
        let start = date - time::Duration::days(PRICE_WINDOW_DAYS);
        let end = (date + time::Duration::days(PRICE_WINDOW_DAYS)).min(as_of);
        if end < start {
            continue;
        }
        match windows.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => windows.push(DateRange { start, end }),
        }
    }
    windows
}

/// The same windows split into ranges the trading-day endpoint accepts.
pub fn trading_day_windows(windows: &[DateRange]) -> Vec<DateRange> {
    windows
        .iter()
        .flat_map(|window| split_window(*window, TRADING_DAY_CHUNK_DAYS))
        .collect()
}

/// Consecutive ranges of at most `days` calendar days that together cover `window`.
pub fn split_window(window: DateRange, days: i64) -> Vec<DateRange> {
    let mut chunks = Vec::new();
    let mut start = window.start;
    while start <= window.end {
        let end = (start + time::Duration::days(days - 1)).min(window.end);
        chunks.push(DateRange { start, end });
        start = end + time::Duration::days(1);
    }
    chunks
}

pub fn assemble(inputs: &EarningsInputs) -> EarningsTable {
    let sessions = trading_sessions(&inputs.trading_days, &inputs.candles);
    let rows: Vec<EarningsRow> = released_periods(&inputs.periods, inputs.count)
        .into_iter()
        .map(|period| {
            let announcement = resolve_announcement(
                period,
                &inputs.report_dates,
                &inputs.period_ends,
                &inputs.events,
            );
            let mut row = row(period, announcement, &sessions);
            row.fiscal_period_end = inputs
                .period_ends
                .iter()
                .find(|known| {
                    known.fiscal_year == period.fiscal_year
                        && known.fiscal_period == period.fiscal_period
                })
                .map(|known| iso_date(known.end));
            row
        })
        .collect();
    let summary = summarize(&rows);
    EarningsTable {
        symbol: inputs.symbol.clone(),
        source: "longbridge",
        observed_at: None,
        availability: Vec::new(),
        company_filings: Vec::new(),
        next_report: inputs.next_report.as_ref().map(|event| NextReport {
            date: iso_date(event.date),
            session: event.session.label(),
        }),
        upcoming: review::upcoming_period(inputs),
        rows,
        summary,
    }
}

/// The table as it is drawn and printed: the period rows followed by the mean and count rows.
pub fn table(table: &EarningsTable) -> TableData {
    let mut rows: Vec<Vec<Value>> = table.rows.iter().map(display_row).collect();
    rows.push(mean_row(&table.summary));
    rows.push(counts_row(&table.summary));
    TableData {
        headers: HEADERS.to_vec(),
        rows,
    }
}

pub fn title(table: &EarningsTable) -> String {
    match &table.next_report {
        Some(next) => format!(
            "{} · Earnings · Next {} ({})",
            table.symbol, next.date, next.session
        ),
        None => format!("{} · Earnings", table.symbol),
    }
}

/// The close of every trading day in the requested windows, indexed so that offsets count
/// sessions rather than calendar days.
struct TradingSessions {
    days: Vec<time::Date>,
    closes: BTreeMap<time::Date, Decimal>,
}

impl TradingSessions {
    fn close(&self, index: usize) -> Option<Decimal> {
        self.closes.get(self.days.get(index)?).copied()
    }

    /// The session whose close first reflects the announcement, and the session before it. An
    /// unknown release session has no answer: the reaction could be the announcement day or the
    /// next one, so the moves around it stay empty.
    fn reaction(
        &self,
        date: time::Date,
        session: Option<ReleaseSession>,
    ) -> Option<(usize, usize)> {
        let reaction = match (self.days.binary_search(&date), session?) {
            (Ok(index), ReleaseSession::AfterHours) => index + 1,
            (Ok(index), ReleaseSession::PreMarket) => index,
            (Err(index), _) => index,
        };
        Some((reaction.checked_sub(1)?, reaction))
    }
}

fn trading_sessions(trading_days: &[time::Date], candles: &[Candle]) -> TradingSessions {
    let closes: BTreeMap<time::Date, Decimal> = candles
        .iter()
        .map(|candle| (candle.timestamp.date(), candle.close))
        .collect();
    let mut days: Vec<time::Date> = trading_days.to_vec();
    days.extend(closes.keys().copied());
    days.sort_unstable();
    days.dedup();
    TradingSessions { days, closes }
}

fn change_percent(from: Decimal, to: Decimal) -> Option<Decimal> {
    if from.is_zero() {
        return None;
    }
    Some(((to - from) / from * Decimal::ONE_HUNDRED).round_dp(2))
}

fn move_between(sessions: &TradingSessions, from: usize, to: usize) -> Option<Decimal> {
    change_percent(sessions.close(from)?, sessions.close(to)?)
}

fn row(
    period: &ConsensusPeriod,
    announcement: Option<(time::Date, Option<ReleaseSession>)>,
    sessions: &TradingSessions,
) -> EarningsRow {
    let reaction = announcement
        .and_then(|(date, session)| sessions.reaction(date, session))
        .filter(|(base, reaction)| {
            sessions.close(*base).is_some() && sessions.close(*reaction).is_some()
        });
    let (base, reaction) = reaction.unzip();
    EarningsRow {
        period: period.label.clone(),
        period_id: review::period_id(period.fiscal_year, &period.fiscal_period),
        fiscal_year: period.fiscal_year,
        fiscal_period: period.fiscal_period.clone(),
        fiscal_period_end: None,
        metrics_source: "consensus",
        statement_items: Vec::new(),
        segments: Vec::new(),
        report_date: announcement.map(|(date, _)| iso_date(date)),
        session: announcement
            .and_then(|(_, session)| session)
            .map(ReleaseSession::name),
        eps_estimate: period.eps.estimate,
        eps_actual: period.eps.actual,
        eps_surprise: period.eps.resolved_surprise(),
        revenue_estimate: period.revenue.estimate,
        revenue_actual: period.revenue.actual,
        revenue_surprise: period.revenue.resolved_surprise(),
        before_20d: base
            .and_then(|base| move_between(sessions, base.checked_sub(LONG_WINDOW)?, base)),
        before_5d: base
            .and_then(|base| move_between(sessions, base.checked_sub(SHORT_WINDOW)?, base)),
        day: base
            .zip(reaction)
            .and_then(|(base, reaction)| move_between(sessions, base, reaction)),
        after_5d: reaction
            .and_then(|reaction| move_between(sessions, reaction, reaction + SHORT_WINDOW)),
        after_20d: reaction
            .and_then(|reaction| move_between(sessions, reaction, reaction + LONG_WINDOW)),
    }
}

fn mean(values: impl Iterator<Item = Decimal>) -> Option<Decimal> {
    let mut total = Decimal::ZERO;
    let mut count = 0_u32;
    for value in values {
        total += value;
        count += 1;
    }
    (count > 0).then(|| (total / Decimal::from(count)).round_dp(2))
}

fn summarize(rows: &[EarningsRow]) -> EarningsSummary {
    let column = |select: fn(&EarningsRow) -> Option<Decimal>| mean(rows.iter().filter_map(select));
    EarningsSummary {
        periods: rows.len(),
        before_20d: column(|row| row.before_20d),
        before_5d: column(|row| row.before_5d),
        day: column(|row| row.day),
        after_5d: column(|row| row.after_5d),
        after_20d: column(|row| row.after_20d),
        mean_absolute_day: mean(rows.iter().filter_map(|row| row.day.map(|day| day.abs()))),
        eps_beats: rows.iter().filter(|row| beat(row.eps_surprise)).count(),
        revenue_beats: rows.iter().filter(|row| beat(row.revenue_surprise)).count(),
        up_days: rows.iter().filter(|row| rose(row.day)).count(),
        eps_beats_and_up: rows
            .iter()
            .filter(|row| beat(row.eps_surprise) && rose(row.day))
            .count(),
    }
}

fn beat(surprise: Option<Decimal>) -> bool {
    surprise.is_some_and(|surprise| surprise > Decimal::ZERO)
}

fn rose(move_percent: Option<Decimal>) -> bool {
    move_percent.is_some_and(|percent| percent > Decimal::ZERO)
}

fn cell(value: Option<Decimal>) -> Value {
    value.map_or(Value::Null, |value| json!(value.normalize().to_string()))
}

fn display_row(row: &EarningsRow) -> Vec<Value> {
    vec![
        json!(row.period),
        row.report_date
            .as_ref()
            .map_or(Value::Null, |date| json!(date)),
        cell(row.eps_estimate),
        cell(row.eps_actual),
        cell(row.eps_surprise),
        cell(row.revenue_estimate),
        cell(row.revenue_actual),
        cell(row.revenue_surprise),
        cell(row.before_20d),
        cell(row.before_5d),
        cell(row.day),
        cell(row.after_5d),
        cell(row.after_20d),
        // The bar column repeats the earnings-day move; the renderer turns it into block glyphs.
        cell(row.day),
    ]
}

fn mean_row(summary: &EarningsSummary) -> Vec<Value> {
    vec![
        json!(MEAN_LABEL),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        cell(summary.before_20d),
        cell(summary.before_5d),
        cell(summary.day),
        cell(summary.after_5d),
        cell(summary.after_20d),
        // A magnitude, not a direction: the pre-rendered bar keeps it out of the up/down colours.
        json!(
            summary
                .mean_absolute_day
                .map(magnitude_bar)
                .unwrap_or_default()
        ),
    ]
}

fn counts_row(summary: &EarningsSummary) -> Vec<Value> {
    let mut row = vec![Value::Null; HEADERS.len()];
    row[PERIOD] = json!(COUNTS_LABEL);
    row[EPS_SURPRISE] = json!(format!("beat {}", summary.eps_beats));
    row[REVENUE_SURPRISE] = json!(format!("beat {}", summary.revenue_beats));
    row[DAY] = json!(format!("up {}", summary.up_days));
    row[DAY_BAR] = json!(format!("both {}", summary.eps_beats_and_up));
    row
}

fn magnitude_bar(percent: Decimal) -> String {
    use rust_decimal::prelude::ToPrimitive;
    tickloop_ui::charts::magnitude_bar(percent.to_f64().unwrap_or_default())
}

#[cfg(test)]
mod column_tests {
    use super::*;

    fn day(year: i32, month: u8, day: u8) -> time::Date {
        time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day).unwrap()
    }

    fn known(fiscal_year: i32, fiscal_period: &str, end: time::Date) -> FiscalPeriodEnd {
        FiscalPeriodEnd {
            fiscal_year,
            fiscal_period: fiscal_period.into(),
            end,
        }
    }

    fn released(fiscal_year: i32, fiscal_period: &str) -> ConsensusPeriod {
        ConsensusPeriod {
            fiscal_year,
            fiscal_period: fiscal_period.into(),
            label: format!("Q{fiscal_period} {fiscal_year}"),
            is_released: true,
            eps: Default::default(),
            revenue: Default::default(),
        }
    }

    #[test]
    fn older_periods_come_after_the_consensus_ones_newest_first_and_never_from_the_future() {
        let as_of = day(2026, 9, 22);
        let ends = vec![
            known(2025, "1", day(2024, 4, 28)),
            known(2025, "2", day(2024, 7, 28)),
            known(2025, "3", day(2024, 10, 27)),
            known(2025, "4", day(2025, 1, 26)),
            known(2027, "3", day(2026, 10, 25)),
        ];
        let covered = vec![released(2025, "4"), released(2025, "3")];

        let older = older_period_ends(&ends, &covered, 5, as_of);
        let quarters: Vec<&str> = older
            .iter()
            .map(|known| known.fiscal_period.as_str())
            .collect();
        assert_eq!(quarters, ["2", "1"]);

        assert_eq!(older_period_ends(&ends, &covered, 1, as_of).len(), 1);
        assert!(older_period_ends(&ends, &covered, 0, as_of).is_empty());
        let without_consensus = older_period_ends(&ends, &[], 10, as_of);
        assert_eq!(without_consensus.len(), 4);
        assert_eq!(without_consensus[0].fiscal_period, "4");
    }

    #[test]
    fn a_calendar_period_takes_the_figures_from_the_event_and_the_ids_from_whichever_knows_them() {
        let event = ReportEvent {
            symbol: "NVDA.US".into(),
            date: day(2024, 8, 28),
            session: ReleaseSession::AfterHours,
            fiscal_year: None,
            fiscal_period: None,
            eps: tick_provider::api::ConsensusMetric {
                estimate: Some(Decimal::new(64, 2)),
                actual: Some(Decimal::new(68, 2)),
                surprise: None,
            },
            revenue: Default::default(),
        };
        let period = calendar_period(&known(2025, "2", day(2024, 7, 28)), &event);
        assert_eq!(period.label, "Q2 2025");
        assert_eq!(
            (period.fiscal_year, period.fiscal_period.as_str()),
            (2025, "2")
        );
        assert!(period.is_released);
        assert_eq!(period.eps.resolved_surprise(), Some(Decimal::new(4, 2)));

        let labelled = ReportEvent {
            fiscal_year: Some(2025),
            fiscal_period: Some("2".into()),
            ..event
        };
        let period = calendar_period(&known(2026, "9", day(2024, 7, 28)), &labelled);
        assert_eq!(period.label, "Q2 2025");
    }

    #[test]
    fn the_announcement_window_surrounds_the_expected_date_and_stops_at_today() {
        let window = announcement_window(day(2026, 7, 26), 31, day(2026, 9, 22)).unwrap();
        assert_eq!(window.start, day(2026, 8, 14));
        assert_eq!(window.end, day(2026, 9, 22));
        assert!(announcement_window(day(2026, 9, 30), 21, day(2026, 9, 22)).is_none());
    }

    #[test]
    fn the_next_report_is_searched_around_the_period_after_the_newest_release() {
        let as_of = day(2026, 9, 22);
        let ends = vec![
            known(2027, "1", day(2026, 4, 26)),
            known(2027, "2", day(2026, 7, 26)),
        ];
        let periods = vec![
            ConsensusPeriod {
                is_released: false,
                ..released(2027, "3")
            },
            released(2027, "2"),
        ];

        let window = next_report_window(&periods, &ends, 31, as_of);
        assert_eq!(window.start, day(2026, 11, 13));
        assert_eq!(window.end, day(2027, 1, 9));

        let wide = next_report_window(&[], &ends, 31, as_of);
        assert_eq!(wide.start, as_of);
        assert_eq!(wide.end, day(2027, 1, 20));

        let stale = next_report_window(&periods, &ends, -200, as_of);
        assert_eq!(stale, wide);
    }

    #[test]
    fn windows_split_into_chunks_that_cover_every_day_exactly_once() {
        let window = DateRange {
            start: day(2026, 1, 1),
            end: day(2026, 1, 31),
        };
        let chunks = split_window(window, 14);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].end, day(2026, 1, 14));
        assert_eq!(chunks[1].start, day(2026, 1, 15));
        assert_eq!(
            chunks[2],
            DateRange {
                start: day(2026, 1, 29),
                end: day(2026, 1, 31),
            }
        );
    }

    #[test]
    fn period_end_prefers_the_reported_fiscal_end_over_the_calendar_quarter() {
        let period = ConsensusPeriod {
            fiscal_year: 2027,
            fiscal_period: "2".into(),
            label: "Q2 2027".into(),
            is_released: true,
            eps: tick_provider::api::ConsensusMetric {
                estimate: None,
                actual: None,
                surprise: None,
            },
            revenue: tick_provider::api::ConsensusMetric {
                estimate: None,
                actual: None,
                surprise: None,
            },
        };
        let reported = time::Date::from_calendar_date(2026, time::Month::July, 26).unwrap();
        let ends = vec![FiscalPeriodEnd {
            fiscal_year: 2027,
            fiscal_period: "2".into(),
            end: reported,
        }];
        assert_eq!(period_end(&ends, &period), Some(reported));
        assert_eq!(
            period_end(&[], &period),
            time::Date::from_calendar_date(2027, time::Month::June, 30).ok()
        );
    }

    #[test]
    fn named_column_indices_match_their_headers() {
        let expected = [
            (PERIOD, "Period"),
            (REPORT_DATE, "Report Date"),
            (EPS_ESTIMATE, "EPS Estimate"),
            (EPS_ACTUAL, "EPS Actual"),
            (EPS_SURPRISE, "EPS Surprise"),
            (REVENUE_ESTIMATE, "Revenue Estimate"),
            (REVENUE_ACTUAL, "Revenue Actual"),
            (REVENUE_SURPRISE, "Revenue Surprise"),
            (BEFORE_20D, "Before 20d"),
            (BEFORE_5D, "Before 5d"),
            (DAY, "Day"),
            (AFTER_5D, "After 5d"),
            (AFTER_20D, "After 20d"),
            (DAY_BAR, "Day Bar"),
        ];
        assert_eq!(HEADERS.len(), expected.len());
        for (index, header) in expected {
            assert_eq!(HEADERS[index], header);
        }
    }
}
