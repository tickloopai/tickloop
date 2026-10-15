use std::{collections::VecDeque, sync::Mutex, time::Duration};

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use tick_market::{
    args::{KlineArgs, NewsArgs, OnceArgs, QuoteArgs},
    commands::{kline, news, quote},
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    api::{
        Candle, CandleAdjust, CandlePeriod, CandleRequest, ConnectionState, MarketEvent, Quote,
        QuoteApi,
    },
    error::{Error, Result},
    news::{NewsApi, NewsArticle, NewsItem},
};
use tick_sdk::stream::TickStream;

struct News {
    responses: Mutex<VecDeque<Result<Vec<NewsItem>>>>,
    calls: Mutex<usize>,
    hang_on_call: Option<usize>,
}

#[async_trait]
impl NewsApi for News {
    async fn news(&self, _: Option<&str>) -> Result<Vec<NewsItem>> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if self.hang_on_call == Some(call) {
            return std::future::pending().await;
        }
        self.responses.lock().unwrap().pop_front().unwrap()
    }

    async fn article(&self, _: &str) -> Result<NewsArticle> {
        panic!("this script does not open an article")
    }
}

#[async_trait]
impl QuoteApi for News {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        panic!("news must not request quotes")
    }
}

fn items(title: &str) -> Vec<NewsItem> {
    serde_json::from_value(json!([{
        "id": "1", "title": title, "source": "Source",
        "published_at": "2026-09-10T12:00:00Z", "url": "https://example.com/1"
    }]))
    .unwrap()
}

fn arguments() -> NewsArgs {
    NewsArgs {
        symbol: None,
        limit: 10,
        output: OnceArgs::default(),
    }
}

fn key(code: KeyCode) -> std::io::Result<Event> {
    Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn records(events: TickStream) -> Vec<Value> {
    events.records().all()
}

#[tokio::test(start_paused = true)]
async fn news_network_failure_keeps_the_view_alive_and_next_refresh_publishes_recovered_content() {
    for quit_after in [31, 61] {
        let api = News {
            responses: Mutex::new(VecDeque::from([
                Ok(items("Initial headline")),
                Err(Error::network("network unavailable")),
                Ok(items("Recovered headline")),
            ])),
            calls: Mutex::new(0),
            hang_on_call: None,
        };
        let input = futures_util::stream::iter([
            (Duration::from_secs(1), KeyCode::Down),
            (Duration::from_secs(quit_after - 1), KeyCode::Char('q')),
        ])
        .then(|(delay, code)| async move {
            tokio::time::sleep(delay).await;
            key(code)
        })
        .boxed();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut events = TickStream::in_memory(TickKind::News);
        news::run_live(
            &arguments(),
            &api,
            &api,
            &Settings::default(),
            &mut terminal,
            input,
            &mut events,
        )
        .await
        .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let records = records(events);
        if quit_after == 31 {
            assert_eq!(*api.calls.lock().unwrap(), 2);
            assert!(screen.contains("Initial headline"));
            assert!(screen.contains("Unavailable"));
            assert_eq!(records.len(), 1);
        } else {
            assert_eq!(*api.calls.lock().unwrap(), 3);
            assert!(screen.contains("Recovered headline"));
            assert!(screen.contains("Data received"));
            assert_eq!(records.len(), 2);
            assert_eq!(
                records[1]["data"]["items"][0]["title"],
                "Recovered headline"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn refresh_authentication_and_usage_errors_exit_instead_of_retrying() {
    for code in [3, 2] {
        let api = News {
            responses: Mutex::new(VecDeque::from([
                Ok(items("Initial headline")),
                Err(Error {
                    code,
                    message: "request rejected".into(),
                }),
            ])),
            calls: Mutex::new(0),
            hang_on_call: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut events = TickStream::in_memory(TickKind::News);
        let error = news::run_live(
            &arguments(),
            &api,
            &api,
            &Settings::default(),
            &mut terminal,
            futures_util::stream::pending(),
            &mut events,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, code);
        if code == 3 {
            assert!(error.message.contains("tick longbridge auth"));
        }
        assert_eq!(*api.calls.lock().unwrap(), 2);
        assert_eq!(records(events).len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_hanging_refresh_times_out_and_the_next_cycle_recovers() {
    let api = News {
        responses: Mutex::new(VecDeque::from([
            Ok(items("Initial headline")),
            Ok(items("Recovered headline")),
        ])),
        calls: Mutex::new(0),
        hang_on_call: Some(2),
    };
    let input = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_secs(61)).await;
        key(KeyCode::Char('q'))
    })
    .boxed();
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    let mut events = TickStream::in_memory(TickKind::News);
    news::run_live(
        &arguments(),
        &api,
        &api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut events,
    )
    .await
    .unwrap();
    assert_eq!(*api.calls.lock().unwrap(), 3);
    let records = records(events);
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1]["data"]["items"][0]["title"],
        "Recovered headline"
    );
}

#[tokio::test(start_paused = true)]
async fn a_hanging_retry_shows_recovering_and_remains_cancellable() {
    let api = News {
        responses: Mutex::new(VecDeque::from([
            Ok(items("Initial headline")),
            Err(Error::network("offline")),
        ])),
        calls: Mutex::new(0),
        hang_on_call: Some(3),
    };
    let input = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_secs(61)).await;
        key(KeyCode::Char('q'))
    })
    .boxed();
    let start = tokio::time::Instant::now();
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    let mut events = TickStream::in_memory(TickKind::News);
    news::run_live(
        &arguments(),
        &api,
        &api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut events,
    )
    .await
    .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(61));
    assert_eq!(*api.calls.lock().unwrap(), 3);
    assert_eq!(records(events).len(), 1);
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("Initial headline"));
    assert!(screen.contains("Recovering"));
}

