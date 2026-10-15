use std::cell::Cell;

use crossterm::event::KeyCode;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{Date, Duration, OffsetDateTime, Time, format_description::well_known::Rfc3339};
use time_tz::{OffsetDateTimeExt, PrimitiveDateTimeExt, Tz};

use crate::config::Settings;
use tick_provider::{
    api::{
        Candle, CandleAdjust, CandleHistoryRequest, CandlePeriod, CandleSessions, DateRange,
        MAX_HISTORY_PAGE_CANDLES, QuoteServices,
    },
    dates::{iso_date, parse_iso_date},
    error::{Error, Result},
};

const CALENDAR_LOOKAROUND_DAYS: i64 = 45;
/// The release, window and baseline lines and the key rows stay whole from this size; the
/// plain candlestick layout of the Kline Tick needs less.
pub const MINIMUM_PANEL_SIZE: crate::metadata::MinimumPanelSize =
    crate::metadata::MinimumPanelSize {
        columns: 60,
        rows: 14,
    };
const MAX_EVENT_CANDLES: usize = 12_000;
pub const MAX_EVENT_PAGE_CANDLES: usize = 1_000;
/// The query Tick Tool that pages through an event window's candles.
pub const EVENT_HISTORY_TOOL: &str = "event_history";
type PriceVolumeBin = (f64, f64, f64, f64, i64);

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum EventSession {
    PreMarket,
    AfterHours,
    DuringMarket,
    Closed,
    #[default]
    Unknown,
}

