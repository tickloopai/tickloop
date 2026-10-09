use std::sync::Arc;

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use rust_decimal::Decimal;
use tick_market::{
    args::{EarningsArgs, OnceArgs, OutputFormat},
    commands::earnings::{EarningsRequest, load, run_live, run_once, until_daily_refresh},
    config::Settings,
    earnings::{EarningsInputs, EarningsTable, assemble, price_windows, trading_day_windows},
    metadata::TickKind,
};
use tick_provider::{
    api::{
        Candle, CandleHistoryRequest, ConsensusMetric, ConsensusPeriod, DateRange, FiscalPeriodEnd,
        Quote, QuoteApi, QuoteServices, ReleaseSession, ReportCalendarPage, ReportCalendarRequest,
        ReportDate, ReportEvent,
    },
    error::{Error, Result},
    exit_code,
    fiscal::fiscal_period_end,
};
use tick_sdk::stream::TickStream;
use tokio::io::AsyncBufReadExt;

const SESSION_COUNT: usize = 51;
const HOLIDAY_INDEX: usize = 12;
const FIRST_SESSION: time::Date = time::macros::date!(2026 - 04 - 06);

fn date(text: &str) -> time::Date {
    tick_provider::dates::parse_iso_date(text).expect("a calendar date")
}

/// Weekdays from a fixed Monday with one holiday removed, so that offsets counted in sessions
/// differ from offsets counted in calendar days.
fn sessions() -> Vec<time::Date> {
    let mut days = Vec::new();
    let mut day = FIRST_SESSION;
    let mut skipped = false;
    while days.len() < SESSION_COUNT {
        let is_weekend = matches!(
            day.weekday(),
            time::Weekday::Saturday | time::Weekday::Sunday
        );
        if !is_weekend {
            if days.len() == HOLIDAY_INDEX && !skipped {
                skipped = true;
            } else {
                days.push(day);
            }
        }
        day += time::Duration::days(1);
    }
    days
}

/// One close per session, rising by one so that a move reveals which sessions were compared.
fn closes(days: &[time::Date]) -> Vec<Candle> {
    days.iter()
        .enumerate()
        .map(|(index, day)| Candle {
            timestamp: day.midnight().assume_utc(),
            open: Decimal::from(100 + index as i64),
            high: Decimal::from(100 + index as i64),
            low: Decimal::from(100 + index as i64),
            close: Decimal::from(100 + index as i64),
            volume: 1,
            turnover: Decimal::from(100 + index as i64),
        })
        .collect()
}

fn period(label: &str, year: i32, quarter: &str, eps: ConsensusMetric) -> ConsensusPeriod {
    ConsensusPeriod {
        fiscal_year: year,
        fiscal_period: quarter.into(),
        label: label.into(),
        is_released: true,
        eps,
        revenue: ConsensusMetric {
            estimate: Some(Decimal::from(20_000_000_000_i64)),
            actual: Some(Decimal::from(21_000_000_000_i64)),
            surprise: Some(Decimal::from(1_000_000_000_i64)),
        },
    }
}

fn metric(estimate: Option<i64>, actual: Option<i64>) -> ConsensusMetric {
    ConsensusMetric {
        estimate: estimate.map(|value| Decimal::new(value, 2)),
        actual: actual.map(|value| Decimal::new(value, 2)),
        surprise: None,
    }
}

fn event(day: time::Date, session: ReleaseSession, year: i32, quarter: &str) -> ReportEvent {
    ReportEvent {
        symbol: "TSLA.US".into(),
        date: day,
        session,
        fiscal_year: Some(year),
        fiscal_period: Some(quarter.into()),
        eps: ConsensusMetric::default(),
        revenue: ConsensusMetric::default(),
    }
}

fn inputs(periods: Vec<ConsensusPeriod>, events: Vec<ReportEvent>) -> EarningsInputs {
    let days = sessions();
    EarningsInputs {
        symbol: "TSLA.US".into(),
        count: 10,
        periods,
        report_dates: Vec::new(),
        period_ends: Vec::new(),
        events,
        next_report: None,
        candles: closes(&days),
        trading_days: days,
    }
}

fn decimal(value: &str) -> Decimal {
    value.parse().expect("a decimal")
}

#[test]
fn trading_day_offsets_count_sessions_across_weekends_and_holidays() {
    let days = sessions();
    let table = assemble(&inputs(
        vec![period("Q2 2026", 2026, "2", metric(Some(30), Some(35)))],
        vec![event(days[25], ReleaseSession::AfterHours, 2026, "2")],
    ));

    let row = &table.rows[0];
    assert_eq!(
        row.report_date,
        Some(tick_provider::dates::iso_date(days[25]))
    );
    assert_eq!(row.session, Some("after_hours"));
    // close 125 into close 126: the session after an after-hours release.
    assert_eq!(row.day, Some(decimal("0.80")));
    assert_eq!(row.before_5d, Some(decimal("4.17")));
    assert_eq!(row.before_20d, Some(decimal("19.05")));
    assert_eq!(row.after_5d, Some(decimal("3.97")));
    assert_eq!(row.after_20d, Some(decimal("15.87")));
}

#[test]
fn a_pre_market_release_reacts_on_the_report_day_itself() {
    let days = sessions();
    let table = assemble(&inputs(
        vec![period("Q2 2026", 2026, "2", metric(Some(30), Some(35)))],
        vec![event(days[25], ReleaseSession::PreMarket, 2026, "2")],
    ));

    // close 124 into close 125, one session earlier than the after-hours rule.
    assert_eq!(table.rows[0].day, Some(decimal("0.81")));
    assert_eq!(table.rows[0].before_5d, Some(decimal("4.20")));
    assert_eq!(table.rows[0].session, Some("pre_market"));
}

#[test]
fn an_unfinished_post_earnings_window_stays_empty() {
    let days = sessions();
    let table = assemble(&inputs(
        vec![period("Q3 2026", 2026, "3", metric(Some(30), Some(35)))],
        vec![event(days[44], ReleaseSession::AfterHours, 2026, "3")],
    ));

    let row = &table.rows[0];
    assert!(row.day.is_some());
    assert!(row.after_5d.is_some());
    assert_eq!(row.after_20d, None);
    assert_eq!(table.summary.after_20d, None);
}

#[test]
fn a_period_without_prices_keeps_its_fundamentals_and_drops_every_move() {
    let table = assemble(&inputs(
        vec![period("Q1 2019", 2019, "1", metric(Some(30), Some(35)))],
        vec![event(
            date("2019-04-25"),
            ReleaseSession::AfterHours,
            2019,
            "1",
        )],
    ));

    let row = &table.rows[0];
    assert_eq!(row.eps_actual, Some(decimal("0.35")));
    assert_eq!(row.report_date.as_deref(), Some("2019-04-25"));
    assert_eq!(row.day, None);
    assert_eq!(row.before_20d, None);
    assert_eq!(table.summary.up_days, 0);
}

