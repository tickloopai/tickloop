use std::sync::Mutex;

use async_trait::async_trait;
use crossterm::event::KeyCode;
use rust_decimal::Decimal;
use serde_json::json;
use tick_market::{
    commands::kline::KlineView,
    config::Settings,
    event_chart::{
        EventChartConfig, EventRange, EventSession, load_history, resolve_window,
        resolve_window_with_half_days,
    },
    runtime::{LiveView, ViewState},
};
use tick_provider::{
    api::{
        Candle, CandleAdjust, CandleHistoryRequest, CandlePeriod, CandleRequest, CandleSessions,
        DateRange, Quote, QuoteApi,
    },
    dates::parse_iso_date,
    error::{Error, Result},
};
use tick_sdk::tools::ToolCall;

fn day(value: &str) -> time::Date {
    parse_iso_date(value).unwrap()
}
fn days(values: &[&str]) -> Vec<time::Date> {
    values.iter().map(|value| day(value)).collect()
}
fn event(date: &str, session: EventSession, published_at: Option<&str>) -> EventChartConfig {
    EventChartConfig {
        date: date.into(),
        session,
        published_at: published_at.map(String::from),
        ..EventChartConfig::default()
    }
}

#[test]
fn pre_market_uses_same_session_and_after_hours_uses_provider_next_session() {
    let calendar = days(&["2026-07-01", "2026-07-02", "2026-07-06", "2026-07-07"]);
    let pre = resolve_window(
        "DEMO.US",
        &event(
            "2026-07-02",
            EventSession::PreMarket,
            Some("2026-07-02T11:00:00Z"),
        ),
        &calendar,
    )
    .unwrap();
    assert_eq!(pre.baseline_date.as_deref(), Some("2026-07-01"));
    assert_eq!(pre.first_regular_session.as_deref(), Some("2026-07-02"));
    assert!(pre.start.starts_with("2026-07-01T15:30:00-04:00"));
    let after = resolve_window(
        "DEMO.US",
        &event("2026-07-02", EventSession::AfterHours, None),
        &calendar,
    )
    .unwrap();
    assert_eq!(after.baseline_date.as_deref(), Some("2026-07-02"));
    assert_eq!(after.first_regular_session.as_deref(), Some("2026-07-06"));
    assert!(after.end.starts_with("2026-07-06T16:00:00-04:00"));
    assert!(after.published_at.is_none());
}

#[test]
fn event_window_crosses_dst_with_each_boundary_in_its_correct_offset() {
    let window = resolve_window(
        "DEMO.US",
        &event(
            "2026-03-06",
            EventSession::AfterHours,
            Some("2026-03-06T16:05:00-05:00"),
        ),
        &days(&["2026-03-05", "2026-03-06", "2026-03-09"]),
    )
    .unwrap();
    assert!(window.start.ends_with("-05:00"));
    assert!(window.end.ends_with("-04:00"));
    assert_eq!(window.first_regular_session.as_deref(), Some("2026-03-09"));
}

#[test]
fn unknown_publication_does_not_invent_t0_or_claim_a_first_session() {
    let calendar = days(&["2026-08-05", "2026-08-06", "2026-08-07"]);
    let config = event("2026-08-06", EventSession::Unknown, None);
    let window = resolve_window("DEMO.US", &config, &calendar).unwrap();
    assert!(window.published_at.is_none());
    assert!(window.first_regular_session.is_none());
    assert!(window.baseline_date.is_none());
    let first = EventChartConfig {
        range: EventRange::FirstSession,
        ..config
    };
    assert!(
        resolve_window("DEMO.US", &first, &calendar)
            .unwrap_err()
            .message
            .contains("unknown")
    );
    assert!(resolve_window("DEMO.US", &first, &[]).is_err());
}

