use async_trait::async_trait;
use serde_json::json;
use tickloop_ticks::{
    api::{MarketState, Quote, QuoteApi, WatchlistGroup},
    cli::{OnceArgs, OutputFormat, WatchlistArgs},
    commands::watchlist,
    config::Settings,
    error::Result,
    metadata::TickKind,
    stream::{Records, TickStream},
};

struct Watchlist;

#[async_trait]
impl QuoteApi for Watchlist {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let quotes: Vec<Quote> = serde_json::from_value(json!([
            {"symbol":"700.HK","name":"腾讯控股","latest_price":"480","previous_close":"400","volume":12000,"updated_at":"2026-09-10T08:00:00Z"},
            {"symbol":"TSLA.US","name":"Tesla","latest_price":"110","previous_close":"100","volume":9000}
        ])).unwrap();
        Ok(quotes
            .into_iter()
            .filter(|quote| symbols.contains(&quote.symbol))
            .collect())
    }

    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        Ok(serde_json::from_value(json!([
            {"id":1,"name":"港股","symbols":["700.HK"]},
            {"id":2,"name":"US","symbols":["TSLA.US"]}
        ]))
        .unwrap())
    }

    async fn market_states(&self, _: &[String]) -> Result<Vec<MarketState>> {
        Ok(serde_json::from_value(
            json!([{"market":"HK","status":"open"},{"market":"US","status":"pre"}]),
        )
        .unwrap())
    }
}

#[tokio::test]
async fn once_defaults_to_first_group_and_publishes_group_identity_without_decorating_json_symbols()
{
    let args = WatchlistArgs {
        group: None,
        output: OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
    };
    let mut output = Vec::new();
    let mut events = TickStream::in_memory(TickKind::Watchlist);
    watchlist::run_once(
        &args,
        &Watchlist,
        &Settings::default(),
        false,
        &mut output,
        &mut events,
    )
    .await
    .unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["symbol"], "700.HK");
    assert_eq!(rows[0]["change_percent"], "20");
    assert_eq!(rows[0].as_object().unwrap().len(), 6);
    let event: serde_json::Value = events.records().all().remove(0);
    assert_eq!(event["data"]["group"], json!({"id":1,"name":"港股"}));
    assert_eq!(event["data"]["quotes"], rows);
    assert_eq!(
        event["data"]["quote_times"],
        json!([{"symbol":"700.HK","updated_at":"2026-09-10T08:00:00Z"}])
    );
    assert_eq!(
        event["data"]["market_states"],
        json!([{"market":"HK","status":"open"}])
    );
}

#[tokio::test]
async fn requested_group_accepts_name_or_id_and_unknown_groups_are_usage_errors() {
    for requested in ["US", "2"] {
        let args = WatchlistArgs {
            group: Some(requested.into()),
            output: OnceArgs {
                once: true,
                format: OutputFormat::Json,
            },
        };
        let mut output = Vec::new();
        watchlist::run_once(
            &args,
            &Watchlist,
            &Settings::default(),
            false,
            &mut output,
            &mut TickStream::in_memory(TickKind::Watchlist),
        )
        .await
        .unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(rows[0]["symbol"], "TSLA.US");
    }
    let args = WatchlistArgs {
        group: Some("missing".into()),
        output: OnceArgs::default(),
    };
    let mut output = Vec::new();
    let error = watchlist::run_once(
        &args,
        &Watchlist,
        &Settings::default(),
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Watchlist),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error.message.contains("missing"));
    assert!(output.is_empty());
}

struct LiveWatchlist {
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>>,
    >,
    subscriptions: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl QuoteApi for LiveWatchlist {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        Watchlist.quotes(symbols).await
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        Watchlist.watchlist_groups().await
    }
    async fn market_states(&self, symbols: &[String]) -> Result<Vec<MarketState>> {
        Watchlist.market_states(symbols).await
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        let mut subscriptions = self.subscriptions.lock().unwrap();
        for symbol in symbols {
            if !subscriptions.contains(symbol) {
                subscriptions.push(symbol.clone());
            }
        }
        Ok(())
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.subscriptions
            .lock()
            .unwrap()
            .retain(|symbol| !symbols.contains(symbol));
        Ok(())
    }
}

