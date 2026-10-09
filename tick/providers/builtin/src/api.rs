use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tick_provider::{
    api::{
        Candle, CandleAdjust, CandleHistoryRequest, CandlePeriod, CandleRequest, CandleSessions,
        CandleUpdate, ConsensusMetric, ConsensusPeriod, DateRange, MarketEvent, MarketState,
        MarketStatus, Quote, QuoteApi, QuoteUpdate, ReleaseSession, ReportCalendarPage,
        ReportCalendarRequest, ReportEvent, TradeApi, WatchlistGroup,
    },
    error::{Error, Result},
    news::{NewsApi, NewsArticle, NewsItem},
};
use time_tz::PrimitiveDateTimeExt;

struct DemoState {
    quotes: Vec<Quote>,
    candle_subscription: Option<CandleRequest>,
    tick: i64,
}

pub struct DemoApi {
    state: Arc<Mutex<DemoState>>,
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>,
    producer: tokio::task::JoinHandle<()>,
}

impl DemoApi {
    pub fn new() -> Result<Self> {
        let quotes = serde_json::from_str(r#"[
            {"symbol":"TSLA.US","name":"Demo · Tesla","latest_price":"248.5","previous_close":"240","open":"241.2","high":"250.6","low":"239.8","volume":72350000},
            {"symbol":"700.HK","name":"Demo · 腾讯控股","latest_price":"480.2","previous_close":"492","open":"490","high":"493.8","low":"478.6","volume":15840000},
            {"symbol":"AAPL.US","name":"Demo · Apple","latest_price":"225.6","previous_close":"222.1","open":"223.2","high":"227","low":"222.5","volume":45100000},
            {"symbol":"9988.HK","name":"Demo · 阿里巴巴","latest_price":"112.4","previous_close":"112.4","open":"112.8","high":"115","low":"111.6","volume":30500000},
            {"symbol":"NVDA.US","name":"Demo · NVIDIA","latest_price":"138.1","previous_close":"142.2","open":"140.6","high":"142.8","low":"136.7","volume":198000000}
        ]"#).map_err(|error| Error::usage(format!("invalid demo fixture: {error}")))?;
        let state = Arc::new(Mutex::new(DemoState {
            quotes,
            candle_subscription: None,
            tick: 0,
        }));
        let producer_state = Arc::clone(&state);
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let producer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sender.closed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => (),
                }
                let mut state = producer_state.lock().unwrap();
                state.tick += 1;
                let tick = state.tick;
                for (index, quote) in state.quotes.iter_mut().enumerate() {
                    let direction = if (tick + index as i64) % 4 < 2 { 1 } else { -1 };
                    quote.latest_price += Decimal::new(direction, 1);
                    quote.high = quote.high.max(quote.latest_price);
                    quote.low = quote.low.min(quote.latest_price);
                    quote.volume += 100;
                    quote.turnover = quote.latest_price * Decimal::from(quote.volume);
                    quote.updated_at = time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default();
                    let update = QuoteUpdate {
                        symbol: quote.symbol.clone(),
                        latest_price: quote.latest_price,
                        open: quote.open,
                        high: quote.high,
                        low: quote.low,
                        volume: quote.volume,
                        turnover: quote.turnover,
                        updated_at: quote.updated_at.clone(),
                    };
                    if sender.send(MarketEvent::Quote(update)).is_err() {
                        return;
                    }
                    if sender
                        .send(MarketEvent::QuoteSnapshot(quote.clone()))
                        .is_err()
                    {
                        return;
                    }
                }
                if let Some(request) = &state.candle_subscription {
                    let update = CandleUpdate {
                        symbol: request.symbol.clone(),
                        period: request.period,
                        adjust: request.adjust,
                        candles: demo_candles(request, tick),
                        replace: true,
                        requested_at: Some(tokio::time::Instant::now()),
                    };
                    if sender.send(MarketEvent::Candles(update)).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Self {
            state,
            events: Mutex::new(Some(events)),
            producer,
        })
    }
}

