use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use longbridge::{
    Config,
    httpclient::{HttpClient, HttpClientConfig, Json, Method},
    oauth::OAuth,
    quote::QuoteContext,
    trade::TradeContext,
};
use std::sync::Arc;
use tick_provider::{
    api::{
        Candle, CandleAdjust, CandleHistoryRequest, CandlePeriod, CandleRequest, CandleSessions,
        CandleUpdate, ConnectionState, ConsensusMetric, ConsensusPeriod, DateRange,
        FiscalPeriodEnd, MarketEvent, MarketState, MarketStatus, Quote, QuoteApi, QuoteSession,
        QuoteUpdate, ReleaseSession, ReportCalendarPage, ReportCalendarRequest, ReportDate,
        TradeApi, WatchlistGroup,
    },
    error::{Error, Result},
    preferences::{Language, Region},
};

const INVALID_REQUEST: i64 = 301600;
const NO_QUOTE: i64 = 301603;
const NO_QUOTE_PERMISSION: i64 = 301604;
const TOKEN_EXPIRED: i64 = 401003;
const QUOTE_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const QUOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const QUOTE_PROBE_BATCH_SIZE: usize = 50;
const CANDLE_SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const KEY_METRICS_REPORT: &str = "quarterly";
const CONSENSUS_EPS_KEY: &str = "eps";
const CONSENSUS_REVENUE_KEY: &str = "revenue";
const PRE_MARKET_SESSION: &str = "before";
const PRE_MARKET_DATE_TYPE: &str = "盘前";

#[derive(Clone, Copy)]
struct QuoteObservation {
    session: QuoteSession,
    timestamp: time::OffsetDateTime,
}

#[derive(Default)]
struct QuoteTracking {
    symbols: std::collections::BTreeSet<String>,
    observed: std::collections::BTreeMap<String, QuoteObservation>,
}

pub async fn config(
    oauth: OAuth,
    language: Language,
    region: Option<Region>,
) -> Result<Arc<Config>> {
    let token = oauth
        .access_token()
        .await
        .map_err(|_| Error::auth("authorization expired"))?;
    let region = if token.starts_with("us_") {
        Region::Global
    } else {
        region.unwrap_or(Region::Global)
    };
    let (http, quote, trade) = match region {
        Region::Cn => (
            "https://openapi.longbridge.cn",
            "wss://openapi-quote.longbridge.cn/v2",
            "wss://openapi-trade.longbridge.cn/v2",
        ),
        Region::Global => (
            "https://openapi.longbridge.com",
            "wss://openapi-quote.longbridge.com/v2",
            "wss://openapi-trade.longbridge.com/v2",
        ),
    };
    Ok(Arc::new(
        Config::from_oauth(oauth)
            .http_url(http)
            .quote_ws_url(quote)
            .trade_ws_url(trade)
            .language(match language {
                Language::ZhCn => longbridge::Language::ZH_CN,
                Language::En => longbridge::Language::EN,
            })
            .push_candlestick_mode(longbridge::PushCandlestickMode::Realtime)
            .dont_print_quote_packages(),
    ))
}

type CandleSubscriptions = std::collections::BTreeMap<(String, CandlePeriod), Arc<CandleRequest>>;

pub struct SdkQuoteApi {
    pub context: QuoteContext,
    candle_subscriptions: Arc<tokio::sync::Mutex<CandleSubscriptions>>,
    quote_tracking: Arc<tokio::sync::Mutex<QuoteTracking>>,
    probe_wake: Arc<tokio::sync::Notify>,
    market_context: longbridge::MarketContext,
    fundamental_context: longbridge::FundamentalContext,
    calendar_context: longbridge::CalendarContext,
    signed_http: HttpClient,
    pub limiter: Arc<RateLimiter>,
    language: Language,
    pub pushes: tokio::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<longbridge::quote::PushEvent>>,
    >,
}