async fn wait_for(observed: &Records, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while observed.len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn switching_groups_updates_subscriptions_and_preserves_push_and_market_events_in_order() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tickloop_ticks::api::{MarketEvent, QuoteUpdate};
    let (push, receiver) = tokio::sync::mpsc::unbounded_channel();
    let api = LiveWatchlist {
        events: std::sync::Mutex::new(Some(receiver)),
        subscriptions: std::sync::Mutex::new(Vec::new()),
    };
    let (keys, key_events) = tokio::sync::mpsc::unbounded_channel();
    let mut updates = TickStream::in_memory(TickKind::Watchlist);
    let observed = updates.records();
    let producer = tokio::spawn(async move {
        wait_for(&observed, 1).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        ))))
        .unwrap();
        wait_for(&observed, 2).await;
        let update: QuoteUpdate = serde_json::from_value(json!({"symbol":"TSLA.US","latest_price":"150","open":"101","high":"151","low":"100","volume":11000,"turnover":"1600000","updated_at":"2026-09-10T14:00:00Z"})).unwrap();
        push.send(MarketEvent::Quote(update.clone())).unwrap();
        push.send(MarketEvent::Quote(update)).unwrap();
        wait_for(&observed, 3).await;
        push.send(MarketEvent::Market(
            serde_json::from_value(json!({"market":"US","status":"open"})).unwrap(),
        ))
        .unwrap();
        wait_for(&observed, 4).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ))))
        .unwrap();
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
    let args = WatchlistArgs {
        group: None,
        output: OnceArgs::default(),
    };
    watchlist::run_live(
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
    let records = updates.records().all();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0]["data"]["group"]["id"], 1);
    assert_eq!(records[1]["data"]["group"]["id"], 2);
    assert_eq!(records[2]["data"]["quotes"][0]["latest_price"], "150");
    assert_eq!(records[3]["data"]["market_states"][0]["status"], "open");
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("TSLA.US"), "{screen}");
    assert!(screen.contains("150"), "{screen}");
    assert!(screen.contains('▶'), "{screen}");
    assert!(!screen.contains("700.HK"), "{screen}");
    assert!(api.subscriptions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn narrow_watchlists_keep_symbol_and_change_percent_in_both_icon_themes() {
    use tickloop_ui::settings::IconTheme;
    for theme in [IconTheme::Unicode, IconTheme::Nerd] {
        let (push, receiver) = tokio::sync::mpsc::unbounded_channel();
        let api = LiveWatchlist {
            events: std::sync::Mutex::new(Some(receiver)),
            subscriptions: std::sync::Mutex::new(Vec::new()),
        };
        let settings = Settings {
            icon_theme: theme,
            ..Settings::default()
        };
        let args = WatchlistArgs {
            group: None,
            output: OnceArgs::default(),
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(32, 8)).unwrap();
        let input = futures_util::stream::iter([Ok(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ))]);
        watchlist::run_live(
            &args,
            &api,
            &settings,
            &mut terminal,
            input,
            &mut TickStream::in_memory(TickKind::Watchlist),
        )
        .await
        .unwrap();
        drop(push);
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("700.HK"), "{screen}");
        assert!(screen.contains("+20.00%"), "{screen}");
        assert!(!screen.contains("Latest Price"), "{screen}");
        assert!(!screen.contains("腾讯控股"), "{screen}");
        assert!(
            screen.contains(tickloop_ui::icons::Icon::MarketOpen.glyph(theme).trim_end()),
            "{screen}"
        );
    }
}