impl Drop for DemoApi {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

#[async_trait]
impl QuoteApi for DemoApi {
    fn source_name(&self) -> &'static str {
        crate::SOURCE
    }
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let state = self.state.lock().unwrap();
        Ok(symbols
            .iter()
            .map(|symbol| {
                state
                    .quotes
                    .iter()
                    .find(|quote| quote.symbol == *symbol)
                    .cloned()
                    .unwrap_or_else(|| Quote {
                        symbol: symbol.clone(),
                        error: Some("No demo fixture".into()),
                        ..Quote::default()
                    })
            })
            .collect())
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        Ok(vec![
            WatchlistGroup {
                id: 1,
                name: "Demo · US".into(),
                symbols: vec!["TSLA.US".into(), "AAPL.US".into(), "NVDA.US".into()],
            },
            WatchlistGroup {
                id: 2,
                name: "Demo · HK".into(),
                symbols: vec!["700.HK".into(), "9988.HK".into()],
            },
        ])
    }
    async fn market_states(&self, _: &[String]) -> Result<Vec<MarketState>> {
        Ok(vec![
            MarketState {
                market: "US".into(),
                status: MarketStatus::Open,
            },
            MarketState {
                market: "HK".into(),
                status: MarketStatus::Closed,
            },
        ])
    }
    async fn subscribe_quotes(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        self.events
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| Error::usage("demo event stream already taken"))
    }
    async fn candles(&self, request: &CandleRequest) -> Result<Vec<Candle>> {
        Ok(demo_candles(request, self.state.lock().unwrap().tick))
    }
    async fn subscribe_candles(&self, request: &CandleRequest) -> Result<bool> {
        self.state.lock().unwrap().candle_subscription = Some(request.clone());
        Ok(true)
    }
    async fn unsubscribe_candles(&self, symbol: &str, period: CandlePeriod) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state
            .candle_subscription
            .as_ref()
            .is_some_and(|request| request.symbol == symbol && request.period == period)
        {
            state.candle_subscription = None;
        }
        Ok(())
    }
    async fn consensus(&self, _: &str) -> Result<Vec<ConsensusPeriod>> {
        Ok(demo_consensus())
    }
    async fn report_calendar(&self, request: &ReportCalendarRequest) -> Result<ReportCalendarPage> {
        Ok(ReportCalendarPage {
            events: demo_report_events(&request.symbol)
                .into_iter()
                .filter(|event| {
                    event.date >= request.range.start && event.date <= request.range.end
                })
                .collect(),
            next_date: None,
        })
    }
    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        Ok(demo_history_candles(request))
    }
    async fn candles_by_date_with_sessions(
        &self,
        request: &CandleHistoryRequest,
        sessions: CandleSessions,
    ) -> Result<Vec<Candle>> {
        if sessions == CandleSessions::Regular
            || matches!(
                request.period,
                CandlePeriod::Day | CandlePeriod::Week | CandlePeriod::Month
            )
        {
            return self.candles_by_date(request).await;
        }
        Ok(demo_all_session_candles(request))
    }
    async fn trading_days(&self, _: &str, range: DateRange) -> Result<Vec<time::Date>> {
        Ok(demo_trading_dates(range))
    }
    async fn financial_statements(&self, _: &str) -> Result<Value> {
        let account = |field: &str, name: &str, percent: bool, base: i64, step: i64| {
            let values:Vec<_> = demo_quarters().enumerate().map(|(index,(year,quarter))| {
                let end=tick_provider::fiscal::fiscal_period_end(year,&quarter.to_string()).unwrap();
                json!({"year":year,"period":format!("Q{quarter}"),"fp_end":end.midnight().assume_utc().unix_timestamp(),"value":(base-index as i64*step).to_string()})
            }).collect();
            json!({"field":field,"name":name,"percent":percent,"values":values})
        };
        Ok(json!({
            "income":{"indicators":[{"currency":"USD","accounts":[account("gross_margin","DEMO gross margin (%)",true,42,1)]}]},
            "cash_flow":{"indicators":[{"currency":"USD","accounts":[account("operating_cash_flow","DEMO operating cash flow",false,3_400_000_000,100_000_000),account("capital_expenditure","DEMO capital expenditure",false,1_300_000_000,50_000_000)]}]}
        }))
    }
    async fn business_segments(&self, _: &str) -> Result<Value> {
        let historical:Vec<_>=demo_consensus().into_iter().map(|period| {
            let end=tick_provider::fiscal::fiscal_period_end(period.fiscal_year,&period.fiscal_period).unwrap();
            let revenue=period.revenue.actual.unwrap();
            json!({"date":tick_provider::dates::iso_date(end).replace('-',""),"currency":"USD","business":[
                {"name":"DEMO enterprise","value":(revenue*Decimal::new(6,1)).to_string(),"percent":"60"},
                {"name":"DEMO consumer","value":(revenue*Decimal::new(4,1)).to_string(),"percent":"40"}
            ]})
        }).collect();
        Ok(json!({"historical":historical}))
    }
    async fn filings(&self, _: &str) -> Result<Value> {
        Ok(
            json!([{"title":"DEMO · Quarterly earnings release (simulated)","publish_at":"2026-07-21T16:05:00-04:00","file_urls":[],"note":"Simulated filing metadata; no remote original is connected"}]),
        )
    }
}