impl SdkQuoteApi {
    pub async fn new(
        config: Arc<Config>,
        oauth: OAuth,
        language: Language,
        region: Option<Region>,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self> {
        let token = oauth
            .access_token()
            .await
            .map_err(|_| Error::auth("authorization expired"))?;
        let region = if longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us {
            Region::Global
        } else {
            region.unwrap_or(Region::Global)
        };
        let base = match region {
            Region::Cn => "https://openapi.longbridge.cn",
            Region::Global => "https://openapi.longbridge.com",
        };
        let signed_http = HttpClient::new(HttpClientConfig::from_oauth(oauth).http_url(base))
            .header("accept-language", language.as_str());
        let market_context = longbridge::MarketContext::new(config.clone());
        let fundamental_context = longbridge::FundamentalContext::new(config.clone());
        let calendar_context = longbridge::CalendarContext::new(config.clone());
        let (context, pushes) = QuoteContext::new(config);
        Ok(Self {
            context,
            candle_subscriptions: Arc::default(),
            quote_tracking: Arc::default(),
            probe_wake: Arc::default(),
            market_context,
            fundamental_context,
            calendar_context,
            signed_http,
            language,
            limiter,
            pushes: tokio::sync::Mutex::new(Some(pushes)),
        })
    }
}

pub fn sdk_error(error: longbridge::Error) -> Error {
    if error.openapi_error_code() == Some(TOKEN_EXPIRED)
        || matches!(&error, longbridge::Error::OAuth(_))
    {
        Error::auth("authorization expired or revoked")
    } else {
        Error::network(error.to_string())
    }
}

/// An entitlement gap is not a network failure: the fundamentals endpoints refuse a credential
/// whose data center or quote permissions do not cover them, and that exits as not authorized.
pub fn fundamentals_error(error: longbridge::Error) -> Error {
    let restricted = matches!(
        &error,
        longbridge::Error::HttpClient(
            longbridge::httpclient::HttpClientError::DcRegionRestricted { .. }
        )
    ) || error.openapi_error_code() == Some(NO_QUOTE_PERMISSION);
    if restricted {
        Error::auth(format!("Longbridge entitlement does not cover {error}"))
    } else {
        sdk_error(error)
    }
}

#[async_trait]
impl QuoteApi for SdkQuoteApi {
    async fn candles(&self, request: &CandleRequest) -> Result<Vec<Candle>> {
        let candles = self
            .limiter
            .execute(|| {
                self.context.candlesticks(
                    &request.symbol,
                    sdk_period(request.period),
                    request.count,
                    sdk_adjust(request.adjust),
                    longbridge::quote::TradeSessions::Intraday,
                )
            })
            .await
            .map_err(sdk_error)?;
        Ok(candles.into_iter().map(candle).collect())
    }

    async fn consensus(&self, symbol: &str) -> Result<Vec<ConsensusPeriod>> {
        let response = self
            .limiter
            .execute(|| self.fundamental_context.consensus(symbol))
            .await
            .map_err(fundamentals_error)?;
        Ok(response.list.into_iter().map(consensus_period).collect())
    }

    async fn key_financial_metrics(&self, symbol: &str) -> Result<Vec<ReportDate>> {
        let response = self
            .limiter
            .execute(|| {
                self.fundamental_context
                    .us_key_financial_metrics(symbol, KEY_METRICS_REPORT)
            })
            .await
            .map_err(fundamentals_error)?;
        Ok(response
            .list
            .into_iter()
            .filter_map(|item| {
                Some(ReportDate {
                    fiscal_year: item.ff_year,
                    fiscal_period: tick_provider::fiscal::normalized_fiscal_period(&item.ff_period),
                    date: tick_provider::dates::parse_iso_date(&item.rpt_date)?,
                })
            })
            .collect())
    }

    async fn fiscal_period_ends(&self, symbol: &str) -> Result<Vec<FiscalPeriodEnd>> {
        let response = self
            .limiter
            .execute(|| {
                self.fundamental_context.financial_report(
                    symbol,
                    longbridge::fundamental::types::FinancialReportKind::IncomeStatement,
                    Some(longbridge::fundamental::types::FinancialReportPeriod::QuarterlyFull),
                )
            })
            .await
            .map_err(fundamentals_error)?;
        Ok(tick_provider::fiscal::fiscal_period_ends_from_report(
            &response.list,
        ))
    }