#[test]
fn actual_timestamp_is_checked_in_market_timezone() {
    let calendar = days(&["2026-08-05", "2026-08-06", "2026-08-07"]);
    let actual = event(
        "2026-08-06",
        EventSession::Unknown,
        Some("2026-08-06T20:05:00Z"),
    );
    assert_eq!(
        resolve_window("DEMO.US", &actual, &calendar)
            .unwrap()
            .release_session,
        EventSession::AfterHours
    );
    let invalid = event(
        "2026-08-06",
        EventSession::AfterHours,
        Some("2026-08-07T12:00:00Z"),
    );
    assert!(resolve_window("DEMO.US", &invalid, &calendar).is_err());
    let malformed = event("2026/08/06", EventSession::AfterHours, None);
    assert!(malformed.validate().is_err());
    let other_offset = event(
        "2026-08-06",
        EventSession::Unknown,
        Some("2026-08-07T05:00:00+10:00"),
    );
    let window = resolve_window("DEMO.US", &other_offset, &calendar).unwrap();
    assert_eq!(window.release_session, EventSession::DuringMarket);
    assert!(window.start.starts_with("2026-08-06T14:30:00-04:00"));
}

fn candles() -> Vec<Candle> {
    serde_json::from_value(json!([
        {"timestamp":"2026-08-06T15:30:00-04:00","open":"100","high":"101","low":"99","close":"100","volume":100,"turnover":"10000"},
        {"timestamp":"2026-08-06T16:05:00-04:00","open":"100","high":"100","low":"97","close":"98","volume":200,"turnover":"19600"},
        {"timestamp":"2026-08-06T17:00:00-04:00","open":"98","high":"99","low":"96","close":"97","volume":300,"turnover":"29100"},
        {"timestamp":"2026-08-07T09:30:00-04:00","open":"96","high":"98","low":"95","close":"97","volume":400,"turnover":"38800"},
        {"timestamp":"2026-08-07T15:59:00-04:00","open":"97","high":"98","low":"96","close":"97.5","volume":100,"turnover":"9750"}
    ])).unwrap()
}

#[derive(Default)]
struct History {
    calls: Mutex<Vec<(CandlePeriod, CandleSessions)>>,
    calendar_calls: Mutex<Vec<(&'static str, DateRange)>>,
    fail: Mutex<bool>,
}
#[async_trait]
impl QuoteApi for History {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn trading_days(&self, market: &str, range: DateRange) -> Result<Vec<time::Date>> {
        assert_eq!(market, "US");
        assert!((range.end - range.start).whole_days() < 28);
        self.calendar_calls.lock().unwrap().push(("full", range));
        Ok(days(&[
            "2026-07-30",
            "2026-07-31",
            "2026-08-03",
            "2026-08-04",
            "2026-08-05",
            "2026-08-06",
            "2026-08-07",
            "2026-08-10",
            "2026-08-11",
            "2026-08-12",
            "2026-08-13",
        ])
        .into_iter()
        .filter(|day| *day >= range.start && *day <= range.end)
        .collect())
    }
    async fn half_trading_days(&self, _: &str, range: DateRange) -> Result<Vec<time::Date>> {
        assert!((range.end - range.start).whole_days() < 28);
        self.calendar_calls.lock().unwrap().push(("half", range));
        Ok(Vec::new())
    }
    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        self.calls
            .lock()
            .unwrap()
            .push((request.period, CandleSessions::Regular));
        let mut candle = candles()[0].clone();
        candle.timestamp = request
            .range
            .start
            .with_hms(16, 0, 0)
            .unwrap()
            .assume_offset(time::UtcOffset::from_hms(-4, 0, 0).unwrap());
        candle.close = Decimal::from(100);
        Ok(vec![candle])
    }
    async fn candles_by_date_with_sessions(
        &self,
        request: &CandleHistoryRequest,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        self.calls.lock().unwrap().push((request.period, sessions));
        if *self.fail.lock().unwrap() {
            return Err(Error::network("history unavailable"));
        }
        Ok(candles())
    }
}

fn request() -> CandleRequest {
    CandleRequest {
        symbol: "DEMO.US".into(),
        period: CandlePeriod::Day,
        count: 1000,
        adjust: CandleAdjust::None,
    }
}
fn config() -> EventChartConfig {
    let mut config = event(
        "2026-08-06",
        EventSession::AfterHours,
        Some("2026-08-06T16:05:00-04:00"),
    );
    config.conference_at = Some("2026-08-06T17:00:00-04:00".into());
    config
}