/// One US session, in minutes, and the factor the demo applies for forward-adjusted prices.
const SESSION_MINUTES: i64 = 390;
const FORWARD_ADJUSTMENT: Decimal = Decimal::from_parts(101, 0, 0, false, 2);

const DEMO_EARNINGS_QUARTERS: usize = 8;
const DEMO_REPORT_LAG_DAYS: i64 = 21;

fn demo_quarters() -> impl Iterator<Item = (i32, u8)> {
    // Newest first, ending with the quarter before the demo's fixed anchor date.
    (0..DEMO_EARNINGS_QUARTERS as i32).map(|offset| {
        let index = 2026 * 4 + 1 - offset;
        (index.div_euclid(4), (index.rem_euclid(4) + 1) as u8)
    })
}

fn demo_consensus() -> Vec<ConsensusPeriod> {
    demo_quarters()
        .enumerate()
        .map(|(index, (year, quarter))| {
            let scale = Decimal::new(index as i64, 0);
            let estimate = Decimal::new(3200, 4) + scale * Decimal::new(120, 4);
            let actual = estimate + Decimal::new(if index % 3 == 0 { 450 } else { -260 }, 4);
            let revenue_estimate = Decimal::from(26_400_000_000_i64 - index as i64 * 700_000_000);
            let revenue_actual = revenue_estimate
                + Decimal::from(if index % 2 == 0 {
                    900_000_000
                } else {
                    -400_000_000
                });
            ConsensusPeriod {
                fiscal_year: year,
                fiscal_period: quarter.to_string(),
                label: format!("Q{quarter} {year}"),
                is_released: true,
                eps: ConsensusMetric {
                    estimate: Some(estimate),
                    actual: Some(actual),
                    surprise: Some(actual - estimate),
                },
                revenue: ConsensusMetric {
                    estimate: Some(revenue_estimate),
                    actual: Some(revenue_actual),
                    surprise: Some(revenue_actual - revenue_estimate),
                },
            }
        })
        .collect()
}

fn demo_report_events(symbol: &str) -> Vec<ReportEvent> {
    demo_quarters()
        .filter_map(|(year, quarter)| {
            let period_end = tick_provider::fiscal::fiscal_period_end(year, &quarter.to_string())?;
            Some(ReportEvent {
                symbol: symbol.to_owned(),
                date: period_end + time::Duration::days(DEMO_REPORT_LAG_DAYS),
                session: if quarter % 2 == 0 {
                    ReleaseSession::AfterHours
                } else {
                    ReleaseSession::PreMarket
                },
                fiscal_year: Some(year),
                fiscal_period: Some(quarter.to_string()),
                eps: ConsensusMetric::default(),
                revenue: ConsensusMetric::default(),
            })
        })
        .collect()
}

fn demo_trading_dates(range: DateRange) -> Vec<time::Date> {
    let mut dates = Vec::new();
    let mut date = range.start;
    while date <= range.end {
        if !matches!(
            date.weekday(),
            time::Weekday::Saturday | time::Weekday::Sunday
        ) {
            dates.push(date);
        }
        date += time::Duration::days(1);
    }
    dates
}

/// The demo history in the period and adjustment that were asked for, so a caller that requests
/// weekly forward-adjusted bars is never handed unadjusted daily ones under another label.
fn demo_history_candles(request: &CandleHistoryRequest) -> Vec<Candle> {
    let daily: Vec<Candle> = demo_trading_dates(request.range)
        .into_iter()
        .map(|date| {
            let mut candle = demo_daily_candle(date);
            candle.timestamp = demo_market_time(&request.symbol, date, 9, 30);
            candle
        })
        .collect();
    let in_period = match request.period {
        CandlePeriod::Day => daily,
        CandlePeriod::Week => merge_sessions(&daily, |date| (date.year(), date.iso_week())),
        CandlePeriod::Month => merge_sessions(&daily, |date| (date.year(), date.month() as u8)),
        period => split_sessions(&daily, period),
    };
    in_period
        .into_iter()
        .map(|candle| adjusted(candle, request.adjust))
        .collect()
}