    async fn report_calendar(&self, request: &ReportCalendarRequest) -> Result<ReportCalendarPage> {
        let market = request
            .symbol
            .rsplit_once('.')
            .map(|(_, market)| market.to_owned());
        let start = tick_provider::dates::iso_date(request.range.start);
        let end = tick_provider::dates::iso_date(request.range.end);
        let page = self
            .limiter
            .execute(|| {
                self.calendar_context.finance_calendar(
                    longbridge::calendar::CalendarCategory::Report,
                    start.as_str(),
                    end.as_str(),
                    market.clone(),
                )
            })
            .await
            .map_err(fundamentals_error)?;
        Ok(ReportCalendarPage {
            events: page
                .list
                .iter()
                .flat_map(|group| {
                    group
                        .infos
                        .iter()
                        .filter(|info| info.symbol == request.symbol)
                        .filter_map(|info| report_event(&group.date, info))
                })
                .collect(),
            next_date: tick_provider::dates::parse_iso_date(&page.next_date),
        })
    }

    async fn financial_statements(&self, symbol: &str) -> Result<serde_json::Value> {
        let response = self
            .limiter
            .execute(|| {
                self.fundamental_context.financial_report(
                    symbol,
                    longbridge::fundamental::types::FinancialReportKind::All,
                    Some(longbridge::fundamental::types::FinancialReportPeriod::QuarterlyFull),
                )
            })
            .await
            .map_err(fundamentals_error)?;
        Ok(response.list)
    }

    async fn business_segments(&self, symbol: &str) -> Result<serde_json::Value> {
        // The SDK's typed response drops report_txt, which identifies fiscal quarters even
        // when its nominal date differs from the issuer's reported fiscal period end.
        let query = serde_json::json!({
            "counter_id": longbridge::counter::symbol_to_counter_id(symbol),
            "report": "qf",
        });
        let response = self
            .limiter
            .execute(|| {
                self.signed_http
                    .request(
                        Method::GET,
                        "/v1/quote/fundamentals/business-segments/history",
                    )
                    .query_params(&query)
                    .response::<Json<serde_json::Value>>()
                    .send()
            })
            .await
            .map_err(|error| fundamentals_error(error.into()))?;
        Ok(response.0)
    }

    async fn filings(&self, symbol: &str) -> Result<serde_json::Value> {
        let response = self
            .limiter
            .execute(|| self.context.filings(symbol))
            .await
            .map_err(sdk_error)?;
        serde_json::to_value(response).map_err(|error| Error::network(error.to_string()))
    }

    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        self.candles_by_date_with_sessions(request, CandleSessions::Regular)
            .await
    }

    async fn candles_by_date_with_sessions(
        &self,
        request: &CandleHistoryRequest,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        let candles = self
            .limiter
            .execute(|| {
                self.context.history_candlesticks_by_date(
                    &request.symbol,
                    sdk_period(request.period),
                    sdk_adjust(request.adjust),
                    Some(request.range.start),
                    Some(request.range.end),
                    match sessions {
                        CandleSessions::Regular => longbridge::quote::TradeSessions::Intraday,
                        CandleSessions::All => longbridge::quote::TradeSessions::All,
                    },
                )
            })
            .await
            .map_err(sdk_error)?;
        Ok(candles.into_iter().map(candle).collect())
    }

    async fn trading_days(&self, market: &str, range: DateRange) -> Result<Vec<time::Date>> {
        let market = sdk_market(market)?;
        let days = self
            .limiter
            .execute(|| self.context.trading_days(market, range.start, range.end))
            .await
            .map_err(sdk_error)?;
        let mut trading_days = days.trading_days;
        trading_days.extend(days.half_trading_days);
        trading_days.sort_unstable();
        trading_days.dedup();
        Ok(trading_days)
    }

    async fn candles_before(
        &self,
        request: &CandleHistoryRequest,
        before: time::PrimitiveDateTime,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        let candles = self
            .limiter
            .execute(|| {
                self.context.history_candlesticks_by_offset(
                    &request.symbol,
                    sdk_period(request.period),
                    sdk_adjust(request.adjust),
                    false,
                    Some(before),
                    tick_provider::api::MAX_HISTORY_PAGE_CANDLES,
                    match sessions {
                        CandleSessions::Regular => longbridge::quote::TradeSessions::Intraday,
                        CandleSessions::All => longbridge::quote::TradeSessions::All,
                    },
                )
            })
            .await
            .map_err(sdk_error)?;
        Ok(candles.into_iter().map(candle).collect())
    }

