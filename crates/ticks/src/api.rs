use crate::error::{Error, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Quote {
    pub symbol: String,
    pub name: String,
    pub latest_price: Decimal,
    pub previous_close: Decimal,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub volume: i64,
    pub turnover: Decimal,
    pub updated_at: String,
    pub error: Option<String>,
    pub session: QuoteSession,
    #[serde(skip)]
    pub basis_updated_at: String,
}

/// The market services a Tick's callbacks and query tools reach. The alias keeps the trait
/// object `'static`: a declared Tick Tool catalog holds handlers over it and outlives any one
/// call, so a borrowed services object could not be named there.
pub type QuoteServices = dyn QuoteApi;

pub const MAX_HISTORY_PAGE_CANDLES: usize = 1_000;

#[async_trait]
pub trait QuoteApi: Send + Sync {
    fn source_name(&self) -> &'static str {
        "longbridge"
    }
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>>;
    async fn candles(&self, _request: &CandleRequest) -> Result<Vec<Candle>> {
        Err(Error::network("candlesticks unavailable"))
    }
    async fn subscribe_candles(&self, _request: &CandleRequest) -> Result<bool> {
        Ok(false)
    }
    async fn unsubscribe_candles(&self, _symbol: &str, _period: CandlePeriod) -> Result<()> {
        Ok(())
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        Err(Error::network("watchlist groups unavailable"))
    }
    async fn market_states(&self, _symbols: &[String]) -> Result<Vec<MarketState>> {
        Ok(Vec::new())
    }
    async fn subscribe_quotes(&self, _symbols: &[String]) -> Result<()> {
        Err(Error::network("quote subscriptions unavailable"))
    }
    async fn unsubscribe_quotes(&self, _symbols: &[String]) -> Result<()> {
        Ok(())
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Err(Error::network("quote event stream unavailable"))
    }
    async fn consensus(&self, _symbol: &str) -> Result<Vec<ConsensusPeriod>> {
        Err(Error::network("analyst consensus unavailable"))
    }
    async fn key_financial_metrics(&self, _symbol: &str) -> Result<Vec<ReportDate>> {
        Err(Error::network("key financial metrics unavailable"))
    }
    async fn fiscal_period_ends(&self, _symbol: &str) -> Result<Vec<FiscalPeriodEnd>> {
        Err(Error::network("financial reports unavailable"))
    }
    async fn financial_statements(&self, _symbol: &str) -> Result<serde_json::Value> {
        Err(Error::network("financial statement details unavailable"))
    }
    async fn business_segments(&self, _symbol: &str) -> Result<serde_json::Value> {
        Err(Error::network("business segments unavailable"))
    }
    async fn filings(&self, _symbol: &str) -> Result<serde_json::Value> {
        Err(Error::network("company filings unavailable"))
    }
    async fn report_calendar(
        &self,
        _request: &ReportCalendarRequest,
    ) -> Result<ReportCalendarPage> {
        Err(Error::network("finance calendar unavailable"))
    }
    async fn candles_by_date(&self, _request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        Err(Error::network("candlesticks by date unavailable"))
    }
    async fn candles_by_date_with_sessions(
        &self,
        request: &CandleHistoryRequest,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        match sessions {
            CandleSessions::Regular => self.candles_by_date(request).await,
            CandleSessions::All => Err(Error::network("extended-session candlesticks unavailable")),
        }
    }
    /// Returns up to one provider page at or before an exchange-local timestamp.
    async fn candles_before(
        &self,
        _request: &CandleHistoryRequest,
        _before: time::PrimitiveDateTime,
        _sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        Err(Error::network(
            "historical candlestick pagination unavailable",
        ))
    }
    async fn trading_days(&self, _market: &str, _range: DateRange) -> Result<Vec<time::Date>> {
        Err(Error::network("trading days unavailable"))
    }
    async fn half_trading_days(&self, _market: &str, _range: DateRange) -> Result<Vec<time::Date>> {
        Ok(Vec::new())
    }
}

#[async_trait]
pub trait TradeApi: Send + Sync {
    async fn positions(&self) -> Result<serde_json::Value>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseSession {
    PreMarket,
    #[default]
    AfterHours,
}

impl ReleaseSession {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreMarket => "pre_market",
            Self::AfterHours => "after_hours",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PreMarket => "pre-market",
            Self::AfterHours => "after hours",
        }
    }
}

/// Estimate, actual and surprise for one consensus metric of one fiscal period.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ConsensusMetric {
    pub estimate: Option<Decimal>,
    pub actual: Option<Decimal>,
    pub surprise: Option<Decimal>,
}

impl ConsensusMetric {
    /// The reported surprise, or actual minus estimate when both sides are known.
    pub fn resolved_surprise(&self) -> Option<Decimal> {
        self.surprise
            .or_else(|| Some(self.actual? - self.estimate?))
    }

