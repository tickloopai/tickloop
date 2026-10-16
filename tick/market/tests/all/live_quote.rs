use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Mutex;
use tick_market::{
    args::{OnceArgs, QuoteArgs},
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    api::{MarketEvent, Quote, QuoteApi, QuoteUpdate},
    error::Result,
};
use tick_sdk::stream::TickStream;

struct Quotes {
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>,
    subscriptions: Mutex<Vec<Vec<String>>>,
    hang_cleanup: bool,
}

#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Ok(serde_json::from_str(
            r#"[{"symbol":"TSLA.US","name":"Tesla","latest_price":"100","previous_close":"90"}]"#,
        )
        .unwrap())
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.subscriptions.lock().unwrap().push(symbols.to_vec());
        Ok(())
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        if self.hang_cleanup {
            return std::future::pending().await;
        }
        assert_eq!(symbols, &["TSLA.US"]);
        self.subscriptions.lock().unwrap().clear();
        Ok(())
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

#[tokio::test]
async fn quote_view_redraws_after_a_push_and_unsubscribes_when_quitting() {
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = Quotes {
        events: Mutex::new(Some(events)),
        subscriptions: Mutex::new(Vec::new()),
        hang_cleanup: false,
    };
    let (keys, key_events) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let update: QuoteUpdate = serde_json::from_str(r#"{"symbol":"TSLA.US","latest_price":"123.45","open":"101","high":"124","low":"99","volume":9000,"turnover":"1110000","updated_at":"2026-09-10T14:00:00Z"}"#).unwrap();
        push.send(MarketEvent::Quote(update.clone())).unwrap();
        push.send(MarketEvent::Quote(update.clone())).unwrap();
        let mut spike = update.clone();
        spike.latest_price = "150".parse().unwrap();
        push.send(MarketEvent::Quote(spike)).unwrap();
        push.send(MarketEvent::Quote(update.clone())).unwrap();
        let mut old = update;
        old.latest_price = "1".parse().unwrap();
        old.updated_at = "2020-01-01T00:00:00Z".into();
        push.send(MarketEvent::Quote(old)).unwrap();
        for key in [
            KeyCode::Char('?'),
            KeyCode::Backspace,
            KeyCode::Down,
            KeyCode::Up,
        ] {
            keys.send(Ok(Event::Key(KeyEvent::new(key, KeyModifiers::NONE))))
                .unwrap();
        }
        keys.send(Ok(Event::Resize(100, 20))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs::default(),
    };
    let mut updates = TickStream::in_memory(TickKind::Quote);
    tick_market::commands::quote::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(key_events),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let records: Vec<serde_json::Value> = updates.records().all();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0]["seq"], 1);
    assert_eq!(records[1]["seq"], 2);
    assert_eq!(records[1]["data"][0]["latest_price"], "123.45");
    assert_eq!(records[2]["data"][0]["latest_price"], "150");
    assert_eq!(records[3]["data"][0]["latest_price"], "123.45");
    assert_eq!(records[3]["seq"], 4);
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("123.45"), "{screen}");
    assert!(screen.contains("TSLA.US"), "{screen}");
    assert!(
        !screen.contains("Keys ·"),
        "Backspace closed the help: {screen}"
    );
    assert!(api.subscriptions.lock().unwrap().is_empty());
}

struct HangingQuotes;
#[async_trait]
impl QuoteApi for HangingQuotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn quit_cancels_initial_network_wait() {
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs::default(),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))]);
    let mut updates = TickStream::in_memory(TickKind::Quote);
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        tick_market::commands::quote::run_live(
            &args,
            &HangingQuotes,
            &Settings::default(),
            &mut terminal,
            input,
            &mut updates,
        ),
    )
    .await
    .expect("quit must cancel initialization")
    .unwrap();
    assert!(updates.records().is_empty());
}

#[tokio::test(start_paused = true)]
async fn quitting_does_not_wait_forever_for_unsubscribe() {
    let (_push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = Quotes {
        events: Mutex::new(Some(events)),
        subscriptions: Mutex::new(Vec::new()),
        hang_cleanup: true,
    };
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs::default(),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))]);
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tick_market::commands::quote::run_live(
            &args,
            &api,
            &Settings::default(),
            &mut terminal,
            input,
            &mut updates,
        ),
    )
    .await
    .expect("cleanup must be bounded")
    .unwrap_err();
    assert_eq!(error.code, 4);
    assert!(error.message.contains("cleanup"));
}

#[tokio::test]
async fn quote_scrolls_a_small_panel_without_losing_content() {
    let (_push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = Quotes {
        events: Mutex::new(Some(events)),
        subscriptions: Mutex::new(Vec::new()),
        hang_cleanup: false,
    };
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs::default(),
    };
    let mut terminal = Terminal::new(TestBackend::new(22, 4)).unwrap();
    let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))]);
    let mut updates = TickStream::in_memory(TickKind::Quote);
    tick_market::commands::quote::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut updates,
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
    assert!(screen.contains("TSLA.US"), "{screen}");
    assert!(
        screen.contains("━━─") && screen.contains("─ ↕ 1–3/6"),
        "{screen}"
    );
    let record: serde_json::Value = updates.records().all().remove(0);
    assert_eq!(record["data"][0]["symbol"], "TSLA.US");
}