fn demo_market_time(symbol: &str, date: time::Date, hour: u8, minute: u8) -> time::OffsetDateTime {
    let zone = if symbol.ends_with(".HK") {
        time_tz::timezones::db::asia::HONG_KONG
    } else {
        time_tz::timezones::db::america::NEW_YORK
    };
    date.with_hms(hour, minute, 0)
        .unwrap()
        .assume_timezone(zone)
        .unwrap()
}

fn demo_all_session_candles(request: &CandleHistoryRequest) -> Vec<Candle> {
    let minutes = period_minutes(request.period);
    let sessions: &[(i64, i64)] = if request.symbol.ends_with(".HK") {
        &[(570, 720), (780, 970)]
    } else {
        &[(240, 570), (570, 960), (960, 1200)]
    };
    let mut candles = Vec::new();
    for date in demo_trading_dates(request.range) {
        let daily = demo_daily_candle(date);
        for &(start, end) in sessions {
            let mut minute = start;
            while minute < end {
                let length = minutes.min(end - minute);
                let relative = if minute < 570 {
                    minute - 570
                } else {
                    minute - 960
                };
                let drift = Decimal::new(relative, 3);
                let open = daily.close + drift;
                let regular_close = if request.symbol.ends_with(".HK") {
                    970
                } else {
                    960
                };
                let close = if minute + length == regular_close {
                    daily.close
                } else {
                    open + Decimal::new((minute / length) % 7 - 3, 2)
                };
                let volume = if (570..960).contains(&minute) {
                    1000 * length
                } else {
                    200 * length
                };
                candles.push(adjusted(
                    Candle {
                        timestamp: demo_market_time(
                            &request.symbol,
                            date,
                            (minute / 60) as u8,
                            (minute % 60) as u8,
                        ),
                        open,
                        high: open.max(close) + Decimal::new(2, 2),
                        low: open.min(close) - Decimal::new(2, 2),
                        close,
                        volume,
                        turnover: close * Decimal::from(volume),
                    },
                    request.adjust,
                ));
                minute += length;
            }
        }
    }
    candles
}

/// Folds consecutive sessions that share a calendar key into one candle of the wider period.
fn merge_sessions<K: PartialEq>(daily: &[Candle], key: impl Fn(time::Date) -> K) -> Vec<Candle> {
    let mut periods: Vec<(K, Candle)> = Vec::new();
    for session in daily {
        let session_key = key(session.timestamp.date());
        match periods.last_mut() {
            Some((open_key, candle)) if *open_key == session_key => {
                candle.high = candle.high.max(session.high);
                candle.low = candle.low.min(session.low);
                candle.close = session.close;
                candle.volume += session.volume;
                candle.turnover += session.turnover;
                candle.timestamp = session.timestamp;
            }
            _ => periods.push((session_key, session.clone())),
        }
    }
    periods.into_iter().map(|(_, candle)| candle).collect()
}

/// Cuts each session into the intraday slots its period asks for, shifted so that the slots of
/// one day are not identical copies.
fn split_sessions(daily: &[Candle], period: CandlePeriod) -> Vec<Candle> {
    let minutes = period_minutes(period);
    let slots = (SESSION_MINUTES / minutes).max(1);
    daily
        .iter()
        .flat_map(|session| {
            (0..slots).map(move |slot| {
                let drift = Decimal::new(slot % 7 - 3, 1);
                Candle {
                    timestamp: session.timestamp + time::Duration::minutes(slot * minutes),
                    open: session.open + drift,
                    high: session.high + drift,
                    low: session.low + drift,
                    close: session.close + drift,
                    volume: session.volume / slots,
                    turnover: session.turnover / Decimal::from(slots),
                }
            })
        })
        .collect()
}

fn adjusted(candle: Candle, adjust: CandleAdjust) -> Candle {
    match adjust {
        CandleAdjust::None => candle,
        CandleAdjust::Forward => {
            let scale = |price: Decimal| (price * FORWARD_ADJUSTMENT).round_dp(3);
            Candle {
                open: scale(candle.open),
                high: scale(candle.high),
                low: scale(candle.low),
                close: scale(candle.close),
                ..candle
            }
        }
    }
}