    pub fn beat_estimate(&self) -> bool {
        self.resolved_surprise()
            .is_some_and(|surprise| surprise > Decimal::ZERO)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ConsensusPeriod {
    pub fiscal_year: i32,
    pub fiscal_period: String,
    pub label: String,
    pub is_released: bool,
    pub eps: ConsensusMetric,
    pub revenue: ConsensusMetric,
}

/// The announcement date of one fiscal period as the US key financial metrics endpoint reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportDate {
    pub fiscal_year: i32,
    pub fiscal_period: String,
    pub date: time::Date,
}

/// The last day of one fiscal period as the company's income statement lists it. Fiscal years
/// need not follow the calendar, so the announcement search starts from this date, not from the
/// calendar quarter end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiscalPeriodEnd {
    pub fiscal_year: i32,
    pub fiscal_period: String,
    pub end: time::Date,
}

/// One earnings announcement in the finance calendar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportEvent {
    pub symbol: String,
    pub date: time::Date,
    pub session: ReleaseSession,
    pub fiscal_year: Option<i32>,
    pub fiscal_period: Option<String>,
    /// The estimate and actual the calendar attaches to a past announcement; empty for a
    /// future one and whenever the calendar leaves them out.
    pub eps: ConsensusMetric,
    pub revenue: ConsensusMetric,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateRange {
    pub start: time::Date,
    pub end: time::Date,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportCalendarRequest {
    pub symbol: String,
    pub range: DateRange,
}

/// One page of the market-wide finance calendar, narrowed to the requested symbol.
/// `next_date` continues the walk; the pages are short because the endpoint is market-wide.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportCalendarPage {
    pub events: Vec<ReportEvent>,
    pub next_date: Option<time::Date>,
}