    async fn half_trading_days(&self, market: &str, range: DateRange) -> Result<Vec<time::Date>> {
        let market = sdk_market(market)?;
        let days = self
            .limiter
            .execute(|| self.context.trading_days(market, range.start, range.end))
            .await
            .map_err(sdk_error)?;
        Ok(days.half_trading_days)
    }

    async fn subscribe_candles(&self, request: &CandleRequest) -> Result<bool> {
        if !request
            .symbol
            .rsplit_once('.')
            .is_some_and(|(_, market)| matches!(market, "US" | "HK" | "SH" | "SZ" | "SG"))
        {
            return Ok(false);
        }
        self.candle_subscriptions.lock().await.insert(
            (request.symbol.clone(), request.period),
            Arc::new(request.clone()),
        );
        self.limiter
            .execute(|| {
                self.context.subscribe_candlesticks(
                    &request.symbol,
                    sdk_period(request.period),
                    longbridge::quote::TradeSessions::Intraday,
                )
            })
            .await
            .map_err(sdk_error)?;
        self.probe_wake.notify_one();
        Ok(true)
    }

    async fn unsubscribe_candles(&self, symbol: &str, period: CandlePeriod) -> Result<()> {
        self.candle_subscriptions
            .lock()
            .await
            .remove(&(symbol.to_owned(), period));
        self.limiter
            .execute(|| {
                self.context
                    .unsubscribe_candlesticks(symbol, sdk_period(period))
            })
            .await
            .map_err(sdk_error)
    }

    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        let groups = self
            .limiter
            .execute(|| self.context.watchlist())
            .await
            .map_err(sdk_error)?;
        Ok(groups
            .into_iter()
            .map(|group| WatchlistGroup {
                id: group.id,
                name: group.name,
                symbols: group
                    .securities
                    .into_iter()
                    .map(|security| security.symbol)
                    .collect(),
            })
            .collect())
    }

    async fn market_states(&self, symbols: &[String]) -> Result<Vec<MarketState>> {
        use longbridge::market::TradeStatus;
        let response = self
            .limiter
            .execute(|| self.market_context.market_status())
            .await
            .map_err(sdk_error)?;
        Ok(response
            .market_time
            .into_iter()
            .filter_map(|item| {
                let market = item.market.to_string();
                if !symbols.iter().any(|symbol| {
                    symbol.rsplit_once('.').is_some_and(|(_, suffix)| {
                        suffix == market || market == "CN" && matches!(suffix, "SH" | "SZ")
                    })
                }) {
                    return None;
                }
                let status = match item.trade_status.normalize() {
                    TradeStatus::US_PREV
                    | TradeStatus::OPEN_BID
                    | TradeStatus::DARK_WAIT
                    | TradeStatus::WILL_OPEN => MarketStatus::Pre,
                    TradeStatus::TRADING
                    | TradeStatus::US_TRADING
                    | TradeStatus::US_AFTER
                    | TradeStatus::US_NIGHT
                    | TradeStatus::CLOSE_BID
                    | TradeStatus::DARK_TRADING
                    | TradeStatus::AFTER_FIX => MarketStatus::Open,
                    TradeStatus::CLOSING
                    | TradeStatus::US_CLOSING
                    | TradeStatus::HALF_CLOSING
                    | TradeStatus::MORNING_CLOSING
                    | TradeStatus::NOON_CLOSING
                    | TradeStatus::DARK_CLOSING
                    | TradeStatus::NOT_OPENED
                    | TradeStatus::REALTIME_QUOTE
                    | TradeStatus::US_STOP
                    | TradeStatus::STOP
                    | TradeStatus::COMMON_SUSPEND
                    | TradeStatus::TRADING_HALT
                    | TradeStatus::FUSE
                    | TradeStatus::DELIST
                    | TradeStatus::EXPIRE => MarketStatus::Closed,
                    _ => MarketStatus::Unknown,
                };
                Some(MarketState { market, status })
            })
            .collect())
    }

    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.quote_tracking
            .lock()
            .await
            .symbols
            .extend(symbols.iter().cloned());
        self.limiter
            .execute(|| {
                self.context
                    .subscribe(symbols, longbridge::quote::SubFlags::QUOTE)
            })
            .await
            .map_err(sdk_error)?;
        self.probe_wake.notify_one();
        Ok(())
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        if symbols.is_empty() {
            return Ok(());
        }
        {
            let mut tracking = self.quote_tracking.lock().await;
            for symbol in symbols {
                tracking.symbols.remove(symbol);
                tracking.observed.remove(symbol);
            }
        }
        self.limiter
            .execute(|| {
                self.context
                    .unsubscribe(symbols, longbridge::quote::SubFlags::QUOTE)
            })
            .await
            .map_err(sdk_error)
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        let mut pushes = self
            .pushes
            .lock()
            .await
            .take()
            .ok_or_else(|| Error::usage("event stream already taken"))?;
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let candle_subscriptions = self.candle_subscriptions.clone();
        let context = self.context.clone();
        let limiter = self.limiter.clone();
        let quote_tracking = self.quote_tracking.clone();
        let probe_wake = self.probe_wake.clone();
        tokio::spawn(monitor_quotes(
            context.clone(),
            limiter.clone(),
            quote_tracking.clone(),
            candle_subscriptions.clone(),
            probe_wake.clone(),
            sender.clone(),
        ));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sender.closed() => break,
                    event = pushes.recv() => {
                        let Some(event) = event else { break; };
                        match event.detail {
                            longbridge::quote::PushEventDetail::Quote(quote) => {
                                let session = quote_session(quote.trade_session);
                                let needs_basis = {
                                    let mut tracking = quote_tracking.lock().await;
                                    let previous = tracking.observed.insert(event.symbol.clone(), QuoteObservation {session,timestamp:quote.timestamp});
                                    previous.is_none_or(|previous| previous.session != session || quote.timestamp - previous.timestamp > time::Duration::seconds(10))
                                };
                                let update = QuoteUpdate { symbol: event.symbol, latest_price: quote.last_done, open: quote.open,
                                    high: quote.high, low: quote.low, volume: quote.volume, turnover: quote.turnover,
                                    updated_at: quote.timestamp.format(&time::format_description::well_known::Rfc3339).unwrap_or_default() };
                                if sender.send(MarketEvent::SessionQuote {update,session}).is_err() { break; }
                                if needs_basis { probe_wake.notify_one(); }
                            }
                            longbridge::quote::PushEventDetail::Candlestick(push) => {
                                let Some(period) = candle_period(push.period) else { continue; };
                                let request = candle_subscriptions.lock().await.get(&(event.symbol.clone(),period)).cloned();
                                let Some(request) = request else { continue; };
                                let replace = true;
                                let requested_at = Some(tokio::time::Instant::now());
                                let candles = {
                                    // SDK reconnects can leave gaps in locally aggregated candles; obtain authoritative history on each notification.
                                    let fetched = tokio::select! {
                                        _ = sender.closed() => break,
                                        fetched = async {
                                            tokio::time::timeout(CANDLE_SNAPSHOT_TIMEOUT, limiter.execute(|| context.candlesticks(&request.symbol,sdk_period(period),request.count,sdk_adjust(request.adjust),longbridge::quote::TradeSessions::Intraday)))
                                                .await.map_err(|_| Error::network("candle refresh timed out"))?
                                                .map_err(sdk_error)
                                        } => fetched,
                                    };
                                    let is_active = candle_subscriptions.lock().await.get(&(event.symbol.clone(), period)).is_some_and(|active| Arc::ptr_eq(active, &request));
                                    if !is_active { continue; }
                                    match fetched {
                                        Ok(candles) => candles.into_iter().map(candle).collect(),
                                        Err(error) => {
                                            let code = error.code;
                                            if sender.send(MarketEvent::Failure { code,message:error.message }).is_err() || code == tick_provider::exit_code::NOT_AUTHORIZED { break; }
                                            continue;
                                        }
                                    }
                                };
                                if sender.send(MarketEvent::Candles(CandleUpdate {symbol:event.symbol,period,adjust:request.adjust,candles,replace,requested_at})).is_err() {break;}
                            }
                            _ => (),
                        }
                    }
                }
            }
        });
        Ok(events)
    }

    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let mut rows = Vec::new();
        for symbol in symbols {
            let result = self.limiter.execute(|| self.context.quote([symbol])).await;
            let quote = match result {
                Ok(quotes) => quotes.into_iter().find(|quote| &quote.symbol == symbol),
                Err(error) if is_symbol_error(&error) => {
                    rows.push(Quote {
                        symbol: symbol.clone(),
                        error: Some(error.to_string()),
                        ..Quote::default()
                    });
                    continue;
                }
                Err(error) => return Err(sdk_error(error)),
            };
            let Some(quote) = quote else {
                rows.push(Quote {
                    symbol: symbol.clone(),
                    error: Some("No quote or permission for this symbol".into()),
                    ..Quote::default()
                });
                continue;
            };
            let names = match self
                .limiter
                .execute(|| self.context.static_info([symbol]))
                .await
            {
                Ok(names) => names,
                Err(error) if is_symbol_error(&error) => {
                    rows.push(Quote {
                        symbol: symbol.clone(),
                        error: Some(error.to_string()),
                        ..Quote::default()
                    });
                    continue;
                }
                Err(error) => return Err(sdk_error(error)),
            };
            let name = names
                .into_iter()
                .find(|info| &info.symbol == symbol)
                .map(|info| match self.language {
                    Language::ZhCn => info.name_cn,
                    Language::En => info.name_en,
                })
                .unwrap_or_default();
            let mut snapshot = quote_snapshot(quote, QuoteSession::Intraday);
            snapshot.name = name;
            rows.push(snapshot);
        }
        Ok(rows)
    }
}