impl EventSession {
    pub fn label(self) -> &'static str {
        match self {
            Self::PreMarket => "pre-market",
            Self::AfterHours => "after-hours",
            Self::DuringMarket => "during market",
            Self::Closed => "closed market",
            Self::Unknown => "time unknown",
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum EventChartMode {
    #[default]
    Line,
    Minute,
    Daily,
}

impl EventChartMode {
    pub const ALL: [Self; 3] = [Self::Line, Self::Minute, Self::Daily];

    pub fn label(self) -> &'static str {
        match self {
            Self::Line => "Event line / 1m",
            Self::Minute => "Candles / 5m",
            Self::Daily => "Daily candles",
        }
    }

    pub fn period(self) -> CandlePeriod {
        match self {
            Self::Line => CandlePeriod::OneMinute,
            Self::Minute => CandlePeriod::FiveMinute,
            Self::Daily => CandlePeriod::Day,
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum EventRange {
    #[default]
    Auto,
    FirstSession,
    FiveSessions,
}

impl EventRange {
    pub const ALL: [Self; 3] = [Self::Auto, Self::FirstSession, Self::FiveSessions];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto event window",
            Self::FirstSession => "First regular session",
            Self::FiveSessions => "5 sessions either side",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventChartConfig {
    /// Publication date in the exchange's local timezone.
    pub date: String,
    #[serde(default)]
    pub session: EventSession,
    /// Verified publication instant, with an explicit RFC 3339 UTC offset. Omit if unknown.
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub conference_at: Option<String>,
    #[serde(default)]
    pub view: EventChartMode,
    #[serde(default)]
    pub range: EventRange,
}

impl EventChartConfig {
    pub fn validate(&self) -> Result<()> {
        if self.date.len() != 10
            || self.date.as_bytes().get(4) != Some(&b'-')
            || self.date.as_bytes().get(7) != Some(&b'-')
            || parse_iso_date(&self.date).is_none()
        {
            return Err(Error::usage("event date must be YYYY-MM-DD"));
        }
        for (name, value) in [
            ("published_at", &self.published_at),
            ("conference_at", &self.conference_at),
        ] {
            if let Some(value) = value {
                parse_instant(name, value)?;
            }
        }
        Ok(())
    }
}

impl crate::args::EventChartArgs {
    pub fn config(&self) -> Result<Option<EventChartConfig>> {
        let Some(date) = &self.event_date else {
            return Ok(None);
        };
        let config = EventChartConfig {
            date: date.clone(),
            session: self.event_session,
            published_at: self.published_at.clone(),
            conference_at: self.conference_at.clone(),
            view: self.chart_mode,
            range: self.event_range,
        };
        config.validate()?;
        Ok(Some(config))
    }
}

fn parse_instant(field: &str, value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        Error::usage(format!(
            "{field} must be an RFC 3339 timestamp with a UTC offset"
        ))
    })
}

#[derive(Clone, Copy)]
struct MarketClock {
    code: &'static str,
    name: &'static str,
    zone: &'static Tz,
}

impl MarketClock {
    fn for_symbol(symbol: &str) -> Result<Self> {
        match symbol.rsplit('.').next() {
            Some("US") => Ok(Self {
                code: "US",
                name: "America/New_York",
                zone: time_tz::timezones::db::america::NEW_YORK,
            }),
            Some("HK") => Ok(Self {
                code: "HK",
                name: "Asia/Hong_Kong",
                zone: time_tz::timezones::db::asia::HONG_KONG,
            }),
            _ => Err(Error::usage(
                "event charts currently support US and HK symbols",
            )),
        }
    }

    fn at(self, day: Date, hour: u8, minute: u8) -> Result<OffsetDateTime> {
        day.with_time(
            Time::from_hms(hour, minute, 0).map_err(|error| Error::usage(error.to_string()))?,
        )
        .assume_timezone(self.zone)
        .take()
        .ok_or_else(|| Error::usage("event boundary is ambiguous in the market timezone"))
    }

    fn local(self, instant: OffsetDateTime) -> OffsetDateTime {
        instant.to_timezone(self.zone)
    }

    fn close(self, day: Date, half_day: bool) -> Result<OffsetDateTime> {
        // NYSE half days close at 13:00. HKEX's closing auction ends randomly by
        // 16:10 / 12:10; using its upper bound never invents an exact auction close.
        let hour = if half_day {
            if self.code == "US" { 13 } else { 12 }
        } else {
            16
        };
        self.at(day, hour, if self.code == "HK" { 10 } else { 0 })
    }

    fn session(self, instant: OffsetDateTime, half_day: bool) -> &'static str {
        let local = self.local(instant);
        let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());
        if self.code == "US" && !(240..1200).contains(&minute) {
            return "overnight";
        }
        let continuous_close = if half_day {
            if self.code == "US" { 780 } else { 720 }
        } else {
            960
        };
        if minute < 570 {
            "pre"
        } else if self.code == "HK" && (continuous_close..continuous_close + 10).contains(&minute) {
            "auction"
        } else if minute >= continuous_close {
            "post"
        } else if self.code == "HK" && (720..780).contains(&minute) {
            "break"
        } else {
            "regular"
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EventWindow {
    pub market_timezone: String,
    pub calendar_source: &'static str,
    pub start: String,
    pub end: String,
    pub baseline_date: Option<String>,
    pub half_trading_days: Vec<String>,
    pub first_regular_session: Option<String>,
    pub release_session: EventSession,
    pub published_at: Option<String>,
    pub conference_at: Option<String>,
    pub session_boundary_note: &'static str,
    #[serde(skip)]
    start_instant: Option<OffsetDateTime>,
    #[serde(skip)]
    end_instant: Option<OffsetDateTime>,
    #[serde(skip)]
    first_date: Option<Date>,
    #[serde(skip)]
    last_date: Option<Date>,
}

fn format_instant(instant: OffsetDateTime) -> String {
    instant.format(&Rfc3339).unwrap_or_default()
}

pub fn resolve_window(
    symbol: &str,
    config: &EventChartConfig,
    trading_days: &[Date],
) -> Result<EventWindow> {
    resolve_window_with_half_days(symbol, config, trading_days, &[])
}

pub fn resolve_window_with_half_days(
    symbol: &str,
    config: &EventChartConfig,
    trading_days: &[Date],
    half_trading_days: &[Date],
) -> Result<EventWindow> {
    config.validate()?;
    let market = MarketClock::for_symbol(symbol)?;
    let date = parse_iso_date(&config.date).ok_or_else(|| Error::usage("invalid event date"))?;
    let mut days = trading_days.to_vec();
    days.sort_unstable();
    days.dedup();
    let previous = days
        .iter()
        .copied()
        .rfind(|day| *day < date)
        .ok_or_else(|| Error::network("trading calendar has no session before this event"))?;
    let next = days
        .iter()
        .copied()
        .find(|day| *day > date)
        .ok_or_else(|| Error::network("trading calendar has no session after this event"))?;
    let is_trading_day = days.binary_search(&date).is_ok();
    let half_day = |day: Date| half_trading_days.contains(&day);
    let close = |day: Date| market.close(day, half_day(day));
    let published = config
        .published_at
        .as_deref()
        .map(|value| parse_instant("published_at", value))
        .transpose()?
        .map(|at| market.local(at));
    let session = if let Some(published) = published {
        let local = market.local(published);
        if local.date() != date {
            return Err(Error::usage(
                "published_at must fall on event date in the market timezone",
            ));
        }
        if !is_trading_day {
            EventSession::Closed
        } else if local.time() < Time::from_hms(9, 30, 0).unwrap() {
            EventSession::PreMarket
        } else if market.code == "HK" && market.session(published, half_day(date)) == "auction" {
            config.session
        } else if local >= close(date)? {
            EventSession::AfterHours
        } else {
            EventSession::DuringMarket
        }
    } else if !is_trading_day {
        EventSession::Closed
    } else {
        config.session
    };
    let baseline = if session == EventSession::AfterHours && is_trading_day {
        date
    } else {
        previous
    };
    let first_session = match session {
        EventSession::PreMarket | EventSession::DuringMarket if is_trading_day => Some(date),
        EventSession::AfterHours | EventSession::Closed => Some(next),
        _ => None,
    };
    let (first_date, last_date, start, end) = match config.range {
        EventRange::Auto => {
            let last = first_session.unwrap_or(next);
            let start = if session == EventSession::DuringMarket {
                published
                    .map(|at| at - Duration::minutes(30))
                    .unwrap_or(market.at(date, 9, 30)?)
            } else {
                close(baseline)? - Duration::minutes(30)
            };
            (start.date(), last, start, close(last)?)
        }
        EventRange::FirstSession => {
            let first = first_session.ok_or_else(|| {
                Error::usage(
                    "the first session is unknown; specify the release session or published_at",
                )
            })?;
            (first, first, market.at(first, 9, 30)?, close(first)?)
        }
        EventRange::FiveSessions => {
            let before: Vec<_> = days.iter().copied().filter(|day| *day < date).collect();
            let after: Vec<_> = days.iter().copied().filter(|day| *day > date).collect();
            if before.len() < 5 || after.len() < 5 {
                return Err(Error::network(
                    "trading calendar does not cover five sessions on each side",
                ));
            }
            let first = before[before.len() - 5];
            let last = after[4];
            (
                first,
                last,
                market.at(first, 4, 0)?,
                market.at(last, 20, 0)?,
            )
        }
    };
    Ok(EventWindow {
        market_timezone: market.name.into(),
        calendar_source: "provider_trading_days",
        start: format_instant(start),
        end: format_instant(end),
        baseline_date: (session != EventSession::Unknown).then(|| iso_date(baseline)),
        half_trading_days: half_trading_days.iter().copied().map(iso_date).collect(),
        first_regular_session: first_session.map(iso_date),
        release_session: session,
        published_at: published.map(|at| format_instant(market.local(at))),
        conference_at: config
            .conference_at
            .as_deref()
            .map(|value| {
                parse_instant("conference_at", value).map(|at| format_instant(market.local(at)))
            })
            .transpose()?,
        session_boundary_note: "Provider calendar includes half days. HK windows include the latest auction close; exact auction close is not inferred. Missing quotes are never filled.",
        start_instant: Some(start),
        end_instant: Some(end),
        first_date: Some(first_date),
        last_date: Some(last_date),
    })
}

#[derive(Clone, Debug)]
pub struct EventData {
    pub source: &'static str,
    pub window: EventWindow,
    pub candles: Vec<Candle>,
    pub baseline: Option<Decimal>,
}

pub async fn load_history(
    api: &QuoteServices,
    symbol: &str,
    adjust: CandleAdjust,
    config: &EventChartConfig,
) -> Result<EventData> {
    config.validate()?;
    let clock = MarketClock::for_symbol(symbol)?;
    let date = parse_iso_date(&config.date).ok_or_else(|| Error::usage("invalid event date"))?;
    let calendar_range = DateRange {
        start: date
            .checked_sub(Duration::days(CALENDAR_LOOKAROUND_DAYS))
            .ok_or_else(|| Error::usage("event date out of range"))?,
        end: date
            .checked_add(Duration::days(CALENDAR_LOOKAROUND_DAYS))
            .ok_or_else(|| Error::usage("event date out of range"))?,
    };
    let mut calendar = Vec::new();
    let mut half_days = Vec::new();
    for range in crate::earnings::trading_day_windows(&[calendar_range]) {
        calendar.extend(api.trading_days(clock.code, range).await?);
        half_days.extend(api.half_trading_days(clock.code, range).await?);
    }
    calendar.sort_unstable();
    calendar.dedup();
    half_days.sort_unstable();
    half_days.dedup();
    let window = resolve_window_with_half_days(symbol, config, &calendar, &half_days)?;
    let range = DateRange {
        start: window.first_date.unwrap(),
        end: window.last_date.unwrap(),
    };
    let history_request = CandleHistoryRequest {
        symbol: symbol.into(),
        period: config.view.period(),
        adjust,
        range,
    };
    let mut candles = if config.view == EventChartMode::Daily {
        api.candles_by_date(&history_request).await?
    } else {
        api.candles_by_date_with_sessions(&history_request, CandleSessions::All)
            .await?
    };
    if config.view != EventChartMode::Daily {
        let start = window.start_instant.unwrap();
        let mut page_size = candles.len();
        let mut earliest = candles.iter().map(|candle| candle.timestamp).min();
        while page_size == MAX_HISTORY_PAGE_CANDLES && earliest.is_some_and(|at| at > start) {
            let before = earliest.unwrap();
            let local = clock.local(before);
            let page = api
                .candles_before(
                    &history_request,
                    time::PrimitiveDateTime::new(local.date(), local.time()),
                    CandleSessions::All,
                )
                .await?;
            page_size = page.len();
            let next = page.iter().map(|candle| candle.timestamp).min();
            if next.is_some_and(|next| next >= before) {
                return Err(Error::network(
                    "historical candlestick pagination did not advance",
                ));
            }
            candles.extend(page.into_iter().filter(|candle| candle.timestamp < before));
            earliest = next;
            if candles.len() > MAX_EVENT_CANDLES + MAX_HISTORY_PAGE_CANDLES {
                return Err(Error::usage(format!(
                    "event history exceeds {MAX_EVENT_CANDLES} candles; select minute or daily view"
                )));
            }
        }
    }
    candles.sort_by_key(|candle| candle.timestamp);
    candles.dedup_by_key(|candle| candle.timestamp);
    if config.view != EventChartMode::Daily {
        candles.retain(|candle| {
            candle.timestamp >= window.start_instant.unwrap()
                && candle.timestamp < window.end_instant.unwrap()
        });
    }
    if candles.len() > MAX_EVENT_CANDLES {
        return Err(Error::usage(format!(
            "event history exceeds {MAX_EVENT_CANDLES} candles; select minute or daily view"
        )));
    }
    let baseline =
        if let Some(baseline_date) = window.baseline_date.as_deref().and_then(parse_iso_date) {
            let baseline_request = CandleHistoryRequest {
                symbol: symbol.into(),
                period: CandlePeriod::Day,
                adjust,
                range: DateRange {
                    start: baseline_date,
                    end: baseline_date,
                },
            };
            api.candles_by_date(&baseline_request)
                .await?
                .into_iter()
                .find(|candle| {
                    clock.local(candle.timestamp).date() == baseline_date
                        || candle.timestamp.date() == baseline_date
                })
                .map(|candle| candle.close)
                .filter(|close| *close > Decimal::ZERO)
        } else {
            None
        };
    Ok(EventData {
        source: api.source_name(),
        window,
        candles,
        baseline,
    })
}

pub fn history_content(
    symbol: &str,
    adjust: CandleAdjust,
    config: &EventChartConfig,
    data: &EventData,
) -> Value {
    history_page(symbol, adjust, config, data, 0, MAX_EVENT_PAGE_CANDLES)
}

fn baseline_definition(window: &EventWindow) -> &'static str {
    if window.baseline_date.is_some() {
        "last regular-session close before publication"
    } else {
        "unavailable: release session is unknown"
    }
}

fn data_status(data: &EventData) -> &'static str {
    if data.candles.is_empty() {
        "no_data"
    } else {
        "ready"
    }
}

/// The event chart's Tick state: the event, its window and baseline, and a summary of the candles,
/// which [`EVENT_HISTORY_TOOL`] pages through.
pub fn summary_content(
    symbol: &str,
    adjust: CandleAdjust,
    config: &EventChartConfig,
    data: &EventData,
) -> Value {
    crate::commands::kline::with_candle_summary(
        json!({
            "symbol": symbol, "adjust": adjust, "event": config, "window": data.window,
            "source": data.source, "period": config.view.period(), "baseline_close": data.baseline,
            "baseline_definition": baseline_definition(&data.window), "status": data_status(data),
        }),
        &data.candles,
        EVENT_HISTORY_TOOL,
    )
}

pub fn history_page(
    symbol: &str,
    adjust: CandleAdjust,
    config: &EventChartConfig,
    data: &EventData,
    offset: usize,
    limit: usize,
) -> Value {
    let clock = MarketClock::for_symbol(symbol).ok();
    let offset = offset.min(data.candles.len());
    let end = offset
        .saturating_add(limit.min(MAX_EVENT_PAGE_CANDLES))
        .min(data.candles.len());
    let candles = &data.candles[offset..end];
    json!({
        "symbol": symbol, "adjust": adjust, "event": config, "window": data.window,
        "source": data.source,
        "period": config.view.period(), "baseline_close": data.baseline,
        "baseline_definition": baseline_definition(&data.window),
        "status": data_status(data),
        "candles": crate::commands::kline::table(candles).json(),
        "total_candles": data.candles.len(), "offset": offset, "returned_candles": candles.len(),
        "next_offset": (end < data.candles.len()).then_some(end),
        "truncated": offset > 0 || end < data.candles.len(), "history_tool": EVENT_HISTORY_TOOL,
        "sessions": candles.iter().map(|candle| if config.view == EventChartMode::Daily { "regular" } else { clock.map(|clock| clock.session(candle.timestamp, data.window.half_trading_days.contains(&iso_date(clock.local(candle.timestamp).date())))).unwrap_or("unknown") }).collect::<Vec<_>>()
    })
}

#[derive(Clone, Copy)]
enum Picker {
    View,
    Range,
}

pub struct EventChart {
    pub config: EventChartConfig,
    pub data: Option<EventData>,
    pub pending: bool,
    pub last_error: Option<String>,
    cursor: usize,
    details: bool,
    picker: Option<(Picker, usize)>,
    columns: Cell<usize>,
}

impl EventChart {
    pub fn selected_observation(&self) -> Value {
        json!({"event":self.config,"source":self.data.as_ref().map(|data|data.source),"window":self.data.as_ref().map(|data|&data.window),"baseline_close":self.data.as_ref().and_then(|data|data.baseline),"candle":self.data.as_ref().and_then(|data|data.candles.get(self.cursor)),"error":self.last_error})
    }
    pub fn has_overlay(&self) -> bool {
        self.picker.is_some() || self.details
    }
    pub fn new(config: EventChartConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            data: None,
            pending: true,
            last_error: None,
            cursor: 0,
            details: false,
            picker: None,
            columns: Cell::new(1),
        })
    }