#[test]
fn unreleased_periods_never_reach_the_rows() {
    let days = sessions();
    let mut unreleased = period("Q4 2026", 2026, "4", metric(Some(30), None));
    unreleased.is_released = false;
    let table = assemble(&inputs(
        vec![
            unreleased,
            period("Q2 2026", 2026, "2", metric(Some(30), Some(35))),
        ],
        vec![event(days[25], ReleaseSession::AfterHours, 2026, "2")],
    ));

    assert_eq!(table.rows.len(), 1);
    assert_eq!(table.rows[0].period, "Q2 2026");
    assert_eq!(table.summary.periods, 1);
}

#[test]
fn a_missing_estimate_or_actual_is_shown_missing_and_left_out_of_the_counts() {
    let days = sessions();
    let table = assemble(&inputs(
        vec![
            period("Q2 2026", 2026, "2", metric(None, Some(35))),
            period("Q1 2026", 2026, "1", metric(Some(30), None)),
            period("Q4 2025", 2025, "4", metric(Some(30), Some(35))),
        ],
        vec![
            event(days[25], ReleaseSession::AfterHours, 2026, "2"),
            event(days[30], ReleaseSession::AfterHours, 2026, "1"),
            event(days[35], ReleaseSession::AfterHours, 2025, "4"),
        ],
    ));

    assert_eq!(table.rows[0].eps_surprise, None);
    assert_eq!(table.rows[1].eps_surprise, None);
    assert_eq!(table.rows[2].eps_surprise, Some(decimal("0.05")));
    assert_eq!(table.summary.eps_beats, 1);
    assert_eq!(table.summary.revenue_beats, 3);
    assert_eq!(table.summary.up_days, 3);
    assert_eq!(table.summary.eps_beats_and_up, 1);
}

#[test]
fn fewer_periods_than_requested_still_produce_both_summary_rows() {
    let days = sessions();
    let table = assemble(&inputs(
        vec![period("Q2 2026", 2026, "2", metric(Some(30), Some(35)))],
        vec![event(days[25], ReleaseSession::AfterHours, 2026, "2")],
    ));

    let rendered = tick_market::earnings::table(&table);
    assert_eq!(rendered.rows.len(), 3);
    assert_eq!(rendered.rows[1][0], serde_json::json!("Mean"));
    assert_eq!(rendered.rows[2][0], serde_json::json!("Counts"));
    assert_eq!(rendered.rows[2][4], serde_json::json!("beat 1"));
    assert_eq!(rendered.rows[2][13], serde_json::json!("both 1"));
    assert_eq!(table.summary.day, table.rows[0].day);
    assert_eq!(table.summary.mean_absolute_day, table.rows[0].day);
}

#[test]
fn the_count_limits_the_rows_to_the_newest_released_periods() {
    let days = sessions();
    let mut request = inputs(
        vec![
            period("Q2 2026", 2026, "2", metric(Some(30), Some(35))),
            period("Q1 2026", 2026, "1", metric(Some(30), Some(35))),
            period("Q4 2025", 2025, "4", metric(Some(30), Some(35))),
        ],
        vec![event(days[25], ReleaseSession::AfterHours, 2026, "2")],
    );
    request.count = 2;

    let table = assemble(&request);
    assert_eq!(
        table
            .rows
            .iter()
            .map(|row| row.period.as_str())
            .collect::<Vec<_>>(),
        ["Q2 2026", "Q1 2026"]
    );
}

#[test]
fn a_reported_date_without_a_calendar_event_leaves_the_session_and_the_moves_unknown() {
    let days = sessions();
    let mut request = inputs(
        vec![period("Q2 2026", 2026, "2", metric(Some(30), Some(35)))],
        Vec::new(),
    );
    request.report_dates = vec![ReportDate {
        fiscal_year: 2026,
        fiscal_period: "2".into(),
        date: days[25],
    }];

    let table = assemble(&request);
    assert_eq!(table.rows[0].report_date, Some("2026-05-12".to_owned()));
    assert_eq!(table.rows[0].session, None, "no event names the session");
    assert_eq!(
        table.rows[0].day, None,
        "which session reacted is unknown, so the earnings-day move is not guessed"
    );
    assert_eq!(table.rows[0].before_5d, None);
    assert_eq!(table.rows[0].after_5d, None);
}

#[test]
fn the_next_report_and_its_session_head_the_title() {
    let days = sessions();
    let mut request = inputs(
        vec![period("Q2 2026", 2026, "2", metric(Some(30), Some(35)))],
        vec![event(days[25], ReleaseSession::AfterHours, 2026, "2")],
    );
    request.next_report = Some(event(
        date("2026-10-21"),
        ReleaseSession::AfterHours,
        2026,
        "3",
    ));

    let table = assemble(&request);
    assert_eq!(
        tick_market::earnings::title(&table),
        "TSLA.US · Earnings · Next 2026-10-21 (after hours)"
    );
    request.next_report = None;
    assert_eq!(
        tick_market::earnings::title(&assemble(&request)),
        "TSLA.US · Earnings"
    );
}

#[test]
fn price_windows_merge_and_split_into_ranges_the_endpoints_accept() {
    let as_of = date("2026-09-21");
    let windows = price_windows(&[date("2026-07-22"), date("2026-04-23")], as_of);
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].start, date("2026-03-12"));
    assert_eq!(windows[1].end, date("2026-09-02"));

    let clipped = price_windows(&[date("2026-09-10")], as_of);
    assert_eq!(clipped[0].end, as_of, "a window never runs past today");

    for chunk in trading_day_windows(&windows) {
        assert!((chunk.end - chunk.start).whole_days() < 30);
    }
    assert_eq!(
        trading_day_windows(&windows)
            .first()
            .map(|chunk| chunk.start),
        Some(windows[0].start)
    );
}

#[test]
fn fiscal_quarters_end_on_the_last_day_of_their_quarter() {
    assert_eq!(fiscal_period_end(2026, "1"), Some(date("2026-03-31")));
    assert_eq!(fiscal_period_end(2026, "Q2"), Some(date("2026-06-30")));
    assert_eq!(fiscal_period_end(2026, "4"), Some(date("2026-12-31")));
    assert_eq!(fiscal_period_end(2026, "annual"), None);
}

#[test]
fn the_daily_reload_lands_on_the_next_refresh_hour() {
    let before = time::macros::datetime!(2026-09-21 05:30 UTC);
    assert_eq!(
        until_daily_refresh(before),
        std::time::Duration::from_secs(30 * 60)
    );
    let after = time::macros::datetime!(2026-09-21 06:30 UTC);
    assert_eq!(
        until_daily_refresh(after),
        std::time::Duration::from_secs(23 * 3600 + 30 * 60)
    );
}

#[test]
fn only_us_symbols_and_bounded_counts_are_accepted() {
    assert_eq!(
        EarningsRequest::new("tsla", None).unwrap(),
        EarningsRequest {
            symbol: "TSLA.US".into(),
            count: 10,
        }
    );
    let rejected = EarningsRequest::new("700.HK", None).unwrap_err();
    assert_eq!(rejected.code, exit_code::USAGE);
    assert!(rejected.message.contains("US symbols only"), "{rejected}");
    assert_eq!(
        EarningsRequest::new("700", None).unwrap_err().code,
        exit_code::USAGE
    );
    assert_eq!(
        EarningsRequest::new("TSLA.US", Some(21)).unwrap_err().code,
        exit_code::USAGE
    );
    assert_eq!(EarningsRequest::new("TSLA.US", Some(20)).unwrap().count, 20);
}