fn demo_daily_candle(date: time::Date) -> Candle {
    // A period that is coprime with the five- and twenty-session offsets, so the demo columns differ.
    let wave = i64::from(date.to_julian_day() % 37) - 18;
    let close = Decimal::new(2400 + wave * 17, 1);
    Candle {
        timestamp: date.midnight().assume_utc(),
        open: close - Decimal::new(8, 1),
        high: close + Decimal::new(14, 1),
        low: close - Decimal::new(16, 1),
        close,
        volume: 1_000_000 + wave.unsigned_abs() as i64 * 10_000,
        turnover: close * Decimal::from(1_000_000),
    }
}

#[async_trait]
impl TradeApi for DemoApi {
    async fn positions(&self) -> Result<Value> {
        Ok(
            json!({"list":[{"account_channel":"lb_papertrading","stock_info":[
                {"symbol":"TSLA.US","symbol_name":"Demo · Tesla","quantity":"120","available_quantity":"100","cost_price":"232.5","currency":"USD"},
                {"symbol":"700.HK","symbol_name":"Demo · 腾讯控股","quantity":"500","available_quantity":"500","cost_price":"500","currency":"HKD"},
                {"symbol":"AAPL.US","symbol_name":"Demo · Apple","quantity":"80","available_quantity":"80","cost_price":"225.6","currency":"USD"}
            ]}]}),
        )
    }
}

#[async_trait]
impl NewsApi for DemoApi {
    async fn news(&self, _: Option<&str>) -> Result<Vec<NewsItem>> {
        Ok(vec![
            NewsItem {
                id: "1".into(),
                title: "Demo · 公司发布季度业绩，投资者关注现金流变化".into(),
                source: "示例资讯".into(),
                published_at: "2026-09-10T09:30:00Z".into(),
                url: "https://example.com/1".into(),
            },
            NewsItem {
                id: "2".into(),
                title: "Demo · Market recap and earnings calendar".into(),
                source: "Demo News".into(),
                published_at: "2026-09-10T08:00:00Z".into(),
                url: "https://example.com/2".into(),
            },
        ])
    }
    async fn article(&self, id: &str) -> Result<NewsArticle> {
        let item = self
            .news(None)
            .await?
            .into_iter()
            .find(|item| item.id == id)
            .ok_or_else(|| Error::usage("unknown demo article"))?;
        Ok(NewsArticle { id: item.id, title: item.title, source: item.source, published_at: item.published_at, url: item.url,
            body: "# 示例资讯正文\n\n这是用于检查有限 Panel 中换行与滚动的模拟内容，不代表真实市场消息。\n\n## 观察要点\n\n- 收入和现金流的变化需要分别核对。\n- 中文段落按照窗口宽度换行，缩小窗口后仍可阅读。\n- 使用 j/k、方向键和 PageDown 滚动，Esc 返回列表。\n\n[示例链接](https://example.com)\n\n**TUI demo** 会使用正式命令相同的界面与操作。".into() })
    }
}

fn period_minutes(period: CandlePeriod) -> i64 {
    match period {
        CandlePeriod::Day => 1440,
        CandlePeriod::Week => 10080,
        CandlePeriod::Month => 43200,
        CandlePeriod::OneMinute => 1,
        CandlePeriod::FiveMinute => 5,
        CandlePeriod::FifteenMinute => 15,
        CandlePeriod::ThirtyMinute => 30,
        CandlePeriod::SixtyMinute => 60,
    }
}

fn demo_candles(request: &CandleRequest, tick: i64) -> Vec<Candle> {
    let minutes = period_minutes(request.period);
    let end = time::macros::datetime!(2026-09-10 09:30 UTC);
    (0..request.count)
        .map(|index| {
            let wave = ((index % 18) as i64 - 9).abs();
            let open = Decimal::new(4700 + (index as i64 % 50) * 2 + wave * 8, 1);
            let mut close = open + Decimal::new(if index % 3 == 0 { -18 } else { 12 }, 1);
            if index + 1 == request.count {
                close += Decimal::new(tick % 10, 1);
            }
            let volume = 1_000_000 + (index as i64 % 17) * 100_000;
            Candle {
                timestamp: end
                    - time::Duration::minutes((request.count - index - 1) as i64 * minutes),
                open,
                high: open.max(close) + Decimal::new(8, 1),
                low: open.min(close) - Decimal::new(6, 1),
                close,
                volume,
                turnover: close * Decimal::from(volume),
            }
        })
        .collect()
}