struct Quotes {
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>,
    unsubscribed: Mutex<usize>,
}

#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Ok(serde_json::from_value(json!([
            {"symbol": "AAPL.US", "name": "Apple", "latest_price": "100", "previous_close": "90"}
        ]))
        .unwrap())
    }

    async fn subscribe_quotes(&self, _: &[String]) -> Result<()> {
        Ok(())
    }

    async fn unsubscribe_quotes(&self, _: &[String]) -> Result<()> {
        *self.unsubscribed.lock().unwrap() += 1;
        Ok(())
    }

    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

#[tokio::test(start_paused = true)]
async fn market_failure_recovers_on_connection_or_data_without_status_ticks() {
    let price_update = MarketEvent::Quote(
        serde_json::from_value(json!({
            "symbol": "AAPL.US", "latest_price": "120", "open": "95", "high": "120",
            "low": "90", "volume": 10, "turnover": "1200", "updated_at": "2026-09-10T12:00:00Z"
        }))
        .unwrap(),
    );
    for recovery in [
        None,
        Some(MarketEvent::Connection(ConnectionState::Connected)),
        Some(price_update),
    ] {
        let (push, market) = tokio::sync::mpsc::unbounded_channel();
        let api = Quotes {
            events: Mutex::new(Some(market)),
            unsubscribed: Mutex::new(0),
        };
        let script_push = push.clone();
        let expects_update = matches!(recovery, Some(MarketEvent::Quote(_)));
        let expects_recovery = recovery.is_some();
        let input = futures_util::stream::once(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            script_push
                .send(MarketEvent::Failure {
                    code: 4,
                    message: "offline".into(),
                })
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(recovery) = recovery {
                script_push
                    .send(MarketEvent::Connection(ConnectionState::Reconnecting))
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
                script_push.send(recovery).unwrap();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            key(KeyCode::Char('q'))
        })
        .boxed();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut events = TickStream::in_memory(TickKind::Quote);
        quote::run_live(
            &QuoteArgs {
                symbols: vec!["AAPL.US".into()],
                output: OnceArgs::default(),
            },
            &api,
            &Settings::default(),
            &mut terminal,
            input,
            &mut events,
        )
        .await
        .unwrap();
        let records = records(events);
        assert_eq!(records.len(), if expects_update { 2 } else { 1 });
        if expects_update {
            assert_eq!(records[1]["data"][0]["latest_price"], "120");
        }
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains(if expects_recovery {
            "Data received"
        } else {
            "Unavailable"
        }));
        assert_eq!(*api.unsubscribed.lock().unwrap(), 1);
        drop(push);
    }
}