/// The five fundamentals and calendar calls, answered the way an AP-region authorization does:
/// the US key financial metrics endpoint refuses, so the report dates come from the calendar.
struct Fundamentals {
    as_of: time::Date,
    quarters: Vec<(i32, u8)>,
    /// Older quarters the consensus no longer carries but the calendar and the income
    /// statement still know, newest first.
    calendar_only_quarters: Vec<(i32, u8)>,
    epoch: i64,
}

const CONSENSUS_QUARTERS: usize = 4;
const CALENDAR_EPS: (Option<i64>, Option<i64>) = (Some(30), Some(33));

impl Fundamentals {
    fn new(as_of: time::Date) -> Self {
        Self::with_calendar_only_quarters(as_of, 0)
    }

    fn with_calendar_only_quarters(as_of: time::Date, calendar_only: usize) -> Self {
        let mut quarters = Vec::new();
        let mut year = as_of.year();
        let mut quarter = 4;
        while quarters.len() < CONSENSUS_QUARTERS + calendar_only {
            if let Some(end) = fiscal_period_end(year, &quarter.to_string())
                && end + time::Duration::days(60) <= as_of
            {
                quarters.push((year, quarter as u8));
            }
            quarter -= 1;
            if quarter == 0 {
                quarter = 4;
                year -= 1;
            }
        }
        let calendar_only_quarters = quarters.split_off(CONSENSUS_QUARTERS);
        Self {
            as_of,
            quarters,
            calendar_only_quarters,
            epoch: i64::from(as_of.to_julian_day()) - 800,
        }
    }

    fn report_date(&self, year: i32, quarter: u8) -> time::Date {
        fiscal_period_end(year, &quarter.to_string()).expect("a quarter end")
            + time::Duration::days(21)
    }

    fn all_quarters(&self) -> impl Iterator<Item = (i32, u8)> + '_ {
        self.quarters
            .iter()
            .chain(&self.calendar_only_quarters)
            .copied()
    }
}

#[async_trait]
impl QuoteApi for Fundamentals {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!("the Earnings Tick never asks for quotes")
    }
    async fn consensus(&self, _: &str) -> Result<Vec<ConsensusPeriod>> {
        Ok(self
            .quarters
            .iter()
            .enumerate()
            .map(|(index, (year, quarter))| {
                let beat = index % 2 == 0;
                period(
                    &format!("Q{quarter} {year}"),
                    *year,
                    &quarter.to_string(),
                    metric(Some(30), Some(if beat { 35 } else { 25 })),
                )
            })
            .collect())
    }
    async fn key_financial_metrics(&self, _: &str) -> Result<Vec<ReportDate>> {
        Err(Error::auth(
            "this API is only available in the US data center",
        ))
    }
    async fn fiscal_period_ends(&self, _: &str) -> Result<Vec<FiscalPeriodEnd>> {
        Ok(self
            .all_quarters()
            .map(|(year, quarter)| FiscalPeriodEnd {
                fiscal_year: year,
                fiscal_period: quarter.to_string(),
                end: fiscal_period_end(year, &quarter.to_string()).expect("a quarter end"),
            })
            .collect())
    }
    async fn report_calendar(&self, request: &ReportCalendarRequest) -> Result<ReportCalendarPage> {
        Ok(ReportCalendarPage {
            events: self
                .all_quarters()
                .map(|(year, quarter)| ReportEvent {
                    symbol: request.symbol.clone(),
                    eps: metric(CALENDAR_EPS.0, CALENDAR_EPS.1),
                    ..event(
                        self.report_date(year, quarter),
                        ReleaseSession::AfterHours,
                        year,
                        &quarter.to_string(),
                    )
                })
                .filter(|event| {
                    event.date >= request.range.start && event.date <= request.range.end
                })
                .collect(),
            next_date: None,
        })
    }
    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        // Closes rise with the calendar, so every earnings day in this fixture is an up day.
        Ok(self
            .trading_days("US", request.range)
            .await?
            .into_iter()
            .map(|day| {
                let level = Decimal::from(100 + i64::from(day.to_julian_day()) - self.epoch);
                Candle {
                    timestamp: day.midnight().assume_utc(),
                    open: level,
                    high: level,
                    low: level,
                    close: level,
                    volume: 1,
                    turnover: level,
                }
            })
            .collect())
    }
    async fn trading_days(&self, market: &str, range: DateRange) -> Result<Vec<time::Date>> {
        assert_eq!(market, "US");
        assert!(range.end <= self.as_of);
        let mut days = Vec::new();
        let mut day = range.start;
        while day <= range.end {
            if !matches!(
                day.weekday(),
                time::Weekday::Saturday | time::Weekday::Sunday
            ) {
                days.push(day);
            }
            day += time::Duration::days(1);
        }
        Ok(days)
    }
}

fn args(format: OutputFormat) -> EarningsArgs {
    EarningsArgs {
        symbol: "TSLA".into(),
        count: None,
        output: OnceArgs { once: true, format },
    }
}

async fn once(api: &QuoteServices, format: OutputFormat) -> Result<(String, TickStream)> {
    let mut output = Vec::new();
    let mut updates = TickStream::in_memory(TickKind::Earnings, "longbridge");
    run_once(
        &args(format),
        api,
        &Settings::default(),
        false,
        &mut output,
        &mut updates,
    )
    .await?;
    Ok((String::from_utf8(output).expect("utf-8 output"), updates))
}

#[tokio::test]
async fn once_json_carries_the_rows_and_the_summary_the_agent_reads() {
    let api = Fundamentals::new(time::OffsetDateTime::now_utc().date());
    let (output, updates) = once(&api, OutputFormat::Json).await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&output).unwrap();

    assert_eq!(json["symbol"], "TSLA.US");
    assert_eq!(json["rows"].as_array().unwrap().len(), 4);
    assert_eq!(json["summary"]["periods"], 4);
    assert_eq!(json["summary"]["eps_beats"], 2);
    assert_eq!(json["summary"]["up_days"], 4);
    assert_eq!(json["summary"]["eps_beats_and_up"], 2);
    assert_eq!(json["rows"][0]["session"], "after_hours");
    assert_eq!(
        json["rows"][0]["report_date"],
        serde_json::json!(tick_provider::dates::iso_date(
            api.report_date(api.quarters[0].0, api.quarters[0].1)
        ))
    );
    assert!(json["rows"][0]["day"].is_string());
    assert_eq!(updates.records().len(), 1);
    assert_eq!(updates.records().all()[0]["data"], json);
}

#[tokio::test]
async fn once_table_prints_the_columns_the_bars_and_both_summary_rows() {
    let api = Fundamentals::new(time::OffsetDateTime::now_utc().date());
    let (output, _) = once(&api, OutputFormat::Table).await.unwrap();

    for heading in ["Period", "Report Date", "EPS Surprise", "Day", "Day Bar"] {
        assert!(output.contains(heading), "missing {heading}:\n{output}");
    }
    assert!(output.contains("Mean"), "{output}");
    assert!(output.contains("beat 2"), "{output}");
    assert!(output.contains("up 4"), "{output}");
    assert!(output.contains("both 2"), "{output}");
    assert!(output.contains('▇'), "{output}");
    assert!(!output.contains('\u{1b}'));
}