struct RefreshingMarkets {
    live: LiveWatchlist,
    observations: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl QuoteApi for RefreshingMarkets {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        self.live.quotes(symbols).await
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        self.live.watchlist_groups().await
    }
    async fn market_states(&self, _: &[String]) -> Result<Vec<MarketState>> {
        let first = self
            .observations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            == 0;
        Ok(
            serde_json::from_value(
                json!([{"market":"HK","status":if first {"pre"} else {"open"}}]),
            )
            .unwrap(),
        )
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>> {
        self.live.take_events().await
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.live.subscribe_quotes(symbols).await
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.live.unsubscribe_quotes(symbols).await
    }
}

#[tokio::test(start_paused = true)]
async fn market_status_refreshes_without_quote_pushes_and_unchanged_status_does_not_emit_again() {
    let (push, receiver) = tokio::sync::mpsc::unbounded_channel();
    let api = RefreshingMarkets {
        live: LiveWatchlist {
            events: std::sync::Mutex::new(Some(receiver)),
            subscriptions: std::sync::Mutex::new(Vec::new()),
        },
        observations: std::sync::atomic::AtomicUsize::new(0),
    };
    let input = futures_util::stream::once(async {
        tokio::time::sleep(std::time::Duration::from_secs(61)).await;
        Ok(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ))
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
    let mut events = TickStream::in_memory(TickKind::Watchlist);
    watchlist::run_live(
        &WatchlistArgs {
            group: None,
            output: OnceArgs::default(),
        },
        &api,
        &Settings::default(),
        &mut terminal,
        Box::pin(input),
        &mut events,
    )
    .await
    .unwrap();
    drop(push);
    let records = events.records().all();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["data"]["market_states"][0]["status"], "pre");
    assert_eq!(records[1]["data"]["market_states"][0]["status"], "open");
    assert!(api.live.subscriptions.lock().unwrap().is_empty());
}

struct RejectedSubscription(LiveWatchlist);

#[async_trait]
impl QuoteApi for RejectedSubscription {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        self.0.quotes(symbols).await
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        self.0.watchlist_groups().await
    }
    async fn market_states(&self, symbols: &[String]) -> Result<Vec<MarketState>> {
        self.0.market_states(symbols).await
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>> {
        self.0.take_events().await
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.0.subscribe_quotes(symbols).await?;
        Err(tickloop_ticks::error::Error::network("subscription failed"))
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.0.unsubscribe_quotes(symbols).await
    }
}

#[tokio::test]
async fn a_failed_subscription_releases_any_partially_subscribed_symbols() {
    let (push, receiver) = tokio::sync::mpsc::unbounded_channel();
    let api = RejectedSubscription(LiveWatchlist {
        events: std::sync::Mutex::new(Some(receiver)),
        subscriptions: std::sync::Mutex::new(Vec::new()),
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
    let args = WatchlistArgs {
        group: None,
        output: OnceArgs::default(),
    };
    let error = watchlist::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        futures_util::stream::pending(),
        &mut TickStream::in_memory(TickKind::Watchlist),
    )
    .await
    .unwrap_err();
    drop(push);
    assert_eq!(error.code, 4);
    assert!(api.0.subscriptions.lock().unwrap().is_empty());
}

struct RejectedGroupSwitch(LiveWatchlist);
#[async_trait]
impl QuoteApi for RejectedGroupSwitch {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        self.0.quotes(symbols).await
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        self.0.watchlist_groups().await
    }
    async fn market_states(&self, symbols: &[String]) -> Result<Vec<MarketState>> {
        self.0.market_states(symbols).await
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>> {
        self.0.take_events().await
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        if symbols.iter().any(|symbol| symbol == "TSLA.US") {
            assert!(
                self.0
                    .subscriptions
                    .lock()
                    .unwrap()
                    .contains(&"700.HK".into()),
                "old group stays subscribed until replacement succeeds"
            );
            self.0.subscribe_quotes(symbols).await?;
            return Err(tickloop_ticks::error::Error::network("switch failed"));
        }
        self.0.subscribe_quotes(symbols).await
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.0.unsubscribe_quotes(symbols).await
    }
}

#[tokio::test]
async fn failed_group_switch_keeps_old_group_and_cleans_up_the_partial_new_subscription() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let (_push, receiver) = tokio::sync::mpsc::unbounded_channel();
    let api = RejectedGroupSwitch(LiveWatchlist {
        events: std::sync::Mutex::new(Some(receiver)),
        subscriptions: std::sync::Mutex::new(Vec::new()),
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
    let input = futures_util::stream::iter(
        [KeyCode::Right, KeyCode::Char('q')]
            .map(|key| Ok(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)))),
    );
    let mut updates = TickStream::in_memory(TickKind::Watchlist);
    watchlist::run_live(
        &WatchlistArgs {
            group: None,
            output: OnceArgs::default(),
        },
        &api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut updates,
    )
    .await
    .unwrap();
    let record: serde_json::Value = updates.records().all().remove(0);
    assert_eq!(record["data"]["group"]["id"], 1);
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("700.HK"), "{screen}");
    assert!(screen.contains("Unavailable"), "{screen}");
    assert!(api.0.subscriptions.lock().unwrap().is_empty());
}