pub struct SdkTradeApi {
    pub context: TradeContext,
    pub limiter: Arc<RateLimiter>,
    pub is_us: bool,
}

#[async_trait]
impl TradeApi for SdkTradeApi {
    async fn positions(&self) -> Result<serde_json::Value> {
        if self.is_us {
            return crate::us_positions::snapshot(&self.context, &self.limiter).await;
        }
        let positions = self
            .limiter
            .execute(|| self.context.stock_positions(None))
            .await
            .map_err(sdk_error)?;
        serde_json::to_value(positions).map_err(|error| Error::network(error.to_string()))
    }
}

fn is_symbol_error(error: &longbridge::Error) -> bool {
    matches!(
        error.openapi_error_code(),
        Some(INVALID_REQUEST | NO_QUOTE | NO_QUOTE_PERMISSION)
    ) || matches!(
        error,
        longbridge::Error::InvalidSecuritySymbol { .. } | longbridge::Error::UnknownMarket { .. }
    )
}

fn sdk_adjust(adjust: CandleAdjust) -> longbridge::quote::AdjustType {
    match adjust {
        CandleAdjust::None => longbridge::quote::AdjustType::NoAdjust,
        CandleAdjust::Forward => longbridge::quote::AdjustType::ForwardAdjust,
    }
}