struct Unentitled;

#[async_trait]
impl QuoteApi for Unentitled {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!("the Earnings Tick never asks for quotes")
    }
    async fn consensus(&self, _: &str) -> Result<Vec<ConsensusPeriod>> {
        Err(Error::auth(
            "Longbridge entitlement does not cover consensus",
        ))
    }
}

#[tokio::test]
async fn an_entitlement_gap_fails_instead_of_printing_an_empty_table() {
    let mut output = Vec::new();
    let error = run_once(
        &args(OutputFormat::Table),
        &Unentitled,
        &Settings::default(),
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Earnings, "longbridge"),
    )
    .await
    .unwrap_err();

    assert_eq!(error.code, exit_code::NOT_AUTHORIZED);
    assert!(output.is_empty(), "no table is printed for a refusal");
}

async fn panel(api: &QuoteServices, columns: u16, rows: u16) -> String {
    use crossterm::event::{Event, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
    let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
        crossterm::event::KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))]);
    tick_market::commands::earnings::run_live(
        &args(OutputFormat::Table),
        api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut TickStream::in_memory(TickKind::Earnings, "longbridge"),
    )
    .await
    .unwrap();
    (0..rows)
        .map(|row| {
            (0..columns)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_panel_below_the_minimum_size_scrolls_over_the_full_layout() {
    let api = Fundamentals::new(time::OffsetDateTime::now_utc().date());
    let screen = panel(&api, 61, 12).await;

    assert!(screen.contains("Compare periods"), "{screen}");
    assert!(screen.contains("< > ↕ 1–11/14 { } scroll"), "{screen}");
}

#[tokio::test]
async fn the_minimum_panel_draws_fiscal_comparisons_and_source_time() {
    let api = Fundamentals::new(time::OffsetDateTime::now_utc().date());
    let minimum = TickKind::Earnings.minimum_panel_size();
    let screen = panel(&api, minimum.columns, minimum.rows).await;

    assert!(screen.contains("TSLA.US · Earnings"), "{screen}");
    for heading in ["Period", "EPS Act", "Revenue", "YoY"] {
        assert!(screen.contains(heading), "missing {heading}:\n{screen}");
    }
    assert!(screen.contains("Longbridge"), "{screen}");
    assert!(screen.contains("Compare periods"), "{screen}");
    assert!(screen.contains("? Help  q Quit"), "{screen}");
}

#[tokio::test]
async fn the_symbol_prompt_owns_the_keys_and_reloads_the_panel_in_place() {
    use crossterm::event::KeyCode;
    use tick_market::{
        commands::earnings::EarningsView,
        runtime::{LiveView, ViewState},
    };

    let as_of = time::OffsetDateTime::now_utc().date();
    let api = Fundamentals::new(as_of);
    let request = EarningsRequest::new("TSLA.US", Some(2)).unwrap();
    let mut view = EarningsView::new(
        request,
        load(&api, &request_of("TSLA.US"), as_of).await.unwrap(),
    );
    let mut state = ViewState::default();

    assert!(!view.captures_text());
    assert!(
        view.on_key(KeyCode::Char('j'), &api, &mut state)
            .await
            .unwrap()
    );
    assert!(
        view.on_key(KeyCode::Char('s'), &api, &mut state)
            .await
            .unwrap()
    );
    assert!(
        view.captures_text(),
        "the prompt owns the keys while it is open"
    );

    for character in "700.hk".chars() {
        view.on_key(KeyCode::Char(character), &api, &mut state)
            .await
            .unwrap();
    }
    view.on_key(KeyCode::Enter, &api, &mut state).await.unwrap();
    assert!(
        view.captures_text(),
        "a rejected symbol keeps the prompt open"
    );
    assert!(view.title().contains("TSLA.US"));

    for _ in 0.."700.HK".len() {
        view.on_key(KeyCode::Backspace, &api, &mut state)
            .await
            .unwrap();
    }
    for character in "aapl".chars() {
        view.on_key(KeyCode::Char(character), &api, &mut state)
            .await
            .unwrap();
    }
    view.on_key(KeyCode::Enter, &api, &mut state).await.unwrap();
    assert!(!view.captures_text());
    assert!(view.title().contains("AAPL.US"), "{}", view.title());

    view.on_key(KeyCode::Char('s'), &api, &mut state)
        .await
        .unwrap();
    for key in [KeyCode::Esc, KeyCode::Backspace] {
        view.on_key(key, &api, &mut state).await.unwrap();
        assert!(
            view.captures_text(),
            "{key:?} in an empty prompt leaves it open"
        );
    }
    view.cancel_text();
    assert!(!view.captures_text(), "cancelling closes the prompt");
    assert!(view.title().contains("AAPL.US"));
}

#[tokio::test]
async fn in_the_running_tick_the_prompt_keeps_q_as_text_and_ctrl_c_cancels_it() {
    let api = Fundamentals::new(today());
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let keyboard = tokio::spawn(async move {
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let typed = |text: &'static str| text.chars().map(move |key| press(KeyCode::Char(key)));
        let events = typed("sqx")
            .chain([KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)])
            .chain(typed("saaplz"))
            .chain([KeyCode::Backspace, KeyCode::Enter, KeyCode::Char('q')].map(press));
        for event in events {
            tokio::time::sleep(std::time::Duration::from_millis(45)).await;
            keys.send(Ok(Event::Key(event))).unwrap();
        }
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    run_live(
        &args(OutputFormat::Table),
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut TickStream::in_memory(TickKind::Earnings, "longbridge"),
    )
    .await
    .unwrap();
    keyboard.await.unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains(" AAPL.US · Earnings"),
        "Ctrl+C discarded the first prompt and Backspace edited the second: {screen}"
    );
}

fn request_of(symbol: &str) -> EarningsRequest {
    EarningsRequest::new(symbol, Some(2)).unwrap()
}

#[tokio::test]
async fn loading_asks_the_calendar_only_inside_the_windows_it_needs() {
    let as_of = time::OffsetDateTime::now_utc().date();
    let api = Fundamentals::new(as_of);
    let table: EarningsTable = load(
        &api,
        &EarningsRequest::new("TSLA.US", Some(2)).unwrap(),
        as_of,
    )
    .await
    .unwrap();

    assert_eq!(table.rows.len(), 2);
    assert!(table.rows.iter().all(|row| row.report_date.is_some()));
    assert!(table.rows.iter().all(|row| row.day.is_some()));
}

#[tokio::test]
async fn periods_the_consensus_no_longer_carries_come_from_the_calendar() {
    let as_of = time::OffsetDateTime::now_utc().date();
    let api = Fundamentals::with_calendar_only_quarters(as_of, 2);
    let table: EarningsTable = load(
        &api,
        &EarningsRequest::new("TSLA.US", Some(6)).unwrap(),
        as_of,
    )
    .await
    .unwrap();

    assert_eq!(table.rows.len(), 6);
    assert_eq!(table.summary.periods, 6);
    let (year, quarter) = api.calendar_only_quarters[0];
    let from_calendar = &table.rows[4];
    assert_eq!(from_calendar.period, format!("Q{quarter} {year}"));
    assert_eq!(
        from_calendar.report_date.as_deref(),
        Some(tick_provider::dates::iso_date(api.report_date(year, quarter)).as_str())
    );
    assert_eq!(from_calendar.eps_estimate, Some(Decimal::new(30, 2)));
    assert_eq!(from_calendar.eps_actual, Some(Decimal::new(33, 2)));
    assert_eq!(from_calendar.eps_surprise, Some(Decimal::new(3, 2)));
    assert!(from_calendar.revenue_actual.is_none());
    assert!(from_calendar.day.is_some());
    assert!(
        table.rows[..4]
            .iter()
            .all(|row| row.eps_estimate == Some(Decimal::new(30, 2)))
    );
    assert_eq!(table.rows[3].eps_actual, Some(Decimal::new(25, 2)));
}

/// Holds the `history` call for one symbol until the test releases it, so that what the Panel
/// answers while a Tick Tool call is still running is observable, and records every consensus
/// call so that the Panel and its tools are seen using one Longbridge client.
struct Gated {
    inner: Fundamentals,
    held_symbol: String,
    gate: Arc<tokio::sync::Semaphore>,
    consensus_symbols: std::sync::Mutex<Vec<String>>,
}

impl Gated {
    fn new(as_of: time::Date, held_symbol: &str, gate: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            inner: Fundamentals::new(as_of),
            held_symbol: held_symbol.into(),
            gate,
            consensus_symbols: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn consensus_symbols(&self) -> Vec<String> {
        self.consensus_symbols.lock().unwrap().clone()
    }
}

#[async_trait]
impl QuoteApi for Gated {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        self.inner.quotes(symbols).await
    }
    async fn consensus(&self, symbol: &str) -> Result<Vec<ConsensusPeriod>> {
        self.consensus_symbols.lock().unwrap().push(symbol.into());
        if symbol == self.held_symbol {
            let _permit = self.gate.acquire().await.expect("the gate stays open");
        }
        self.inner.consensus(symbol).await
    }
    async fn key_financial_metrics(&self, symbol: &str) -> Result<Vec<ReportDate>> {
        self.inner.key_financial_metrics(symbol).await
    }
    async fn report_calendar(&self, request: &ReportCalendarRequest) -> Result<ReportCalendarPage> {
        self.inner.report_calendar(request).await
    }
    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        self.inner.candles_by_date(request).await
    }
    async fn trading_days(&self, market: &str, range: DateRange) -> Result<Vec<time::Date>> {
        self.inner.trading_days(market, range).await
    }
}

/// Key presses delivered to a running Panel, ending only when the test drops the sender.
struct Keys(tokio::sync::mpsc::UnboundedReceiver<Event>);

impl futures_util::Stream for Keys {
    type Item = std::io::Result<Event>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(context).map(|event| event.map(Ok))
    }
}