#[tokio::test]
async fn minimum_quote_panel_keeps_symbol_price_and_percentage_visible() {
    let (_push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = Quotes {
        events: Mutex::new(Some(events)),
        subscriptions: Mutex::new(Vec::new()),
        hang_cleanup: false,
    };
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs::default(),
    };
    let minimum = TickKind::Quote.minimum_panel_size();
    let mut terminal = Terminal::new(TestBackend::new(minimum.columns, minimum.rows)).unwrap();
    let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))]);
    let mut updates = TickStream::in_memory(TickKind::Quote);
    tick_market::commands::quote::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut updates,
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
    assert!(
        screen.contains("TSLA.US")
            && screen.contains("Tesla")
            && screen.contains("100")
            && screen.contains("11.11%")
            && !screen.contains("scroll"),
        "{screen}"
    );
}

struct RolloverQuotes(Quotes);
#[async_trait]
impl QuoteApi for RolloverQuotes {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let mut quotes = self.0.quotes(symbols).await?;
        quotes[0].updated_at = "2026-09-10T20:00:00Z".into();
        Ok(quotes)
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.0.subscribe_quotes(symbols).await
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.0.unsubscribe_quotes(symbols).await
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        self.0.take_events().await
    }
}

#[tokio::test]
async fn authoritative_previous_close_refresh_does_not_replace_newer_prices_or_restore_an_old_basis()
 {
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = RolloverQuotes(Quotes {
        events: Mutex::new(Some(events)),
        subscriptions: Mutex::new(Vec::new()),
        hang_cleanup: false,
    });
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut update: QuoteUpdate = serde_json::from_str(r#"{"symbol":"TSLA.US","latest_price":"110","open":"105","high":"110","low":"105","volume":500,"turnover":"55000","updated_at":"2026-09-11T13:30:00Z"}"#).unwrap();
        push.send(MarketEvent::Quote(update.clone())).unwrap();
        let basis: Quote = serde_json::from_str(r#"{"symbol":"TSLA.US","latest_price":"105","previous_close":"100","updated_at":"2026-09-11T13:29:59Z"}"#).unwrap();
        push.send(MarketEvent::QuoteSnapshot(basis.clone()))
            .unwrap();
        update.latest_price = "120".parse().unwrap();
        update.high = "120".parse().unwrap();
        push.send(MarketEvent::Quote(update)).unwrap();
        let old: Quote = serde_json::from_str(r#"{"symbol":"TSLA.US","latest_price":"100","previous_close":"90","updated_at":"2026-09-10T20:00:00Z"}"#).unwrap();
        push.send(MarketEvent::QuoteSnapshot(old)).unwrap();
        let same_second = Quote {
            latest_price: "115".parse().unwrap(),
            updated_at: "2026-09-11T13:30:00Z".into(),
            ..basis
        };
        push.send(MarketEvent::QuoteSnapshot(same_second.clone()))
            .unwrap();
        push.send(MarketEvent::QuoteSnapshot(same_second)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs::default(),
    };
    let mut updates = TickStream::in_memory(TickKind::Quote);
    tick_market::commands::quote::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let records: Vec<serde_json::Value> = updates.records().all();
    assert_eq!(
        records.len(),
        4,
        "old snapshots and duplicate observations are not content updates"
    );
    assert!(records[1]["data"][0]["change"].is_null());
    assert!(records[1]["data"][0]["change_percent"].is_null());
    assert_eq!(records[2]["data"][0]["latest_price"], "110");
    assert_eq!(records[2]["data"][0]["previous_close"], "100");
    assert_eq!(records[2]["data"][0]["change_percent"], "10");
    assert_eq!(records[3]["data"][0]["latest_price"], "120");
    assert_eq!(records[3]["data"][0]["change_percent"], "20");
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("120.00"), "{screen}");
    assert!(screen.contains("+20.00%"), "{screen}");
}

#[tokio::test]
async fn recovered_snapshot_updates_the_last_trade_without_waiting_for_another_push() {
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = RolloverQuotes(Quotes {
        events: Mutex::new(Some(events)),
        subscriptions: Mutex::new(Vec::new()),
        hang_cleanup: false,
    });
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        push.send(MarketEvent::Failure {
            code: 4,
            message: "offline".into(),
        })
        .unwrap();
        let snapshot: Quote = serde_json::from_str(r#"{"symbol":"TSLA.US","latest_price":"130","previous_close":"100","open":"110","high":"131","low":"109","volume":700,"turnover":"91000","updated_at":"2026-09-11T20:00:00Z"}"#).unwrap();
        push.send(MarketEvent::QuoteSnapshot(snapshot.clone()))
            .unwrap();
        push.send(MarketEvent::Connection(
            tick_provider::api::ConnectionState::Connected,
        ))
        .unwrap();
        push.send(MarketEvent::QuoteSnapshot(snapshot)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    tick_market::commands::quote::run_live(
        &QuoteArgs {
            symbols: vec!["TSLA".into()],
            output: OnceArgs::default(),
        },
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let records: Vec<serde_json::Value> = updates.records().all();
    assert_eq!(
        records.len(),
        2,
        "recovery publishes exactly one new business snapshot"
    );
    assert_eq!(records[1]["data"][0]["latest_price"], "130");
    assert_eq!(records[1]["data"][0]["change_percent"], "30");
    assert_eq!(records[1]["data"][0]["volume"], 700);
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("130.00"), "{screen}");
}

/// Its host waits only briefly for a launched Tick to answer, while the first quotes may take
/// longer: a quote Tick still waiting on them is already described and reads as no data yet.
#[tokio::test]
async fn a_quote_tick_waiting_on_its_first_quotes_already_answers_describe_and_read() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (mut host_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, host_responses) = tokio::io::duplex(64 * 1024);
    let mut session = TickStream::duplex(TickKind::Quote, tick_requests, tick_responses);
    let tick = tokio::spawn(async move {
        let args = QuoteArgs {
            symbols: vec!["TSLA.US".into()],
            output: OnceArgs::default(),
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        tick_market::commands::quote::run_live(
            &args,
            &HangingQuotes,
            &Settings::default(),
            &mut terminal,
            futures_util::stream::pending(),
            &mut session,
        )
        .await
    });
    let mut answers = tokio::io::BufReader::new(host_responses).lines();
    for request in [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tick/describe"}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tick/read"}),
    ] {
        host_requests
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
    }
    let mut answer = async || {
        let line = tokio::time::timeout(std::time::Duration::from_secs(1), answers.next_line())
            .await
            .expect("answered while the first quotes are pending")
            .unwrap()
            .unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let described = answer().await;
    assert_eq!(described["id"], 1);
    assert_eq!(described["result"]["kind"], "quote");
    let read = answer().await;
    assert_eq!(read["id"], 2);
    assert_eq!(read["result"], serde_json::json!({"seq": 0, "data": null}));
    tick.abort();
}

/// Quotes that answer the first request and leave every later one running, with a market feed
/// the test drives.
struct LaterQuotesRunning {
    requests: std::sync::atomic::AtomicUsize,
    later_request: tokio::sync::Notify,
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>,
}

#[async_trait]
impl QuoteApi for LaterQuotesRunning {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        if self
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.later_request.notify_one();
            return std::future::pending().await;
        }
        Ok(serde_json::from_str(
            r#"[{"symbol":"TSLA.US","name":"Tesla","latest_price":"100","previous_close":"90"}]"#,
        )
        .unwrap())
    }
    async fn subscribe_quotes(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

/// A view that fails, here because the login was refused, still answers the Tick Tool calls it
/// was running instead of leaving its host to wait them out.
#[tokio::test]
async fn a_failing_view_answers_the_tick_tool_calls_it_was_running() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (pushes, events) = tokio::sync::mpsc::unbounded_channel();
    let api = LaterQuotesRunning {
        requests: Default::default(),
        later_request: Default::default(),
        events: Mutex::new(Some(events)),
    };
    let (mut host_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, host_responses) = tokio::io::duplex(64 * 1024);
    let mut session = TickStream::duplex(TickKind::Quote, tick_requests, tick_responses);
    let args = QuoteArgs {
        symbols: vec!["TSLA.US".into()],
        output: OnceArgs::default(),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let settings = Settings::default();
    let tick = tick_market::commands::quote::run_live(
        &args,
        &api,
        &settings,
        &mut terminal,
        futures_util::stream::pending(),
        &mut session,
    );
    let host = async {
        let mut frames = tokio::io::BufReader::new(host_responses).lines();
        let mut next = async || {
            let line = tokio::time::timeout(std::time::Duration::from_secs(1), frames.next_line())
                .await
                .expect("a frame within a second")
                .unwrap()
                .unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        };
        let content = next().await;
        assert_eq!(content["method"], "tick/update", "{content}");
        let describe = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tick/describe"});
        host_requests
            .write_all(format!("{describe}\n").as_bytes())
            .await
            .unwrap();
        let revision = next().await["result"]["revision"].clone();
        let call = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tick/call","params":{"tool":"quotes","args":{"symbols":["TSLA.US"]},"revision":revision}});
        host_requests
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
        api.later_request.notified().await;
        pushes
            .send(MarketEvent::Failure {
                code: tick_provider::exit_code::NOT_AUTHORIZED,
                message: "authorization expired or revoked".into(),
            })
            .unwrap();
        next().await
    };
    let (result, answered) = tokio::join!(tick, host);
    assert_eq!(
        result.unwrap_err().code,
        tick_provider::exit_code::NOT_AUTHORIZED
    );
    assert_eq!(answered["id"], 2, "{answered}");
    assert_eq!(
        answered["error"]["message"],
        "authorization expired or revoked"
    );
    assert_eq!(
        answered["error"]["data"]["code"],
        tick_provider::exit_code::NOT_AUTHORIZED
    );
}