    pub async fn reload(
        &mut self,
        api: &QuoteServices,
        symbol: &str,
        adjust: CandleAdjust,
    ) -> Result<()> {
        self.pending = false;
        match load_history(api, symbol, adjust, &self.config).await {
            Ok(data) => {
                let selected = self
                    .data
                    .as_ref()
                    .and_then(|data| data.candles.get(self.cursor))
                    .map(|candle| candle.timestamp);
                self.cursor = selected
                    .and_then(|selected| {
                        data.candles
                            .iter()
                            .position(|candle| candle.timestamp == selected)
                    })
                    .unwrap_or_else(|| {
                        self.config
                            .published_at
                            .as_deref()
                            .and_then(|value| parse_instant("published_at", value).ok())
                            .and_then(|at| {
                                data.candles
                                    .iter()
                                    .position(|candle| candle.timestamp >= at)
                            })
                            .unwrap_or(0)
                    });
                self.data = Some(data);
                self.last_error = None;
                Ok(())
            }
            Err(error) => {
                self.last_error = Some(error.message.clone());
                Err(error)
            }
        }
    }

    pub fn content(&self, symbol: &str, adjust: CandleAdjust) -> Value {
        match &self.data {
            Some(data) => {
                let mut content = summary_content(symbol, adjust, &self.config, data);
                if let Some(error) = &self.last_error {
                    content["status"] = json!("stale");
                    content["error"] = json!(error);
                }
                content
            }
            None => crate::commands::kline::with_candle_summary(
                json!({"symbol":symbol,"adjust":adjust,"event":self.config,"status":if self.pending {"loading"} else {"unavailable"},"error":self.last_error}),
                &[],
                EVENT_HISTORY_TOOL,
            ),
        }
    }