#[tokio::test(start_paused = true)]
async fn market_authentication_failure_exits_with_auth_guidance_and_cleans_up_subscription() {
    let (push, market) = tokio::sync::mpsc::unbounded_channel();
    let api = Quotes {
        events: Mutex::new(Some(market)),
        unsubscribed: Mutex::new(0),
    };
    push.send(MarketEvent::Failure {
        code: 3,
        message: "token expired".into(),
    })
    .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    let mut events = TickStream::in_memory(TickKind::Quote);
    let error = quote::run_live(
        &QuoteArgs {
            symbols: vec!["AAPL.US".into()],
            output: OnceArgs::default(),
        },
        &api,
        &Settings::default(),
        &mut terminal,
        futures_util::stream::pending(),
        &mut events,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, 3);
    assert!(error.message.contains("tick longbridge auth"));
    assert_eq!(*api.unsubscribed.lock().unwrap(), 1);
    assert_eq!(records(events).len(), 1);
}

struct SwitchingHistory {
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>,
    calls: Mutex<Vec<CandlePeriod>>,
    fail_first_refresh: bool,
}

#[async_trait]
impl QuoteApi for SwitchingHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        panic!("candles only")
    }

    async fn candles(&self, request: &CandleRequest) -> Result<Vec<Candle>> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(request.period);
        if self.fail_first_refresh && calls.len() == 2 {
            return Err(Error::network("history remains unavailable"));
        }
        let close = match calls.len() {
            1 => "100",
            2 => "101",
            3 => "102",
            _ => "103",
        };
        Ok(serde_json::from_value(json!([{
            "timestamp": "2026-09-10T12:00:00Z", "open": "100", "high": "110", "low": "90",
            "close": close, "volume": 10, "turnover": "1000"
        }]))
        .unwrap())
    }

    async fn subscribe_candles(&self, request: &CandleRequest) -> Result<bool> {
        Ok(request.period == CandlePeriod::Day)
    }

    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

#[tokio::test(start_paused = true)]
async fn switching_candle_periods_keeps_periodic_history_correction_with_push_available() {
    let (_push, market) = tokio::sync::mpsc::unbounded_channel();
    let api = SwitchingHistory {
        events: Mutex::new(Some(market)),
        calls: Mutex::new(Vec::new()),
        fail_first_refresh: false,
    };
    let input = futures_util::stream::iter([
        (1, KeyCode::Char('5')),
        (32, KeyCode::Char('1')),
        (31, KeyCode::Char('q')),
    ])
    .then(|(delay, code)| async move {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        key(code)
    })
    .boxed();
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let mut events = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &KlineArgs {
            symbol: "AAPL.US".into(),
            period: CandlePeriod::Day,
            count: Some(1),
            adjust: CandleAdjust::None,
            event: Default::default(),
            output: OnceArgs::default(),
        },
        &api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut events,
    )
    .await
    .unwrap();
    assert_eq!(
        *api.calls.lock().unwrap(),
        [
            CandlePeriod::Day,
            CandlePeriod::FiveMinute,
            CandlePeriod::FiveMinute,
            CandlePeriod::Day,
            CandlePeriod::Day
        ]
    );
    let records = records(events);
    assert_eq!(records.len(), 4);
    assert_eq!(records[2]["data"]["latest"]["close"], "102");
}