const FRAME_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// An Earnings Panel running over an in-memory duplex transport, so that stream requests, key
/// presses and the frames the Tick writes back can be interleaved inside one test.
struct Panel {
    requests: tokio::io::DuplexStream,
    frames: tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
    keys: tokio::sync::mpsc::UnboundedSender<Event>,
    run: tokio::task::JoinHandle<Result<()>>,
}

impl Panel {
    fn start(api: Arc<dyn QuoteApi>, symbol: &str, count: Option<usize>) -> Self {
        let (requests, tick_requests) = tokio::io::duplex(8 * 1024);
        let (tick_responses, responses) = tokio::io::duplex(64 * 1024);
        let mut session = TickStream::duplex(
            TickKind::Earnings,
            "longbridge",
            "herdr:test-earnings".to_owned(),
            tick_requests,
            tick_responses,
        );
        let (keys, pressed) = tokio::sync::mpsc::unbounded_channel();
        let arguments = EarningsArgs {
            symbol: symbol.into(),
            count,
            output: OnceArgs::default(),
        };
        let run = tokio::spawn(async move {
            let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
            let result = run_live(
                &arguments,
                api.as_ref(),
                &Settings::default(),
                &mut terminal,
                Keys(pressed),
                &mut session,
            )
            .await;
            session.close().await;
            result
        });
        Self {
            requests,
            frames: tokio::io::BufReader::new(responses).lines(),
            keys,
            run,
        }
    }

    async fn request(&mut self, request: serde_json::Value) {
        let mut line = serde_json::to_vec(&request).unwrap();
        line.push(b'\n');
        tokio::io::AsyncWriteExt::write_all(&mut self.requests, &line)
            .await
            .unwrap();
    }

    fn press(&self, code: KeyCode) {
        self.keys
            .send(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .unwrap();
    }

    fn type_text(&self, text: &str) {
        for character in text.chars() {
            self.press(KeyCode::Char(character));
        }
    }

    async fn frame(&mut self) -> serde_json::Value {
        loop {
            let line = tokio::time::timeout(FRAME_DEADLINE, self.frames.next_line())
                .await
                .expect("the Tick wrote a frame")
                .unwrap()
                .expect("the Tick kept its stream open");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["method"] != "tick/context" {
                return frame;
            }
        }
    }

    async fn next_frames(&mut self, count: usize) -> Vec<serde_json::Value> {
        let mut collected = Vec::new();
        while collected.len() < count {
            collected.push(self.frame().await);
        }
        collected
    }

    /// Closes the request stream and returns every frame the Tick still had to write.
    async fn finish(mut self) -> Vec<serde_json::Value> {
        tokio::io::AsyncWriteExt::shutdown(&mut self.requests)
            .await
            .unwrap();
        let mut frames = Vec::new();
        loop {
            let line = tokio::time::timeout(FRAME_DEADLINE, self.frames.next_line())
                .await
                .expect("the Tick closed its stream")
                .unwrap();
            match line {
                Some(line) => frames.push(serde_json::from_str(&line).unwrap()),
                None => break,
            }
        }
        self.run.await.unwrap().unwrap();
        frames
    }
}

fn history_call(id: u64, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tool/call",
        "params": {"tool": "history", "args": args},
    })
}

fn answer(frames: &[serde_json::Value], id: u64) -> &serde_json::Value {
    frames
        .iter()
        .find(|frame| frame["id"] == id)
        .unwrap_or_else(|| panic!("request {id} was answered: {frames:?}"))
}

fn published_updates(frames: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "tick/update")
        .map(|frame| &frame["params"])
        .collect()
}

fn today() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