    pub fn set_view(&mut self, view: EventChartMode, range: EventRange) {
        if self.config.view == view && self.config.range == range {
            return;
        }
        self.config.view = view;
        self.config.range = range;
        self.data = None;
        self.pending = true;
        self.last_error = None;
        self.cursor = 0;
    }

    pub async fn on_key(
        &mut self,
        key: KeyCode,
        api: &QuoteServices,
        symbol: &str,
        adjust: CandleAdjust,
    ) -> Result<bool> {
        if let Some((picker, index)) = self.picker.as_mut() {
            match key {
                KeyCode::Backspace => self.picker = None,
                KeyCode::Up | KeyCode::Char('k') => *index = index.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *index = (*index + 1).min(2),
                KeyCode::Enter => {
                    let (picker, index) = (*picker, *index);
                    self.picker = None;
                    let (view, range) = match picker {
                        Picker::View => (
                            EventChartMode::ALL[index],
                            if EventChartMode::ALL[index] == EventChartMode::Daily {
                                EventRange::FiveSessions
                            } else {
                                self.config.range
                            },
                        ),
                        Picker::Range => (self.config.view, EventRange::ALL[index]),
                    };
                    self.set_view(view, range);
                    self.reload(api, symbol, adjust).await?;
                }
                _ => (),
            }
            return Ok(true);
        }
        if self.details {
            if matches!(key, KeyCode::Backspace | KeyCode::Enter) {
                self.details = false;
            }
            return Ok(true);
        }
        let count = self.data.as_ref().map_or(0, |data| data.candles.len());
        match key {
            KeyCode::Enter => self.details = !self.details,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(count.saturating_sub(1))
            }
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = count.saturating_sub(1),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(self.columns.get()),
            KeyCode::PageDown => {
                self.cursor = (self.cursor + self.columns.get()).min(count.saturating_sub(1))
            }
            KeyCode::Char('v') => {
                self.picker = Some((
                    Picker::View,
                    EventChartMode::ALL
                        .iter()
                        .position(|view| *view == self.config.view)
                        .unwrap_or(0),
                ))
            }
            KeyCode::Char('r') => {
                self.picker = Some((
                    Picker::Range,
                    EventRange::ALL
                        .iter()
                        .position(|range| *range == self.config.range)
                        .unwrap_or(0),
                ))
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let current = EventRange::ALL
                    .iter()
                    .position(|range| *range == self.config.range)
                    .unwrap_or(0);
                let next = if key == KeyCode::Char(']') {
                    (current + 1) % 3
                } else {
                    (current + 2) % 3
                };
                self.set_view(self.config.view, EventRange::ALL[next]);
                self.reload(api, symbol, adjust).await?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn draw(
        &self,
        frame: &mut ratatui::Frame<'_>,
        symbol: &str,
        settings: &Settings,
        marks: &[crate::commands::kline::PriceMark],
    ) {
        use tickloop_ui::format::write_text;
        let area = frame.area();
        let styles = settings.styles();
        frame.buffer_mut().set_style(area, styles.panel_base());
        if area.width < 32 || area.height < 14 {
            frame.render_widget(
                Paragraph::new("Event chart needs 32×14").style(styles.muted_text),
                area,
            );
            return;
        }
        let block = Block::default()
            .title(format!(
                " {symbol} · {} · {} ",
                self.config.date,
                self.config.view.label()
            ))
            .borders(Borders::ALL)
            .style(styles.panel_base())
            .border_style(styles.border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = usize::from(inner.width);
        let clock = MarketClock::for_symbol(symbol).ok();
        let text = |frame: &mut ratatui::Frame<'_>, row: u16, value: &str, style| {
            write_text(
                frame.buffer_mut(),
                inner.x,
                inner.y + row,
                value,
                width,
                settings.ambiguous_width,
                style,
            )
        };
        let data = self.data.as_ref();
        let publication = data
            .and_then(|data| data.window.published_at.as_deref())
            .or(self.config.published_at.as_deref())
            .unwrap_or("exact publication time unknown; no T=0 marker");
        text(
            frame,
            0,
            &format!(
                "{} bars / {} cols · {} · {}",
                data.map_or(0, |data| data.candles.len()),
                inner.width.saturating_sub(10),
                self.config.range.label(),
                data.map_or(self.config.session, |data| data.window.release_session)
                    .label()
            ),
            styles.text,
        );
        text(
            frame,
            1,
            &format!("Release: {publication}"),
            styles.muted_text,
        );
        if let Some(data) = data {
            text(
                frame,
                2,
                &format!("{} → {}", data.window.start, data.window.end),
                styles.muted_text,
            );
            text(
                frame,
                3,
                &format!(
                    "Baseline {} regular close {} · {}",
                    data.window.baseline_date.as_deref().unwrap_or("unknown"),
                    data.baseline
                        .map(|value| value.normalize().to_string())
                        .unwrap_or_else(|| "unavailable".into()),
                    data.window.market_timezone
                ),
                styles.muted_text,
            );
        }
        let selected = data.and_then(|data| data.candles.get(self.cursor));
        if let Some(candle) = selected {
            let at = clock.map_or(candle.timestamp, |clock| clock.local(candle.timestamp));
            let change = data
                .and_then(|data| data.baseline)
                .map(|baseline| (candle.close - baseline) / baseline * Decimal::ONE_HUNDRED);
            text(
                frame,
                4,
                &format!(
                    "{} {:02}:{:02} {} C {} ({}) V {}",
                    iso_date(at.date()),
                    at.hour(),
                    at.minute(),
                    if self.config.view == EventChartMode::Daily {
                        "regular"
                    } else {
                        clock.map_or("unknown", |clock| {
                            clock.session(
                                candle.timestamp,
                                data.is_some_and(|data| {
                                    data.window
                                        .half_trading_days
                                        .contains(&iso_date(clock.local(candle.timestamp).date()))
                                }),
                            )
                        })
                    },
                    candle.close.normalize(),
                    change
                        .map(|value| format!("{value:+.2}%"))
                        .unwrap_or_else(|| "--".into()),
                    candle.volume
                ),
                styles.text,
            );
        }
        let chart = ratatui::layout::Rect::new(
            inner.x,
            inner.y + 5,
            inner.width,
            inner.height.saturating_sub(8),
        );
        if let Some(data) = data.filter(|data| !data.candles.is_empty()) {
            self.draw_prices(frame, chart, data, settings, marks, clock);
        } else {
            let message = if self.pending {
                "Loading event history…"
            } else {
                self.last_error
                    .as_deref()
                    .unwrap_or("No event candles returned; missing sessions are not filled")
            };
            frame.render_widget(
                Paragraph::new(message).style(if self.last_error.is_some() {
                    styles.error
                } else {
                    styles.muted_text
                }),
                chart,
            );
        }
        text(
            frame,
            inner.height - 3,
            if self.config.view == EventChartMode::Daily {
                "Regular only · OHLC grouped by column · V = column sum"
            } else {
                "P pre R regular A post N night C auction * mixed"
            },
            styles.muted_text,
        );
        text(
            frame,
            inner.height - 2,
            "v View · r / [ ] Range · e Event · Enter Detail",
            styles.status_base(),
        );
        text(
            frame,
            inner.height - 1,
            "? Help · q Quit",
            styles.status_base(),
        );
        if let Some((picker, selected)) = self.picker {
            let labels = match picker {
                Picker::View => EventChartMode::ALL.map(EventChartMode::label),
                Picker::Range => EventRange::ALL.map(EventRange::label),
            };
            let popup = centered(area, 38, 7);
            frame.render_widget(Clear, popup);
            let lines = labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    format!("{} {label}", if index == selected { "›" } else { " " })
                })
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(format!("{lines}\nEnter Select · Backspace Back"))
                    .block(
                        Block::default()
                            .title("Events")
                            .borders(Borders::ALL)
                            .border_style(styles.border),
                    )
                    .style(styles.overlay_base()),
                popup,
            );
        } else if self.details
            && let Some(candle) = selected
        {
            let popup = centered(area, 70, 10);
            frame.render_widget(Clear, popup);
            frame.render_widget(Paragraph::new(format!("{}\nOpen {}   High {}\nLow {}   Close {}\nVolume {}  Turnover {}\nConference: {}\n{}\nBackspace Back", format_instant(candle.timestamp), candle.open,candle.high,candle.low,candle.close,candle.volume,candle.turnover,self.config.conference_at.as_deref().unwrap_or("unknown"),data.map_or("",|data|data.window.session_boundary_note))).block(Block::default().title("Candle / event detail").borders(Borders::ALL).border_style(styles.border)).style(styles.overlay_base()), popup);
        }
    }

    fn draw_prices(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        data: &EventData,
        settings: &Settings,
        marks: &[crate::commands::kline::PriceMark],
        clock: Option<MarketClock>,
    ) {
        use tickloop_ui::{format::write_text, styles::PriceDirection};
        if area.height < 4 || area.width < 12 {
            return;
        }
        let styles = settings.styles();
        let plot_width = usize::from(area.width.saturating_sub(10)).max(1);
        self.columns.set(plot_width);
        let plot_height = area.height.saturating_sub(3).max(1);
        let prices = &data.candles;
        let mut minimum = prices
            .iter()
            .filter_map(|c| c.low.to_f64())
            .fold(f64::INFINITY, f64::min);
        let mut maximum = prices
            .iter()
            .filter_map(|c| c.high.to_f64())
            .fold(f64::NEG_INFINITY, f64::max);
        if !minimum.is_finite() || !maximum.is_finite() {
            return;
        }
        if let Some(baseline) = data.baseline.and_then(|value| value.to_f64()) {
            minimum = minimum.min(baseline);
            maximum = maximum.max(baseline);
        }
        for mark in marks {
            minimum = minimum.min(mark.price);
            maximum = maximum.max(mark.price);
        }
        if (maximum - minimum).abs() < f64::EPSILON {
            minimum -= 1.;
            maximum += 1.;
        }
        let project = |value: f64| {
            (maximum - value) / (maximum - minimum) * f64::from(plot_height.saturating_sub(1))
        };
        let interval = match self.config.view {
            EventChartMode::Line => 60,
            EventChartMode::Minute => 300,
            EventChartMode::Daily => 86400,
        };
        let mut logical = vec![0usize];
        let gap = prices.len().div_ceil(30).max(2);
        for pair in prices.windows(2) {
            let missing = self.config.view != EventChartMode::Daily
                && (pair[1].timestamp - pair[0].timestamp).whole_seconds() > interval * 2;
            logical.push(logical.last().copied().unwrap() + 1 + if missing { gap } else { 0 });
        }
        let span = *logical.last().unwrap_or(&1).max(&1);
        let xs: Vec<_> = logical
            .iter()
            .map(|index| index * (plot_width - 1) / span)
            .collect();
        let mut bins: Vec<Option<PriceVolumeBin>> = vec![None; plot_width];
        let mut sessions = vec![None; plot_width];
        for (candle, x) in prices.iter().zip(&xs) {
            let o = candle.open.to_f64().unwrap_or(minimum);
            let h = candle.high.to_f64().unwrap_or(minimum);
            let l = candle.low.to_f64().unwrap_or(minimum);
            let c = candle.close.to_f64().unwrap_or(minimum);
            match &mut bins[*x] {
                Some((_, high, low, close, volume)) => {
                    *high = high.max(h);
                    *low = low.min(l);
                    *close = c;
                    *volume = volume.saturating_add(candle.volume);
                }
                slot @ None => *slot = Some((o, h, l, c, candle.volume)),
            };
            let session = if self.config.view == EventChartMode::Daily {
                'R'
            } else {
                match clock.map(|clock| {
                    clock.session(
                        candle.timestamp,
                        data.window
                            .half_trading_days
                            .contains(&iso_date(clock.local(candle.timestamp).date())),
                    )
                }) {
                    Some("pre") => 'P',
                    Some("regular") => 'R',
                    Some("post") => 'A',
                    Some("overnight") => 'N',
                    Some("auction") => 'C',
                    _ => '?',
                }
            };
            sessions[*x] = Some(match sessions[*x] {
                Some(previous) if previous != session => '*',
                _ => session,
            });
        }
        if self.config.view == EventChartMode::Line {
            let mut lines: Vec<Vec<(u16, u16)>> = vec![Vec::new()];
            for (index, candle) in prices.iter().enumerate() {
                if index > 0
                    && (candle.timestamp - prices[index - 1].timestamp).whole_seconds()
                        > interval * 2
                {
                    lines.push(Vec::new());
                }
                lines.last_mut().unwrap().push((
                    (xs[index] * 2) as u16,
                    (project(candle.close.to_f64().unwrap_or(minimum)) * 4.)
                        .round()
                        .max(0.) as u16,
                ));
            }
            let cells = tickloop_ui::charts::braille_lines(&lines, plot_width as u16, plot_height);
            for (index, cell) in cells.iter().enumerate() {
                if let Some(cell) = cell {
                    write_text(
                        frame.buffer_mut(),
                        area.x + (index % plot_width) as u16,
                        area.y + (index / plot_width) as u16,
                        &cell.glyph.to_string(),
                        1,
                        settings.ambiguous_width,
                        styles.text,
                    );
                }
            }
        } else {
            for (x, bin) in bins.iter().enumerate() {
                if let Some((o, h, l, c, _)) = bin {
                    let style = styles.price(PriceDirection::from_change(c - o));
                    for row in 0..plot_height {
                        let from_bottom = |v: f64| f64::from(plot_height - 1) - project(v);
                        let glyph = tickloop_ui::charts::candle_glyph(
                            from_bottom(*o),
                            from_bottom(*h),
                            from_bottom(*l),
                            from_bottom(*c),
                            f64::from(plot_height - 1 - row),
                        );
                        write_text(
                            frame.buffer_mut(),
                            area.x + x as u16,
                            area.y + row,
                            &glyph.to_string(),
                            1,
                            settings.ambiguous_width,
                            style,
                        );
                    }
                }
            }
        }
        let max_volume = bins
            .iter()
            .flatten()
            .map(|bin| bin.4)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        for (x, bin) in bins.iter().enumerate() {
            if let Some((o, _, _, c, volume)) = bin {
                let glyph = tickloop_ui::charts::volume_glyph(*volume as f64 / max_volume);
                write_text(
                    frame.buffer_mut(),
                    area.x + x as u16,
                    area.y + plot_height,
                    &glyph.to_string(),
                    1,
                    settings.ambiguous_width,
                    styles.price(PriceDirection::from_change(c - o)),
                );
            }
            if let Some(session) = sessions[x] {
                write_text(
                    frame.buffer_mut(),
                    area.x + x as u16,
                    area.y + plot_height + 1,
                    &session.to_string(),
                    1,
                    settings.ambiguous_width,
                    styles.muted_text,
                );
            }
        }
        write_text(
            frame.buffer_mut(),
            area.x + plot_width as u16 + 1,
            area.y + plot_height,
            "V sum/col",
            9,
            settings.ambiguous_width,
            styles.muted_text,
        );
        write_text(
            frame.buffer_mut(),
            area.x + plot_width as u16 + 1,
            area.y + plot_height + 1,
            "session",
            9,
            settings.ambiguous_width,
            styles.muted_text,
        );
        write_text(
            frame.buffer_mut(),
            area.x + plot_width as u16 + 1,
            area.y + plot_height + 2,
            "gaps cut",
            9,
            settings.ambiguous_width,
            styles.muted_text,
        );
        for row in [0, plot_height / 2, plot_height - 1] {
            let value = maximum
                - f64::from(row) / f64::from(plot_height.saturating_sub(1).max(1))
                    * (maximum - minimum);
            write_text(
                frame.buffer_mut(),
                area.x + plot_width as u16 + 1,
                area.y + row,
                &format!("{value:.2}"),
                9,
                settings.ambiguous_width,
                styles.muted_text,
            );
        }
        if let Some(value) = data.baseline.and_then(|value| value.to_f64()) {
            let row = project(value).round() as u16;
            write_text(
                frame.buffer_mut(),
                area.x + plot_width as u16 + 1,
                area.y + row.min(plot_height - 1),
                "BASE",
                9,
                settings.ambiguous_width,
                styles.price_mark,
            );
        }
        for mark in marks {
            let row = project(mark.price).round() as u16;
            write_text(
                frame.buffer_mut(),
                area.x,
                area.y + row.min(plot_height - 1),
                &"┄".repeat(plot_width),
                plot_width,
                settings.ambiguous_width,
                styles.price_mark,
            );
            write_text(
                frame.buffer_mut(),
                area.x,
                area.y + row.min(plot_height - 1),
                &format!("{} {:.2}", mark.label, mark.price),
                plot_width,
                settings.ambiguous_width,
                styles.price_mark,
            );
        }
        let zone = time_tz::timezones::get_by_name(&data.window.market_timezone);
        let local = |at: OffsetDateTime| zone.map_or(at, |zone| at.to_timezone(zone));
        let axis_label = |at: OffsetDateTime| {
            let at = local(at);
            if self.config.view == EventChartMode::Daily {
                format!("{:02}-{:02}", at.month() as u8, at.day())
            } else {
                format!(
                    "{:02}-{:02} {:02}:{:02}",
                    at.month() as u8,
                    at.day(),
                    at.hour(),
                    at.minute()
                )
            }
        };
        let first_label = axis_label(prices[0].timestamp);
        let last_label = axis_label(prices[prices.len() - 1].timestamp);
        write_text(
            frame.buffer_mut(),
            area.x,
            area.y + plot_height + 2,
            &first_label,
            plot_width,
            settings.ambiguous_width,
            styles.muted_text,
        );
        if plot_width > first_label.len() + last_label.len() + 2 {
            write_text(
                frame.buffer_mut(),
                area.x + (plot_width - last_label.len()) as u16,
                area.y + plot_height + 2,
                &last_label,
                last_label.len(),
                settings.ambiguous_width,
                styles.muted_text,
            );
        }
        let selected = self.cursor.min(xs.len() - 1);
        write_text(
            frame.buffer_mut(),
            area.x + xs[selected] as u16,
            area.y + plot_height - 1,
            "▲",
            1,
            settings.ambiguous_width,
            styles.chart_cursor,
        );
        let marker = |frame: &mut ratatui::Frame<'_>,
                      at: &Option<String>,
                      date: Option<Date>,
                      label: &str,
                      row: u16| {
            let instant = at
                .as_deref()
                .and_then(|at| parse_instant("marker", at).ok());
            let index = if self.config.view == EventChartMode::Daily {
                date.or_else(|| instant.map(|at| local(at).date()))
                    .and_then(|date| {
                        prices
                            .iter()
                            .position(|candle| local(candle.timestamp).date() == date)
                    })
            } else {
                instant
                    .filter(|at| *at >= prices[0].timestamp)
                    .and_then(|at| prices.iter().position(|candle| candle.timestamp >= at))
            };
            if let Some(index) = index {
                let x = xs[index];
                let remaining = plot_width.saturating_sub(x);
                write_text(
                    frame.buffer_mut(),
                    area.x + x as u16,
                    area.y + row.min(plot_height - 1),
                    &label[..label.len().min(remaining)],
                    remaining,
                    settings.ambiguous_width,
                    styles.price_mark,
                );
            }
        };
        marker(
            frame,
            &data.window.published_at,
            parse_iso_date(&self.config.date),
            if self.config.view == EventChartMode::Daily {
                "EVENT"
            } else {
                "T0"
            },
            0,
        );
        marker(frame, &data.window.conference_at, None, "CALL", 1);
    }
}