#[tokio::test(start_paused = true)]
async fn connection_recovery_backfills_candles_and_healthy_probes_do_not_repeat_it() {
    for fail_first_refresh in [false, true] {
        let (push, market) = tokio::sync::mpsc::unbounded_channel();
        let api = SwitchingHistory {
            events: Mutex::new(Some(market)),
            calls: Mutex::new(Vec::new()),
            fail_first_refresh,
        };
        let script_push = push.clone();
        let input = futures_util::stream::once(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            script_push
                .send(MarketEvent::Failure {
                    code: 4,
                    message: "offline".into(),
                })
                .unwrap();
            script_push
                .send(MarketEvent::QuoteSnapshot(
                    serde_json::from_value(json!({
                        "symbol": "AAPL.US", "latest_price": "100", "previous_close": "90"
                    }))
                    .unwrap(),
                ))
                .unwrap();
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                script_push
                    .send(MarketEvent::Connection(ConnectionState::Connected))
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            key(KeyCode::Char('q'))
        })
        .boxed();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut events = TickStream::in_memory(TickKind::Kline);
        kline::run_live(
            &KlineArgs {
                symbol: "AAPL.US".into(),
                period: CandlePeriod::Day,
                count: Some(1),
                adjust: CandleAdjust::None,
                event: Default::default(),
                output: OnceArgs::default(),
            },
            &api,
            &Settings::default(),
            &mut terminal,
            input,
            &mut events,
        )
        .await
        .unwrap();
        assert_eq!(
            api.calls.lock().unwrap().len(),
            if fail_first_refresh { 3 } else { 2 }
        );
        let records = records(events);
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[1]["data"]["latest"]["close"],
            if fail_first_refresh { "102" } else { "101" }
        );
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Data received"));
        drop(push);
    }
}

struct ArticleNews {
    error_code: u8,
    article_calls: Mutex<usize>,
}

#[async_trait]
impl NewsApi for ArticleNews {
    async fn news(&self, _: Option<&str>) -> Result<Vec<NewsItem>> {
        Ok(items("Initial headline"))
    }

    async fn article(&self, id: &str) -> Result<NewsArticle> {
        let mut calls = self.article_calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            return Err(Error {
                code: self.error_code,
                message: "article request failed".into(),
            });
        }
        Ok(NewsArticle {
            id: id.into(),
            title: "Recovered article".into(),
            source: "Source".into(),
            published_at: "2026-09-10T12:00:00Z".into(),
            url: "https://example.com/1".into(),
            body: "Recovered article body".into(),
        })
    }
}

#[async_trait]
impl QuoteApi for ArticleNews {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        panic!("news must not request quotes")
    }
}

#[tokio::test(start_paused = true)]
async fn article_network_failure_keeps_the_list_and_enter_can_retry_without_ui_ticks() {
    for retry in [false, true] {
        let api = ArticleNews {
            error_code: 4,
            article_calls: Mutex::new(0),
        };
        let mut keys = vec![KeyCode::Enter, KeyCode::Down];
        if retry {
            keys.push(KeyCode::Enter);
        }
        keys.push(KeyCode::Char('q'));
        let input = futures_util::stream::iter(keys)
            .then(|code| async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                key(code)
            })
            .boxed();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut events = TickStream::in_memory(TickKind::News);
        news::run_live(
            &arguments(),
            &api,
            &api,
            &Settings::default(),
            &mut terminal,
            input,
            &mut events,
        )
        .await
        .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let records = records(events);
        if retry {
            assert_eq!(*api.article_calls.lock().unwrap(), 2);
            assert!(screen.contains("Recovered article body"));
            assert!(screen.contains("Data received"));
            assert_eq!(records.len(), 2);
            assert_eq!(
                records[1]["data"]["articles"]["1"]["body"],
                "Recovered article body"
            );
        } else {
            assert_eq!(*api.article_calls.lock().unwrap(), 1);
            assert!(screen.contains("Initial headline"));
            assert!(screen.contains("Unavailable"));
            assert_eq!(records.len(), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn article_authentication_and_usage_errors_remain_fatal() {
    for code in [2, 3] {
        let api = ArticleNews {
            error_code: code,
            article_calls: Mutex::new(0),
        };
        let input = futures_util::stream::once(async { key(KeyCode::Enter) })
            .chain(futures_util::stream::pending())
            .boxed();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut events = TickStream::in_memory(TickKind::News);
        let error = news::run_live(
            &arguments(),
            &api,
            &api,
            &Settings::default(),
            &mut terminal,
            input,
            &mut events,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, code);
        if code == 3 {
            assert!(error.message.contains("tick longbridge auth"));
        }
        assert_eq!(*api.article_calls.lock().unwrap(), 1);
        assert_eq!(records(events).len(), 1);
    }
}