fn sdk_period(period: CandlePeriod) -> longbridge::quote::Period {
    use longbridge::quote::Period;
    match period {
        CandlePeriod::Day => Period::Day,
        CandlePeriod::Week => Period::Week,
        CandlePeriod::Month => Period::Month,
        CandlePeriod::OneMinute => Period::OneMinute,
        CandlePeriod::FiveMinute => Period::FiveMinute,
        CandlePeriod::FifteenMinute => Period::FifteenMinute,
        CandlePeriod::ThirtyMinute => Period::ThirtyMinute,
        CandlePeriod::SixtyMinute => Period::SixtyMinute,
    }
}

fn candle_period(period: longbridge::quote::Period) -> Option<CandlePeriod> {
    CandlePeriod::ALL
        .into_iter()
        .find(|candidate| sdk_period(*candidate) == period)
}

fn sdk_market(market: &str) -> Result<longbridge::Market> {
    match market {
        "US" => Ok(longbridge::Market::US),
        "HK" => Ok(longbridge::Market::HK),
        "SG" => Ok(longbridge::Market::SG),
        "CN" | "SH" | "SZ" => Ok(longbridge::Market::CN),
        other => Err(Error::usage(format!("unsupported market {other}"))),
    }
}

fn consensus_period(report: longbridge::fundamental::types::ConsensusReport) -> ConsensusPeriod {
    let metric = |key: &str| {
        report
            .details
            .iter()
            .find(|detail| detail.key == key)
            .map(|detail| ConsensusMetric {
                estimate: detail.estimate,
                actual: detail.actual,
                surprise: detail.comp_value,
            })
            .unwrap_or_default()
    };
    ConsensusPeriod {
        fiscal_year: report.fiscal_year,
        fiscal_period: tick_provider::fiscal::normalized_fiscal_period(&report.fiscal_period),
        label: report.period_text.clone(),
        is_released: report.details.iter().any(|detail| detail.is_released),
        eps: metric(CONSENSUS_EPS_KEY),
        revenue: metric(CONSENSUS_REVENUE_KEY),
    }
}