fn centered(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    ratatui::layout::Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub enum EventEditResult {
    Continue,
    Apply(EventChartConfig),
}

pub struct EventEditor {
    config: EventChartConfig,
    fields: [crate::input::TextInput; 4],
    active: usize,
    error: Option<String>,
}

impl EventEditor {
    pub fn new(symbol: &str, existing: Option<&EventChartConfig>) -> Result<Self> {
        let clock = MarketClock::for_symbol(symbol)?;
        let config = existing.cloned().unwrap_or_else(|| EventChartConfig {
            date: iso_date(clock.local(OffsetDateTime::now_utc()).date()),
            ..EventChartConfig::default()
        });
        let session = serde_json::to_value(config.session)
            .unwrap_or(Value::Null)
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        Ok(Self {
            fields: [
                config.date.clone(),
                session,
                config.published_at.clone().unwrap_or_default(),
                config.conference_at.clone().unwrap_or_default(),
            ]
            .map(crate::input::TextInput::new),
            config,
            active: 0,
            error: None,
        })
    }

    pub fn on_key(&mut self, key: KeyCode) -> EventEditResult {
        match key {
            KeyCode::Tab => {
                self.active = (self.active + 1) % self.fields.len();
                return EventEditResult::Continue;
            }
            KeyCode::BackTab => {
                self.active = (self.active + self.fields.len() - 1) % self.fields.len();
                return EventEditResult::Continue;
            }
            _ => (),
        }
        match self.fields[self.active].handle(key) {
            crate::input::EditOutcome::Submit => {
                let mut config = self.config.clone();
                config.date = self.fields[0].text().trim().into();
                config.session = match serde_json::from_value(Value::String(
                    self.fields[1].text().trim().into(),
                )) {
                    Ok(session) => session,
                    Err(_) => {
                        self.error = Some(
                            "Session: pre_market / after_hours / during_market / closed / unknown"
                                .into(),
                        );
                        return EventEditResult::Continue;
                    }
                };
                config.published_at = nonempty(self.fields[2].text());
                config.conference_at = nonempty(self.fields[3].text());
                match config.validate() {
                    Ok(()) => EventEditResult::Apply(config),
                    Err(error) => {
                        self.error = Some(error.message);
                        EventEditResult::Continue
                    }
                }
            }
            _ => EventEditResult::Continue,
        }
    }

    pub fn draw(&self, frame: &mut ratatui::Frame<'_>, settings: &Settings) {
        use tickloop_ui::format::{width, write_text};
        let styles = settings.styles();
        let popup = centered(frame.area(), 88, 13);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(" Event settings ")
            .borders(Borders::ALL)
            .border_style(styles.border)
            .style(styles.overlay_base());
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        if inner.width < 12 || inner.height < 5 {
            frame.render_widget(
                Paragraph::new("Enlarge to edit event").style(styles.muted_text),
                inner,
            );
            return;
        }
        let labels = ["Date", "Session", "Actual release", "Conference"];
        for (index, input) in self.fields.iter().enumerate() {
            let label = format!(
                "{} {:14} ",
                if index == self.active { "›" } else { " " },
                labels[index]
            );
            let row = inner.y + index as u16;
            let label_width = width(&label, settings.ambiguous_width);
            let (visible, cursor) = input.viewport(
                usize::from(inner.width).saturating_sub(label_width),
                settings.ambiguous_width,
            );
            write_text(
                frame.buffer_mut(),
                inner.x,
                row,
                &format!("{label}{visible}"),
                inner.width as usize,
                settings.ambiguous_width,
                if index == self.active {
                    styles.selection
                } else {
                    styles.text
                },
            );
            if index == self.active {
                let column = label_width + cursor;
                frame.set_cursor_position((
                    inner.x + (column as u16).min(inner.width.saturating_sub(1)),
                    row,
                ));
            }
        }
        let help = "Date: exchange-local YYYY-MM-DD\nSession: pre_market / after_hours / during_market / closed / unknown\nActual times: RFC3339 with offset, or blank if unknown\nTab Next field · Enter Apply · Ctrl+C Cancel";
        frame.render_widget(
            Paragraph::new(help).style(styles.muted_text),
            ratatui::layout::Rect::new(
                inner.x,
                inner.y + 5,
                inner.width,
                inner.height.saturating_sub(5),
            ),
        );
        if let Some(error) = &self.error {
            write_text(
                frame.buffer_mut(),
                inner.x,
                inner.bottom().saturating_sub(1),
                error,
                inner.width as usize,
                settings.ambiguous_width,
                styles.error,
            );
        }
    }
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