#[tokio::test]
async fn earnings_declares_history_as_the_tool_that_recovers_its_updates() {
    let mut panel = Panel::start(Arc::new(Fundamentals::new(today())), "TSLA", Some(2));
    panel.frame().await;

    panel
        .request(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tick/describe"}))
        .await;
    let describe = panel.frame().await;

    assert_eq!(describe["result"]["tick_type"], "earnings");
    assert_eq!(describe["result"]["recover_with"], "history");
    let tools = describe["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    assert_eq!(tools[1]["name"], "detail");
    assert_eq!(tools[2]["name"], "select_period");
    assert_eq!(tools[2]["mutates"], true);
    assert_eq!(tools[0]["name"], "history");
    assert_eq!(tools[0]["mutates"], false);
    let params = &tools[0]["params"];
    assert_eq!(params["required"], serde_json::json!(["symbol"]));
    assert_eq!(params["properties"]["symbol"]["type"], "string");
    assert_eq!(params["properties"]["count"]["maximum"], 20);
    assert_eq!(params["additionalProperties"], false);
    panel.finish().await;
}

#[tokio::test]
async fn a_history_call_reads_another_symbol_while_the_panel_keeps_answering() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let api = Arc::new(Gated::new(today(), "NVDA.US", gate.clone()));
    let mut panel = Panel::start(api.clone(), "TSLA", Some(2));

    let snapshot = panel.frame().await;
    assert_eq!(snapshot["params"]["data"]["symbol"], "TSLA.US");

    panel
        .request(history_call(
            1,
            serde_json::json!({"symbol": "nvda", "count": 2}),
        ))
        .await;
    panel
        .request(serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tick/describe"}))
        .await;

    let describe = panel.frame().await;
    assert_eq!(
        describe["id"], 2,
        "the Panel answers while the history call is still running: {describe:?}"
    );

    gate.add_permits(1);
    let history = panel.frame().await;
    assert_eq!(history["id"], 1);
    assert_eq!(history["result"]["symbol"], "NVDA.US");
    assert_eq!(history["result"]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(history["result"]["summary"]["periods"], 2);

    let frames = panel.finish().await;
    assert!(
        published_updates(&frames).is_empty(),
        "the Panel published nothing of its own after the snapshot: {frames:?}"
    );
    assert_eq!(
        api.consensus_symbols(),
        ["TSLA.US", "NVDA.US"],
        "the Panel and its Tick Tool calls go through one Longbridge client and its rate limiter"
    );
}

#[tokio::test]
async fn changing_the_panel_symbol_publishes_the_new_table_as_a_tick_update() {
    let mut panel = Panel::start(Arc::new(Fundamentals::new(today())), "TSLA", Some(2));
    let snapshot = panel.frame().await;
    assert_eq!(snapshot["params"]["kind"], "snapshot");
    assert_eq!(snapshot["params"]["data"]["symbol"], "TSLA.US");

    panel.press(KeyCode::Char('s'));
    panel.type_text("nvda");
    panel.press(KeyCode::Enter);

    let update = panel.frame().await;
    assert_eq!(update["method"], "tick/update");
    assert_eq!(update["params"]["kind"], "update");
    assert_eq!(update["params"]["sequence"], 2);
    assert_eq!(update["params"]["data"]["symbol"], "NVDA.US");
    panel.finish().await;
}

#[tokio::test]
async fn a_wrong_history_call_answers_with_the_schema_and_the_ticks_own_limits() {
    let mut panel = Panel::start(Arc::new(Fundamentals::new(today())), "TSLA", Some(2));
    panel.frame().await;

    panel.request(history_call(1, serde_json::json!({}))).await;
    panel
        .request(history_call(2, serde_json::json!({"symbol": "700.HK"})))
        .await;
    panel
        .request(history_call(
            3,
            serde_json::json!({"symbol": "TSLA", "count": 99}),
        ))
        .await;
    let frames = panel.next_frames(3).await;

    let missing = answer(&frames, 1);
    assert_eq!(missing["error"]["code"], -32602);
    assert_eq!(
        missing["error"]["data"]["schema"]["properties"]["count"]["maximum"], 20,
        "the declared schema travels with the error: {missing:?}"
    );
    let non_us = answer(&frames, 2);
    assert_eq!(non_us["error"]["code"], -32000);
    assert!(
        non_us["error"]["message"]
            .as_str()
            .unwrap()
            .contains("US symbols only"),
        "{non_us:?}"
    );
    let too_many = answer(&frames, 3);
    assert_eq!(too_many["error"]["code"], -32000);
    assert!(
        too_many["error"]["message"]
            .as_str()
            .unwrap()
            .contains("between 1 and 20"),
        "{too_many:?}"
    );
    panel.finish().await;
}

#[test]
fn review_matches_fiscal_baselines_and_explains_zero_or_negative_growth() {
    use tick_market::earnings::review::detail;
    let table = assemble(&inputs(
        vec![
            period("Q1 2026", 2026, "1", metric(Some(140), Some(150))),
            period("Q4 2025", 2025, "4", metric(None, Some(-50))),
            period("Q1 2025", 2025, "1", metric(None, Some(100))),
        ],
        Vec::new(),
    ));
    let detail = detail(&table, 0).unwrap();
    assert_eq!(detail.period_id, "FY2026-Q1");
    assert_eq!(detail.metrics[0].yoy.percent, Some(decimal("50")));
    assert_eq!(
        detail.metrics[0].yoy.baseline_period_id.as_deref(),
        Some("FY2025-Q1")
    );
    assert_eq!(detail.metrics[0].qoq.percent, None);
    assert_eq!(detail.metrics[0].qoq.absolute_change, Some(decimal("2")));
    assert_eq!(
        detail.metrics[0].qoq.unavailable_reason,
        Some("baseline_non_positive")
    );
    assert!(detail.estimate_snapshot_at.is_none());
    assert!(detail.release_timestamp.is_none());
    assert!(
        detail
            .estimate_note
            .contains("no verified pre-release snapshot")
    );

    let missing = assemble(&inputs(
        vec![
            period("Q3 2026", 2026, "3", metric(None, Some(150))),
            period("Q1 2026", 2026, "1", metric(None, Some(0))),
        ],
        Vec::new(),
    ));
    let detail = tick_market::earnings::review::detail(&missing, 0).unwrap();
    assert_eq!(
        detail.metrics[0].qoq.unavailable_reason,
        Some("baseline_period_missing")
    );
    assert!(
        detail.metrics[0].qoq.baseline_period_id.is_none(),
        "Q1 must not stand in for missing Q2"
    );
}

#[test]
fn statements_and_segments_attach_only_to_the_exact_reported_period_end() {
    use serde_json::json;
    use tick_market::earnings::review::{attach_segments, attach_statements, detail};
    let mut table = assemble(&inputs(
        vec![
            period("Q2 2026", 2026, "2", metric(None, Some(150))),
            period("Q1 2026", 2026, "1", metric(None, Some(100))),
        ],
        Vec::new(),
    ));
    let end = date("2026-07-26")
        .midnight()
        .assume_utc()
        .unix_timestamp()
        .to_string();
    attach_statements(
        &mut table,
        &json!({"CF":{"indicators":[{"currency":"USD","accounts":[{
            "field":"OPERATING_CF", "name":"Operating cash flow(USD)", "percent":false,
            "values":[{"period":"Q2 2026","year":2026,"fp_end":end,"value":"100000000","yoy":"0.25"}]
        }]}]}}),
    );
    attach_segments(
        &mut table,
        &json!({"historical":[
            {"date":"20260726","currency":"USD","business":[{"name":"Products","value":"80000000","percent":"80"}]},
            {"date":"20260630","currency":"USD","business":[{"name":"Unmatched","value":"999","percent":"99"}]}
        ]}),
    );
    let detail = detail(&table, 0).unwrap();
    assert_eq!(detail.fiscal_period_end.as_deref(), Some("2026-07-26"));
    assert_eq!(detail.metrics[2].field, "CF:OPERATING_CF");
    assert_eq!(detail.metrics[2].unit, "USD");
    assert_eq!(detail.metrics[2].actual, Some(decimal("100000000")));
    assert!(detail.metrics[2].estimate.is_none());
    assert_eq!(
        detail.statement_items[0].reported_yoy_ratio,
        Some(decimal("0.25"))
    );
    assert_eq!(detail.segments.len(), 1);
    assert_eq!(detail.segments[0].name, "Products");
    assert!(table.rows[1].statement_items.is_empty());
    assert!(table.rows[1].segments.is_empty());
}

#[test]
fn segment_fiscal_identity_overrides_nominal_calendar_end_without_rewriting_actual_end() {
    use serde_json::json;
    use tick_market::earnings::review::attach_segments;
    let mut table = assemble(&inputs(
        vec![
            period("Q3 2026", 2026, "3", metric(None, Some(150))),
            period("Q2 2026", 2026, "2", metric(None, Some(100))),
        ],
        Vec::new(),
    ));
    table.rows[0].fiscal_period_end = Some("2026-06-27".into());
    table.rows[1].fiscal_period_end = Some("2026-03-28".into());
    attach_segments(
        &mut table,
        &json!({"historical":[
            {"date":"20260630","report":"qf","report_txt":"2026.Q3","fp_end":"2026.06.30","currency":"USD",
             "business":[{"name":"Americas","value":"49563000000","percent":"49.9"}],"regionals":[]},
            {"date":"20260331","report":"qf","report_txt":"2026.Q2","fp_end":"2026.03.31","currency":"USD",
             "business":[{"name":"Americas","value":"45000000000","percent":"48.5"}],"regionals":[]},
            {"date":"20260627","report_txt":"2025.Q3","currency":"USD",
             "business":[{"name":"Wrong fiscal year","value":"1"}]},
            {"date":"20260627","report_txt":"2026.H1","currency":"USD",
             "business":[{"name":"Non-quarterly report","value":"1"}]}
        ]}),
    );
    assert_eq!(table.rows[0].segments.len(), 1);
    assert_eq!(table.rows[1].segments.len(), 1);
    assert_eq!(
        table.rows[0].segments[0].revenue,
        Some(decimal("49563000000"))
    );
    assert_eq!(
        table.rows[0].fiscal_period_end.as_deref(),
        Some("2026-06-27")
    );
    assert_eq!(
        table.rows[1].fiscal_period_end.as_deref(),
        Some("2026-03-28")
    );
}

#[tokio::test]
async fn fiscal_navigation_and_agent_selection_keep_history_and_citations_stable() {
    use tick_market::{
        commands::earnings::EarningsView,
        runtime::{LiveView, ViewState},
    };
    use tick_sdk::tools::ToolCall;
    let api = Fundamentals::new(today());
    let table = load(&api, &request_of("TSLA"), today()).await.unwrap();
    let second = table.rows[1].period_id.clone();
    let first = table.rows[0].period_id.clone();
    let mut view = EarningsView::new(request_of("TSLA"), table);
    let original = view.content();
    let mut state = ViewState::default();
    view.on_key(KeyCode::Char('['), &api, &mut state)
        .await
        .unwrap();
    view.on_key(KeyCode::Enter, &api, &mut state).await.unwrap();
    assert!(view.title().contains(&second));
    assert_eq!(view.selected_detail().unwrap().period_id, second);
    view.on_key(KeyCode::Backspace, &api, &mut state)
        .await
        .unwrap();
    assert!(view.title().contains("Compare periods"));
    let result = view
        .act(ToolCall {
            tool: "select_period".into(),
            args: serde_json::json!({"period_id": first}),
        })
        .await
        .unwrap();
    assert_eq!(result["period_id"], first);
    assert_eq!(
        view.content(),
        original,
        "view navigation is not a financial update"
    );
    assert!(
        view.act(ToolCall {
            tool: "select_period".into(),
            args: serde_json::json!({"period_id":"FY1900-Q1"})
        })
        .await
        .is_err()
    );
    assert_eq!(
        view.selected_detail().unwrap().period_id,
        first,
        "invalid selection is atomic"
    );
}

#[tokio::test]
async fn detail_tool_returns_exact_period_and_unavailable_sources_without_claiming_snapshots() {
    use tick_sdk::tools::ToolDispatch;
    let api = Fundamentals::new(today());
    let tools = tick_market::commands::earnings::tools();
    let table = load(&api, &request_of("TSLA"), today()).await.unwrap();
    let period_id = &table.rows[0].period_id;
    let ToolDispatch::Query(query) = tools
        .get("detail")
        .unwrap()
        .prepare(
            &api,
            serde_json::json!({"symbol":"TSLA","period_id":period_id}),
        )
        .unwrap()
    else {
        panic!("query tool")
    };
    let result = query.await.unwrap();
    assert_eq!(result["period_id"], *period_id);
    assert_eq!(result["source"], "longbridge");
    assert!(result["observed_at"].as_str().is_some());
    assert!(result["estimate_snapshot_at"].is_null());
    assert_eq!(result["availability"][0]["available"], false);
    assert!(result["company_filings"].as_array().unwrap().is_empty());
}

#[test]
fn original_materials_match_reported_fiscal_end_and_exclude_unrelated_same_day_filings() {
    use serde_json::json;
    use tick_market::earnings::review::detail;
    let mut table = assemble(&inputs(
        vec![
            period("Q3 2026", 2026, "3", metric(None, Some(202))),
            period("Q2 2026", 2026, "2", metric(None, Some(201))),
        ],
        Vec::new(),
    ));
    table.rows[0].fiscal_period_end = Some("2026-06-27".into());
    table.rows[1].fiscal_period_end = Some("2026-03-28".into());
    table.company_filings = vec![
        json!({"id":"q3","file_name":"10-Q - Apple Inc. (Filer)","title":"Q3 filing","publish_at":"2026-07-31T10:01:02Z","file_urls":["https://www.sec.gov/Archives/edgar/data/320193/000032019326000020/aapl-20260627.htm","https://www.sec.gov/Archives/exhibit31106272026.htm"]}),
        json!({"id":"q3-results","file_name":"8-K - Apple Inc. (Filer)","title":"Results","publish_at":"2026-07-30T20:30:28Z","file_urls":["https://www.sec.gov/Archives/aapl-20260730.htm","https://www.sec.gov/Archives/a8-kex991q3202606272026.htm"]}),
        json!({"id":"q2","file_name":"10-Q - Apple Inc. (Filer)","file_urls":["https://www.sec.gov/Archives/aapl-20260328.htm"]}),
        json!({"id":"insider","file_name":"4 - Apple Inc. (Issuer)","file_urls":["https://www.sec.gov/Archives/aapl-20260627.htm"]}),
        json!({"id":"other-event","file_name":"8-K - Apple Inc. (Filer)","publish_at":"2026-07-30T20:30:28Z","file_urls":["https://www.sec.gov/Archives/other-20260730.htm"]}),
    ];
    let q3 = detail(&table, 0).unwrap();
    assert_eq!(q3.materials.len(), 2);
    assert_eq!(q3.company_filings.len(), 2);
    assert_eq!(
        q3.materials[1].published_at.as_deref(),
        Some("2026-07-30T20:30:28Z")
    );
    assert!(
        q3.release_timestamp.is_none(),
        "filing acceptance is not earnings release time"
    );
    assert!(
        q3.materials
            .iter()
            .all(|material| material.verification == "metadata_match_body_not_verified")
    );
    assert_eq!(detail(&table, 1).unwrap().materials.len(), 1);
}

#[tokio::test]
async fn metric_navigation_carries_exact_period_and_does_not_publish_cursor_as_business_data() {
    use tick_market::{
        commands::earnings::EarningsView,
        research::ResearchTarget,
        runtime::{LiveView, ViewState},
    };
    let table = assemble(&inputs(
        vec![
            period("Q3 2026", 2026, "3", metric(None, Some(202))),
            period("Q2 2026", 2026, "2", metric(None, Some(201))),
        ],
        Vec::new(),
    ));
    let mut view = EarningsView::new(request_of("AAPL"), table);
    let original = view.content();
    let mut state = ViewState::default();
    view.on_key(KeyCode::Enter, &Fundamentals::new(today()), &mut state)
        .await
        .unwrap();
    view.on_key(KeyCode::Enter, &Fundamentals::new(today()), &mut state)
        .await
        .unwrap();
    let navigation = view.take_navigation().unwrap();
    assert_eq!(navigation.target, ResearchTarget::Reader);
    assert_eq!(navigation.context.period_id, "FY2026-Q3");
    assert_eq!(
        navigation.context.evidence.unwrap().metric_key.as_deref(),
        Some("eps")
    );
    view.on_key(KeyCode::Char('['), &Fundamentals::new(today()), &mut state)
        .await
        .unwrap();
    view.on_key(KeyCode::Char('a'), &Fundamentals::new(today()), &mut state)
        .await
        .unwrap();
    assert_eq!(
        view.take_navigation().unwrap().context.period_id,
        "FY2026-Q2"
    );
    assert_eq!(view.content(), original);
}

#[test]
fn upcoming_period_requires_explicit_calendar_identity_and_monitoring_accelerates_near_release() {
    use tick_market::commands::earnings::monitoring_interval;
    let mut pending = period("Q4 2026", 2026, "4", metric(Some(220), None));
    pending.is_released = false;
    let mut data = inputs(vec![pending], Vec::new());
    let mut release = event(date("2026-10-29"), ReleaseSession::AfterHours, 2026, "4");
    release.fiscal_period = None;
    data.next_report = Some(release);
    let unknown = assemble(&data);
    assert_eq!(unknown.upcoming.as_ref().unwrap().period_id, "FY2026-Q4");
    assert!(unknown.upcoming.as_ref().unwrap().release_date.is_none());
    assert!(unknown.rows.is_empty());
    data.next_report.as_mut().unwrap().fiscal_period = Some("4".into());
    let known = assemble(&data);
    assert_eq!(
        known.upcoming.as_ref().unwrap().release_date.as_deref(),
        Some("2026-10-29")
    );
    assert_eq!(
        monitoring_interval(&known, date("2026-10-29")).as_secs(),
        60
    );
    assert_eq!(
        monitoring_interval(&known, date("2026-09-24")).as_secs(),
        900
    );
}

#[tokio::test]
async fn an_upcoming_event_becomes_its_released_detail_without_changing_period() {
    use tick_market::{
        commands::earnings::EarningsView,
        research::ResearchStatus,
        runtime::{LiveView, ViewState},
    };
    let api = Fundamentals::new(today());
    let (year, quarter) = api.quarters[0];
    let mut pending = period(
        "Awaiting actuals",
        year,
        &quarter.to_string(),
        metric(Some(30), None),
    );
    pending.is_released = false;
    let mut view = EarningsView::new(
        request_of("TSLA"),
        assemble(&inputs(vec![pending], Vec::new())),
    );
    let mut state = ViewState::default();
    view.on_key(KeyCode::Char('u'), &api, &mut state)
        .await
        .unwrap();
    let before = view.research_context().unwrap();
    assert_eq!(before.status, ResearchStatus::Upcoming);
    view.refresh(&api).await.unwrap();
    let after = view.research_context().unwrap();
    assert_eq!(after.period_id, before.period_id);
    assert_eq!(after.status, ResearchStatus::Released);
    assert_eq!(view.selected_detail().unwrap().period_id, before.period_id);
}

#[tokio::test]
async fn a_statement_metric_navigation_retains_value_source_and_lookup_terms() {
    use tick_market::{
        commands::earnings::EarningsView,
        earnings::review::StatementItem,
        runtime::{LiveView, ViewState},
    };
    let mut table = assemble(&inputs(
        vec![period("Q3 2026", 2026, "3", metric(None, Some(202)))],
        Vec::new(),
    ));
    table.rows[0].statement_items.push(StatementItem {
        statement: "income".into(),
        field: "net_income".into(),
        name: "Net income (USD)".into(),
        currency: "USD".into(),
        percent: false,
        value: Some(Decimal::from(100)),
        reported_yoy_ratio: None,
    });
    let mut view = EarningsView::new(request_of("TSLA"), table);
    let mut state = ViewState::default();
    view.on_key(KeyCode::Enter, &Fundamentals::new(today()), &mut state)
        .await
        .unwrap();
    state.selected = 2;
    view.on_key(KeyCode::Char('a'), &Fundamentals::new(today()), &mut state)
        .await
        .unwrap();
    let evidence = view.take_navigation().unwrap().context.evidence.unwrap();
    assert_eq!(evidence.metric_key.as_deref(), Some("income:net_income"));
    assert!(
        evidence.text.contains("100")
            && evidence.text.contains("USD")
            && evidence.text.contains("financial_report")
    );
    assert!(!evidence.search_terms.is_empty());
}

#[tokio::test]
async fn missing_upcoming_event_does_not_silently_follow_a_different_period() {
    use tick_market::{
        commands::earnings::EarningsView,
        runtime::{LiveView, ViewState},
    };
    let api = Fundamentals::new(today());
    let mut pending = period(
        "Upcoming event",
        today().year() + 1,
        "1",
        metric(Some(30), None),
    );
    pending.is_released = false;
    let mut view = EarningsView::new(
        request_of("TSLA"),
        assemble(&inputs(vec![pending], Vec::new())),
    );
    view.on_key(KeyCode::Char('u'), &api, &mut ViewState::default())
        .await
        .unwrap();
    let before = view.research_context();
    let content = view.content();
    assert!(view.refresh(&api).await.is_err());
    assert_eq!(view.research_context(), before);
    assert_eq!(view.content(), content);
}