const CALENDAR_EPS_ESTIMATE: &str = "estimate_eps";
const CALENDAR_EPS_ACTUAL: &str = "actual_eps";
const CALENDAR_REVENUE_ESTIMATE: &str = "estimate_revenue";
const CALENDAR_REVENUE_ACTUAL: &str = "actual_revenue";

fn report_event(
    date: &str,
    info: &longbridge::calendar::CalendarEventInfo,
) -> Option<tick_provider::api::ReportEvent> {
    let raw_value = |value_type: &str| {
        info.data_kv
            .iter()
            .find(|entry| entry.value_type == value_type)
            .and_then(|entry| entry.value_raw)
    };
    let financial_report = info
        .ext
        .as_ref()
        .and_then(|ext| ext.get("financial_report"));
    let field = |key: &str| {
        financial_report
            .and_then(|report| report.get(key))
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned)
            })
    };
    let market_time = field("market_time").unwrap_or_default();
    let session = if market_time.eq_ignore_ascii_case(PRE_MARKET_SESSION)
        || info.date_type.contains(PRE_MARKET_DATE_TYPE)
    {
        ReleaseSession::PreMarket
    } else {
        ReleaseSession::AfterHours
    };
    Some(tick_provider::api::ReportEvent {
        symbol: info.symbol.clone(),
        date: tick_provider::dates::parse_iso_date(date)?,
        session,
        fiscal_year: field("fiscal_year").and_then(|year| year.trim().parse().ok()),
        fiscal_period: field("period").map(|period| {
            tick_provider::fiscal::normalized_fiscal_period(period.trim_matches('"'))
        }),
        eps: ConsensusMetric {
            estimate: raw_value(CALENDAR_EPS_ESTIMATE),
            actual: raw_value(CALENDAR_EPS_ACTUAL),
            surprise: None,
        },
        revenue: ConsensusMetric {
            estimate: raw_value(CALENDAR_REVENUE_ESTIMATE),
            actual: raw_value(CALENDAR_REVENUE_ACTUAL),
            surprise: None,
        },
    })
}

fn candle(candle: longbridge::quote::Candlestick) -> Candle {
    Candle {
        timestamp: candle.timestamp,
        open: candle.open,
        high: candle.high,
        low: candle.low,
        close: candle.close,
        volume: candle.volume,
        turnover: candle.turnover,
    }
}

fn quote_session(session: longbridge::quote::TradeSession) -> QuoteSession {
    match session {
        longbridge::quote::TradeSession::Intraday => QuoteSession::Intraday,
        longbridge::quote::TradeSession::Pre => QuoteSession::Pre,
        longbridge::quote::TradeSession::Post => QuoteSession::Post,
        longbridge::quote::TradeSession::Overnight => QuoteSession::Overnight,
    }
}