#[tokio::test]
async fn intraday_load_requests_all_sessions_and_daily_baseline_separately() {
    let api = History::default();
    let data = load_history(&api, "DEMO.US", CandleAdjust::None, &config())
        .await
        .unwrap();
    assert_eq!(data.baseline, Some(Decimal::from(100)));
    assert_eq!(data.candles.len(), 5);
    assert_eq!(
        *api.calls.lock().unwrap(),
        vec![
            (CandlePeriod::OneMinute, CandleSessions::All),
            (CandlePeriod::Day, CandleSessions::Regular)
        ]
    );
}

#[tokio::test]
async fn calendars_are_requested_in_contiguous_ranges_within_provider_month_limit() {
    let api = History::default();
    let data = load_history(&api, "DEMO.US", CandleAdjust::None, &config())
        .await
        .unwrap();
    assert_eq!(
        data.window.first_regular_session.as_deref(),
        Some("2026-08-07")
    );
    let calls = api.calendar_calls.lock().unwrap();
    for kind in ["full", "half"] {
        let ranges: Vec<_> = calls
            .iter()
            .filter(|call| call.0 == kind)
            .map(|call| call.1)
            .collect();
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges.first().unwrap().start, day("2026-06-22"));
        assert_eq!(ranges.last().unwrap().end, day("2026-09-20"));
        assert!(
            ranges
                .windows(2)
                .all(|pair| pair[0].end.next_day() == Some(pair[1].start))
        );
    }
}

