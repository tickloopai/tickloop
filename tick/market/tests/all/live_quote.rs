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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
    assert_eq!(records[0]["kind"], "snapshot");
    assert_eq!(records[1]["kind"], "update");
    assert_eq!(records[1]["sequence"], 2);
    assert_eq!(records[1]["data"][0]["latest_price"], "123.45");
    assert_eq!(records[2]["data"][0]["latest_price"], "150");
    assert_eq!(records[3]["data"][0]["latest_price"], "123.45");
    assert_eq!(records[3]["sequence"], 4);
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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
    let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
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