fn quote_snapshot(
    quote: longbridge::quote::SecurityQuote,
    preferred_session: QuoteSession,
) -> Quote {
    let candidates = [
        Some((QuoteSession::Intraday, quote.timestamp)),
        quote
            .pre_market_quote
            .as_ref()
            .map(|value| (QuoteSession::Pre, value.timestamp)),
        quote
            .post_market_quote
            .as_ref()
            .map(|value| (QuoteSession::Post, value.timestamp)),
        quote
            .overnight_quote
            .as_ref()
            .map(|value| (QuoteSession::Overnight, value.timestamp)),
    ];
    let session = candidates
        .into_iter()
        .flatten()
        .max_by_key(|(session, timestamp)| (*timestamp, *session == preferred_session))
        .map(|(session, _)| session)
        .unwrap_or(QuoteSession::Intraday);
    let extended = match session {
        QuoteSession::Intraday => None,
        QuoteSession::Pre => quote.pre_market_quote.as_ref(),
        QuoteSession::Post => quote.post_market_quote.as_ref(),
        QuoteSession::Overnight => quote.overnight_quote.as_ref(),
    };
    Quote {
        symbol: quote.symbol.clone(),
        latest_price: extended.map_or(quote.last_done, |value| value.last_done),
        previous_close: extended.map_or(quote.prev_close, |value| value.prev_close),
        open: if extended.is_some() {
            rust_decimal::Decimal::ZERO
        } else {
            quote.open
        },
        high: extended.map_or(quote.high, |value| value.high),
        low: extended.map_or(quote.low, |value| value.low),
        volume: extended.map_or(quote.volume, |value| value.volume),
        turnover: extended.map_or(quote.turnover, |value| value.turnover),
        updated_at: extended
            .map_or(quote.timestamp, |value| value.timestamp)
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        session: if extended.is_some() {
            session
        } else {
            QuoteSession::Intraday
        },
        ..Quote::default()
    }
}

async fn probe_symbols(
    tracking: &tokio::sync::Mutex<QuoteTracking>,
    candles: &tokio::sync::Mutex<CandleSubscriptions>,
) -> std::collections::BTreeMap<String, QuoteSession> {
    let tracking = tracking.lock().await;
    let candles = candles.lock().await;
    tracking
        .symbols
        .iter()
        .chain(candles.keys().map(|(symbol, _)| symbol))
        .map(|symbol| {
            (
                symbol.clone(),
                tracking
                    .observed
                    .get(symbol)
                    .map_or(QuoteSession::Intraday, |quote| quote.session),
            )
        })
        .collect()
}

async fn monitor_quotes(
    context: QuoteContext,
    limiter: Arc<RateLimiter>,
    tracking: Arc<tokio::sync::Mutex<QuoteTracking>>,
    candles: Arc<tokio::sync::Mutex<CandleSubscriptions>>,
    wake: Arc<tokio::sync::Notify>,
    sender: tokio::sync::mpsc::UnboundedSender<MarketEvent>,
) {
    let mut interval = tokio::time::interval(QUOTE_PROBE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut connection = None;
    loop {
        tokio::select! {
            _ = sender.closed() => return,
            _ = interval.tick() => (),
            _ = wake.notified() => (),
        }
        let symbols: Vec<String> = probe_symbols(&tracking, &candles)
            .await
            .into_keys()
            .collect();
        if symbols.is_empty() {
            continue;
        }
        if connection == Some(ConnectionState::Disconnected) {
            connection = Some(ConnectionState::Reconnecting);
            if sender
                .send(MarketEvent::Connection(ConnectionState::Reconnecting))
                .is_err()
            {
                return;
            }
        }
        let mut failure = None;
        for batch in symbols.chunks(QUOTE_PROBE_BATCH_SIZE) {
            let response = tokio::select! {
                _ = sender.closed() => return,
                response = tokio::time::timeout(QUOTE_PROBE_TIMEOUT, limiter.execute(|| context.quote(batch))) => response,
            };
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    failure = Some(sdk_error(error));
                    break;
                }
                Err(_) => {
                    failure = Some(Error::network("quote availability probe timed out"));
                    break;
                }
            };
            let active = probe_symbols(&tracking, &candles).await;
            for quote in response {
                let Some(session) = active.get(&quote.symbol).copied() else {
                    continue;
                };
                if sender
                    .send(MarketEvent::QuoteSnapshot(quote_snapshot(quote, session)))
                    .is_err()
                {
                    return;
                }
            }
        }
        if let Some(error) = failure {
            if connection != Some(ConnectionState::Disconnected) {
                connection = Some(ConnectionState::Disconnected);
                if sender
                    .send(MarketEvent::Connection(ConnectionState::Disconnected))
                    .is_err()
                {
                    return;
                }
            }
            let code = error.code;
            if sender
                .send(MarketEvent::Failure {
                    code,
                    message: error.message,
                })
                .is_err()
                || code == tick_provider::exit_code::NOT_AUTHORIZED
            {
                return;
            }
        } else {
            connection = Some(ConnectionState::Connected);
            if sender
                .send(MarketEvent::Connection(ConnectionState::Connected))
                .is_err()
            {
                return;
            }
        }
    }
}