pub fn normalize_symbol(symbol: &str) -> String {
    if !symbol.is_empty()
        && symbol
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        format!("{symbol}.US")
    } else if !symbol.is_empty() && symbol.chars().all(|character| character.is_ascii_digit()) {
        format!("{symbol}.HK")
    } else {
        symbol.to_owned()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct QuoteUpdate {
    pub symbol: String,
    pub latest_price: Decimal,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub volume: i64,
    pub turnover: Decimal,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Reconnecting,
    Disconnected,
}

#[derive(Clone, Debug)]
pub enum MarketEvent {
    Quote(QuoteUpdate),
    SessionQuote {
        update: QuoteUpdate,
        session: QuoteSession,
    },
    QuoteSnapshot(Quote),
    Connection(ConnectionState),
    Market(MarketState),
    Candles(CandleUpdate),
    Failure {
        code: u8,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WatchlistGroup {
    pub id: i64,
    pub name: String,
    pub symbols: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    Pre,
    Open,
    Closed,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MarketState {
    pub market: String,
    pub status: MarketStatus,
}

const QUOTE_BASIS_MAX_AGE: time::Duration = time::Duration::seconds(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSession {
    #[default]
    Intraday,
    Pre,
    Post,
    Overnight,
}

fn quote_time(text: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
}

impl Quote {
    pub fn update_event(&mut self, event: &MarketEvent) {
        match event {
            MarketEvent::Quote(update) => self.update(update),
            MarketEvent::SessionQuote { update, session } => self.update_session(update, *session),
            MarketEvent::QuoteSnapshot(snapshot) => self.apply_snapshot(snapshot),
            _ => (),
        }
    }

    pub fn update(&mut self, update: &QuoteUpdate) {
        self.update_session(update, QuoteSession::Intraday);
    }

    fn update_session(&mut self, update: &QuoteUpdate, session: QuoteSession) {
        if self.symbol != update.symbol {
            return;
        }
        let current = quote_time(&self.updated_at);
        let incoming = quote_time(&update.updated_at);
        if let (Some(current), Some(incoming)) = (current, incoming)
            && incoming < current
        {
            return;
        }
        if self.basis_updated_at.is_empty() {
            self.basis_updated_at = self.updated_at.clone();
        }
        let basis_is_stale = match (quote_time(&self.basis_updated_at), incoming) {
            (Some(basis), Some(incoming)) => incoming - basis > QUOTE_BASIS_MAX_AGE,
            _ => false,
        };
        if session != self.session || basis_is_stale {
            self.previous_close = Decimal::ZERO;
        }
        self.session = session;
        self.latest_price = update.latest_price;
        self.open = update.open;
        self.high = update.high;
        self.low = update.low;
        self.volume = update.volume;
        self.turnover = update.turnover;
        self.updated_at = update.updated_at.clone();
        self.error = None;
    }

    fn apply_snapshot(&mut self, snapshot: &Quote) {
        if self.symbol != snapshot.symbol || snapshot.error.is_some() {
            return;
        }
        let Some(incoming) = quote_time(&snapshot.updated_at) else {
            return;
        };
        let basis = quote_time(if self.basis_updated_at.is_empty() {
            &self.updated_at
        } else {
            &self.basis_updated_at
        });
        if basis.is_some_and(|basis| incoming < basis) {
            return;
        }
        let current = quote_time(&self.updated_at);
        if current.is_none_or(|current| incoming > current) {
            self.latest_price = snapshot.latest_price;
            self.open = snapshot.open;
            self.high = snapshot.high;
            self.low = snapshot.low;
            self.volume = snapshot.volume;
            self.turnover = snapshot.turnover;
            self.updated_at = snapshot.updated_at.clone();
            self.session = snapshot.session;
        } else if self.session != snapshot.session
            || current.is_some_and(|current| current - incoming > QUOTE_BASIS_MAX_AGE)
        {
            return;
        }
        // An equal-time snapshot refreshes the baseline without replacing a consumed push price.
        self.previous_close = snapshot.previous_close;
        self.basis_updated_at = snapshot.updated_at.clone();
        if !snapshot.name.is_empty() {
            self.name = snapshot.name.clone();
        }
        self.error = None;
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Deserialize,
    Serialize,
    JsonSchema,
    clap::ValueEnum,
)]
pub enum CandlePeriod {
    #[default]
    #[serde(rename = "day")]
    Day,
    #[serde(rename = "week")]
    Week,
    #[serde(rename = "month")]
    Month,
    #[value(name = "1m")]
    #[serde(rename = "1m")]
    OneMinute,
    #[value(name = "5m")]
    #[serde(rename = "5m")]
    FiveMinute,
    #[value(name = "15m")]
    #[serde(rename = "15m")]
    FifteenMinute,
    #[value(name = "30m")]
    #[serde(rename = "30m")]
    ThirtyMinute,
    #[value(name = "60m")]
    #[serde(rename = "60m")]
    SixtyMinute,
}

impl CandlePeriod {
    pub const ALL: [Self; 8] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::OneMinute,
        Self::FiveMinute,
        Self::FifteenMinute,
        Self::ThirtyMinute,
        Self::SixtyMinute,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::OneMinute => "1m",
            Self::FiveMinute => "5m",
            Self::FifteenMinute => "15m",
            Self::ThirtyMinute => "30m",
            Self::SixtyMinute => "60m",
        }
    }

    /// An upper bound on the candles a span of calendar days can produce in this period. It is
    /// deliberately generous — five trading days a week and a full US session a day — because it
    /// exists to refuse a request no single answer could carry, not to predict the exact count.
    pub fn most_candles_in(self, calendar_days: u64) -> u64 {
        const TRADING_DAYS_PER_WEEK: u64 = 5;
        const CALENDAR_DAYS_PER_WEEK: u64 = 7;
        const TRADING_DAYS_PER_MONTH: u64 = 21;
        const SESSION_MINUTES: u64 = 390;
        let trading_days = calendar_days
            .saturating_mul(TRADING_DAYS_PER_WEEK)
            .div_ceil(CALENDAR_DAYS_PER_WEEK)
            .max(1);
        let minutes_per_candle = match self {
            Self::Day => return trading_days,
            Self::Week => return trading_days.div_ceil(TRADING_DAYS_PER_WEEK).max(1),
            Self::Month => return trading_days.div_ceil(TRADING_DAYS_PER_MONTH).max(1),
            Self::OneMinute => 1,
            Self::FiveMinute => 5,
            Self::FifteenMinute => 15,
            Self::ThirtyMinute => 30,
            Self::SixtyMinute => 60,
        };
        trading_days * SESSION_MINUTES.div_ceil(minutes_per_candle)
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum CandleAdjust {
    #[default]
    None,
    Forward,
}

impl CandleAdjust {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Forward => "forward",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandleRequest {
    pub symbol: String,
    pub period: CandlePeriod,
    pub count: usize,
    pub adjust: CandleAdjust,
}

/// Candlesticks of one period between two dates, the shape the Agent recovers
/// history with after the latest-only update stream dropped an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandleHistoryRequest {
    pub symbol: String,
    pub period: CandlePeriod,
    pub adjust: CandleAdjust,
    pub range: DateRange,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandleSessions {
    #[default]
    Regular,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Candle {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: i64,
    pub turnover: Decimal,
}

#[derive(Clone, Debug)]
pub struct CandleUpdate {
    pub symbol: String,
    pub period: CandlePeriod,
    pub adjust: CandleAdjust,
    pub candles: Vec<Candle>,
    pub replace: bool,
    pub requested_at: Option<tokio::time::Instant>,
}