#[tokio::test]
async fn event_navigation_does_not_publish_business_changes_and_view_does_reload() {
    let api = History::default();
    let mut view = KlineView::with_event(request(), config()).unwrap();
    view.load_event(&api).await.unwrap();
    let before = view.content();
    let mut state = ViewState::default();
    for key in [
        KeyCode::Char('l'),
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Char('v'),
        KeyCode::Backspace,
    ] {
        view.on_key(key, &api, &mut state).await.unwrap();
    }
    let screen = drawn(&view, 120, 32);
    assert!(
        !screen.contains("Candle / event detail") && !screen.contains("Enter Select"),
        "Backspace closed the detail and the picker: {screen}"
    );
    assert_eq!(before, view.content());
    view.act(ToolCall {
        tool: "set_view".into(),
        args: json!({"view":"minute","range":"first_session"}),
    })
    .await
    .unwrap();
    assert_eq!(view.content()["status"], "loading");
    assert!(view.content()["candles"].as_array().unwrap().is_empty());
    view.load_event(&api).await.unwrap();
    assert_eq!(view.content()["period"], "5m");
    assert_eq!(view.content()["candles"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_new_event_does_not_mislabel_old_candles_and_can_retry() {
    let api = History::default();
    let mut view = KlineView::with_event(request(), config()).unwrap();
    view.load_event(&api).await.unwrap();
    view.act(ToolCall {
        tool: "set_event".into(),
        args: json!({"event":config()}),
    })
    .await
    .unwrap();
    *api.fail.lock().unwrap() = true;
    assert!(view.load_event(&api).await.is_err());
    assert_eq!(view.content()["status"], "unavailable");
    assert!(view.content()["candles"].as_array().unwrap().is_empty());
    *api.fail.lock().unwrap() = false;
    view.load_event(&api).await.unwrap();
    assert_eq!(view.content()["status"], "ready");
}

#[tokio::test]
async fn event_chart_renders_at_narrow_and_wide_sizes_and_the_editor_has_no_modes() {
    let api = History::default();
    let mut view = KlineView::with_event(request(), config()).unwrap();
    view.load_event(&api).await.unwrap();
    for (width, height) in [(32, 14), (64, 22), (120, 32)] {
        assert!(drawn(&view, width, height).contains("DEMO.US"));
    }
    let mut state = ViewState::default();
    view.on_key(KeyCode::Char('e'), &api, &mut state)
        .await
        .unwrap();
    assert!(view.captures_text());
    for key in [KeyCode::Esc, KeyCode::Backspace, KeyCode::Char('q')] {
        view.on_key(key, &api, &mut state).await.unwrap();
        assert!(view.captures_text(), "{key:?} keeps the editor open");
    }
    let editor = drawn(&view, 120, 32);
    assert!(editor.contains("Event settings"), "{editor}");
    assert!(editor.contains("Ctrl+C Cancel"), "{editor}");
    assert!(
        !editor.contains("NORMAL") && !editor.contains("INSERT"),
        "{editor}"
    );
    view.cancel_text();
    assert!(!view.captures_text());
}

struct HangingCalendar;
#[async_trait]
impl QuoteApi for HangingCalendar {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn trading_days(&self, _: &str, _: DateRange) -> Result<Vec<time::Date>> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn ctrl_c_quits_while_an_applied_event_edit_is_still_loading() {
    use crossterm::event::{Event, KeyEvent, KeyModifiers};

    let mut view = KlineView::with_event(request(), config()).unwrap();
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    for key in [
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        keys.send(Ok(Event::Key(key))).unwrap();
    }
    let (_market, market_events) = tokio::sync::mpsc::unbounded_channel();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
    let mut updates = tick_sdk::stream::TickStream::in_memory(
        tick_market::metadata::TickKind::Kline,
        "longbridge",
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tick_market::runtime::run(
            &mut view,
            &HangingCalendar,
            &Settings::default(),
            &mut terminal,
            tokio_stream::wrappers::UnboundedReceiverStream::new(input),
            market_events,
            &mut updates,
        ),
    )
    .await
    .expect("Enter submitted the edit, so Ctrl+C quits its wait")
    .unwrap();
    drop(keys);
}

fn drawn(view: &KlineView, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| view.draw(frame, &mut ViewState::default(), &Settings::default()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn half_day_publication_uses_early_close_and_skips_holiday_from_provider_calendar() {
    let calendar = days(&["2026-11-25", "2026-11-27", "2026-11-30"]);
    let config = event(
        "2026-11-27",
        EventSession::Unknown,
        Some("2026-11-27T14:05:00-05:00"),
    );
    let window =
        resolve_window_with_half_days("DEMO.US", &config, &calendar, &days(&["2026-11-27"]))
            .unwrap();
    assert_eq!(window.release_session, EventSession::AfterHours);
    assert!(window.start.starts_with("2026-11-27T12:30:00"));
    assert_eq!(window.first_regular_session.as_deref(), Some("2026-11-30"));
}

#[test]
fn hk_closing_auction_publication_is_not_assumed_to_be_after_close() {
    let calendar = days(&["2026-12-23", "2026-12-24", "2026-12-28"]);
    let config = event(
        "2026-12-24",
        EventSession::Unknown,
        Some("2026-12-24T12:05:00+08:00"),
    );
    let window =
        resolve_window_with_half_days("700.HK", &config, &calendar, &days(&["2026-12-24"]))
            .unwrap();
    assert_eq!(window.release_session, EventSession::Unknown);
    assert!(window.baseline_date.is_none());
    assert!(window.end.ends_with("16:10:00+08:00"));
}

#[tokio::test]
async fn demo_event_history_has_real_session_boundaries_and_explicit_demo_source() {
    use time_tz::OffsetDateTimeExt;
    let api = tick_builtin::DemoApi::new().unwrap();
    let config = event(
        "2026-08-07",
        EventSession::AfterHours,
        Some("2026-08-07T16:05:00-04:00"),
    );
    let data = load_history(&api, "DEMO.US", CandleAdjust::None, &config)
        .await
        .unwrap();
    assert_eq!(data.source, "demo");
    assert_eq!(
        data.window.first_regular_session.as_deref(),
        Some("2026-08-10")
    );
    assert_eq!(data.candles.len(), 990);
    let local: Vec<_> = data
        .candles
        .iter()
        .map(|candle| {
            candle
                .timestamp
                .to_timezone(time_tz::timezones::db::america::NEW_YORK)
        })
        .collect();
    assert!(local.iter().all(|at| !matches!(
        at.weekday(),
        time::Weekday::Saturday | time::Weekday::Sunday
    )));
    assert!(local.iter().any(|at| at.hour() == 5));
    assert!(local.iter().any(|at| at.hour() == 18));
    assert!(
        local
            .windows(2)
            .any(|pair| (pair[1] - pair[0]).whole_hours() > 24)
    );
    assert_eq!(
        local.last().unwrap().time(),
        time::Time::from_hms(15, 59, 0).unwrap()
    );
}

#[tokio::test]
async fn large_event_windows_publish_bounded_pages_and_expose_the_next_offset() {
    let api = tick_builtin::DemoApi::new().unwrap();
    let mut config = config();
    config.range = EventRange::FiveSessions;
    let data = load_history(&api, "DEMO.US", CandleAdjust::None, &config)
        .await
        .unwrap();
    assert!(data.candles.len() > 1000);
    let first =
        tick_market::event_chart::history_content("DEMO.US", CandleAdjust::None, &config, &data);
    assert_eq!(first["returned_candles"], 1000);
    assert_eq!(first["next_offset"], 1000);
    assert_eq!(first["truncated"], true);
    assert!(serde_json::to_vec(&first).unwrap().len() < 512 * 1024);
    let second = tick_market::event_chart::history_page(
        "DEMO.US",
        CandleAdjust::None,
        &config,
        &data,
        1000,
        1000,
    );
    assert_eq!(second["offset"], 1000);
    assert_eq!(second["next_offset"], 2000);
    assert_ne!(
        first["candles"][999]["timestamp"],
        second["candles"][0]["timestamp"]
    );
}

struct CappedHistory {
    demo: tick_builtin::DemoApi,
    offsets: Mutex<Vec<time::PrimitiveDateTime>>,
    repeats_page: bool,
}

#[async_trait]
impl QuoteApi for CappedHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn trading_days(&self, market: &str, range: DateRange) -> Result<Vec<time::Date>> {
        self.demo.trading_days(market, range).await
    }
    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        self.demo.candles_by_date(request).await
    }
    async fn candles_by_date_with_sessions(
        &self,
        request: &CandleHistoryRequest,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        let mut candles = self
            .demo
            .candles_by_date_with_sessions(request, sessions)
            .await?;
        candles.drain(..candles.len().saturating_sub(1_000));
        Ok(candles)
    }
    async fn candles_before(
        &self,
        request: &CandleHistoryRequest,
        before: time::PrimitiveDateTime,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        use time_tz::OffsetDateTimeExt;
        assert_eq!(sessions, CandleSessions::All);
        self.offsets.lock().unwrap().push(before);
        let mut candles = self
            .demo
            .candles_by_date_with_sessions(request, sessions)
            .await?;
        if !self.repeats_page {
            candles.retain(|candle| {
                let at = candle
                    .timestamp
                    .to_timezone(time_tz::timezones::db::america::NEW_YORK);
                time::PrimitiveDateTime::new(at.date(), at.time()) <= before
            });
        }
        candles.drain(..candles.len().saturating_sub(1_000));
        Ok(candles)
    }
}

#[tokio::test]
async fn capped_history_pages_back_in_exchange_time_without_losing_pre_event_bars() {
    let api = CappedHistory {
        demo: tick_builtin::DemoApi::new().unwrap(),
        offsets: Mutex::default(),
        repeats_page: false,
    };
    for range in [EventRange::Auto, EventRange::FiveSessions] {
        let mut config = config();
        config.range = range;
        let expected = load_history(&api.demo, "DEMO.US", CandleAdjust::None, &config)
            .await
            .unwrap();
        let actual = load_history(&api, "DEMO.US", CandleAdjust::None, &config)
            .await
            .unwrap();
        assert_eq!(actual.candles.len(), expected.candles.len());
        assert!(
            actual
                .candles
                .iter()
                .zip(expected.candles)
                .all(|(actual, expected)| actual.timestamp == expected.timestamp)
        );
    }
    let offsets = api.offsets.lock().unwrap();
    assert!(offsets.len() > 2);
    // Two 960-minute sessions: the last 1000 bars begin at 19:20 in New York, not UTC.
    assert_eq!(offsets[0], day("2026-08-06").with_hms(19, 20, 0).unwrap());
}

#[tokio::test]
async fn repeated_provider_page_is_an_error_instead_of_a_partial_success() {
    let api = CappedHistory {
        demo: tick_builtin::DemoApi::new().unwrap(),
        offsets: Mutex::default(),
        repeats_page: true,
    };
    let error = load_history(&api, "DEMO.US", CandleAdjust::None, &config())
        .await
        .unwrap_err();
    assert!(error.message.contains("did not advance"));
}

#[tokio::test]
async fn compressed_event_plot_identifies_volume_sums_and_mixed_sessions() {
    let api = tick_builtin::DemoApi::new().unwrap();
    let mut config = config();
    config.range = EventRange::FiveSessions;
    let mut view = KlineView::with_event(request(), config).unwrap();
    view.load_event(&api).await.unwrap();
    for (width, height) in [(32, 14), (120, 32)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| view.draw(frame, &mut ViewState::default(), &Settings::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("10560 bars /"));
        assert!(rendered.contains("V sum/col"));
        let sessions = (1..width - 11)
            .map(|x| buffer[(x, height - 6)].symbol())
            .collect::<String>();
        assert!(sessions.contains('*'));
        if width == 120 {
            assert!(sessions.contains('P'));
            assert!(sessions.contains('R'));
            assert!(sessions.contains('A'));
            assert!(sessions.contains(' '));
        }
        let volume = (1..width - 11)
            .map(|x| buffer[(x, height - 7)].symbol())
            .collect::<String>();
        assert!(!volume.trim().is_empty());
    }
}

#[tokio::test]
async fn daily_event_marker_uses_known_date_without_inventing_publication_time() {
    let api = tick_builtin::DemoApi::new().unwrap();
    let mut config = config();
    config.published_at = None;
    config.view = tick_market::event_chart::EventChartMode::Daily;
    config.range = EventRange::FiveSessions;
    let mut view = KlineView::with_event(request(), config).unwrap();
    view.load_event(&api).await.unwrap();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|frame| view.draw(frame, &mut ViewState::default(), &Settings::default()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("EVENT"));
    assert!(rendered.contains("regular C"));
    let buffer = terminal.backend().buffer();
    let plot = (6..25)
        .flat_map(|y| (1..109).map(move |x| buffer[(x, y)].symbol()))
        .collect::<String>();
    assert!(!plot.contains("T0"));
    assert!(view.content()["window"]["published_at"].is_null());
}

#[tokio::test]
async fn research_context_switches_symbol_period_and_event_without_reusing_old_data() {
    use tick_market::research::ResearchContext;
    let api = History::default();
    let mut view = KlineView::with_event(request(), config()).unwrap();
    let first = ResearchContext {
        symbol: "DEMO.US".into(),
        period_id: "FY2026-Q2".into(),
        release_date: Some("2026-08-06".into()),
        release_session: EventSession::AfterHours,
        published_at: Some("2026-08-06T16:05:00-04:00".into()),
        ..Default::default()
    };
    view.act(ToolCall {
        tool: "set_context".into(),
        args: json!({"context":first}),
    })
    .await
    .unwrap();
    view.load_event(&api).await.unwrap();
    assert_eq!(view.content()["candles"].as_array().unwrap().len(), 5);
    let mut state = ViewState::default();
    view.on_key(KeyCode::Char('a'), &api, &mut state)
        .await
        .unwrap();
    let original_navigation = view.take_navigation().unwrap();
    assert_eq!(original_navigation.context.period_id, "FY2026-Q2");
    assert!(
        original_navigation
            .context
            .evidence
            .as_ref()
            .unwrap()
            .text
            .contains("2026-08-06")
    );
    let second = ResearchContext {
        symbol: "OTHER.US".into(),
        period_id: "FY2026-Q1".into(),
        release_date: Some("2026-07-31".into()),
        release_session: EventSession::AfterHours,
        ..Default::default()
    };
    view.act(ToolCall {
        tool: "set_context".into(),
        args: json!({"context":second}),
    })
    .await
    .unwrap();
    assert_eq!(view.content()["status"], "loading");
    assert!(view.content()["candles"].as_array().unwrap().is_empty());
    assert_eq!(view.content()["symbol"], "OTHER.US");
    view.load_event(&api).await.unwrap();
    assert!(view.content()["window"]["published_at"].is_null());
    assert_eq!(
        view.content()["window"]["first_regular_session"],
        "2026-08-03"
    );
    assert_eq!(original_navigation.context.period_id, "FY2026-Q2");
    let missing = ResearchContext {
        release_date: None,
        ..second
    };
    view.set_research_context(missing).unwrap();
    view.refresh(&api).await.unwrap();
    assert_eq!(view.content()["status"], "unavailable");
    assert!(view.content()["candles"].as_array().unwrap().is_empty());
}
