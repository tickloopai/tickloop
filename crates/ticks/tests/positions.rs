use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_ticks::{
    api::{Quote, QuoteApi, TradeApi},
    cli::{OnceArgs, OutputFormat},
    commands::positions,
    config::Settings,
    error::Result,
    metadata::TickKind,
    stream::TickStream,
};

struct Portfolio(Value);
#[async_trait]
impl TradeApi for Portfolio {
    async fn positions(&self) -> Result<Value> {
        Ok(self.0.clone())
    }
}
struct Quotes;
#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        assert_eq!(symbols, &["700.HK", "TSLA.US", "AAPL.US"]);
        Ok(serde_json::from_value(json!([
            {"symbol":"700.HK","latest_price":"120"},
            {"symbol":"TSLA.US","latest_price":"250"},
            {"symbol":"AAPL.US","latest_price":"180"}
        ]))
        .unwrap())
    }
}
fn portfolio() -> Portfolio {
    Portfolio(
        json!({"account_id":"PRIVATE", "list":[{"account_channel":"lb_papertrading", "account_no":"PRIVATE", "stock_info":[
            {"symbol":"700.HK", "symbol_name":"腾讯控股", "quantity":"10", "available_quantity":"8", "cost_price":"100", "currency":"HKD", "metadata":{"account_id":"PRIVATE", "nested":[{"member_id":"PRIVATE"}]}},
            {"symbol":"TSLA.US", "symbol_name":"Tesla", "quantity":"2", "available_quantity":"2", "cost_price":"200", "currency":"USD"},
            {"symbol":"AAPL.US", "symbol_name":"Apple", "quantity":"5", "available_quantity":"5", "cost_price":"200", "currency":"USD"}
        ]}]}),
    )
}
#[tokio::test]
async fn positions_snapshot_calculates_profit_and_separate_currency_totals_without_internal_fields()
{
    let mut output = Vec::new();
    let mut updates = TickStream::in_memory(TickKind::Positions);
    let args = OnceArgs {
        once: true,
        format: OutputFormat::Json,
    };
    positions::run_once(
        &args,
        &Quotes,
        &portfolio(),
        &Settings::default(),
        false,
        &mut output,
        &mut updates,
    )
    .await
    .unwrap();
    let rows: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows[0]["market_value"], "1200");
    assert_eq!(rows[0]["profit_loss"], "200");
    assert_eq!(rows[0]["profit_loss_percent"], "20");
    assert_eq!(rows[2]["profit_loss"], "-100");
    assert_eq!(rows[3]["symbol"], "Total HKD");
    assert_eq!(rows[3]["market_value"], "1200");
    assert_eq!(rows[4]["symbol"], "Total USD");
    assert_eq!(rows[4]["market_value"], "1400");
    assert_eq!(rows[4]["profit_loss"], "0");
    assert_eq!(rows[0].as_object().unwrap().len(), 9);
    let text = String::from_utf8(output).unwrap();
    assert!(!text.contains("PRIVATE"));
    assert!(!text.contains("account_"));
    assert!(!text.contains("member_id"));
    assert!(!text.contains("Paper"));
    let event: Value = updates.records().all().remove(0);
    assert_eq!(event["data"]["account_mode"], "paper");
    assert_eq!(event["data"]["positions"], rows);
}

struct LiveQuotes {
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>>,
    >,
    subscriptions: std::sync::Mutex<Vec<Vec<String>>>,
}
#[async_trait]
impl QuoteApi for LiveQuotes {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        Quotes.quotes(symbols).await
    }
    async fn subscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.subscriptions.lock().unwrap().push(symbols.to_vec());
        Ok(())
    }
    async fn unsubscribe_quotes(&self, symbols: &[String]) -> Result<()> {
        self.subscriptions.lock().unwrap().push(symbols.to_vec());
        Ok(())
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tickloop_ticks::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

#[tokio::test]
async fn position_quote_push_changes_screen_and_content_then_unsubscribes() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tickloop_ticks::api::{MarketEvent, QuoteUpdate};
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let quotes = LiveQuotes {
        events: std::sync::Mutex::new(Some(events)),
        subscriptions: std::sync::Mutex::new(Vec::new()),
    };
    let (keys, key_events) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let update: QuoteUpdate = serde_json::from_value(json!({"symbol":"TSLA.US", "latest_price":"300", "open":"250", "high":"300", "low":"250", "volume":10, "turnover":"3000", "updated_at":"2026-09-10T14:00:00Z"})).unwrap();
        push.send(MarketEvent::Quote(update.clone())).unwrap();
        push.send(MarketEvent::Quote(update)).unwrap();
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ))))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 16)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Positions);
    positions::run_live(
        &OnceArgs::default(),
        &quotes,
        &portfolio(),
        &Settings {
            language: tickloop_ticks::config::Language::En,
            ..Settings::default()
        },
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(key_events),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("Paper account"), "{screen}");
    assert!(screen.contains("300.00"), "{screen}");
    assert!(screen.contains("+50.00%"), "{screen}");
    let records: Vec<Value> = updates.records().all();
    assert_eq!(
        records.len(),
        2,
        "only business changes emit content updates"
    );
    assert_eq!(records[1]["data"]["positions"][1]["market_value"], "600");
    assert_eq!(records[1]["data"]["positions"][1]["profit_loss"], "200");
    assert_eq!(records[1]["data"]["positions"][4]["market_value"], "1500");
    assert_eq!(records[1]["data"]["positions"][4]["profit_loss"], "100");
    assert_eq!(
        *quotes.subscriptions.lock().unwrap(),
        vec![
            vec!["700.HK", "TSLA.US", "AAPL.US"],
            vec!["700.HK", "TSLA.US", "AAPL.US"]
        ]
    );
}

struct NoQuotes;
#[async_trait]
impl QuoteApi for NoQuotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        panic!("an empty portfolio does not fetch quotes")
    }
}

#[tokio::test]
async fn empty_live_account_has_an_explicit_empty_state_and_successful_json_snapshot() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let portfolio = Portfolio(json!({"list":[{"account_channel":"lb", "stock_info":[]}]}));
    let settings = Settings {
        language: tickloop_ticks::config::Language::En,
        ..Settings::default()
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
    let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))]);
    let mut updates = TickStream::in_memory(TickKind::Positions);
    positions::run_live(
        &OnceArgs::default(),
        &NoQuotes,
        &portfolio,
        &settings,
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
    assert!(screen.contains("Live account"), "{screen}");
    assert!(screen.contains("No positions"), "{screen}");
    let mut output = Vec::new();
    positions::run_once(
        &OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
        &NoQuotes,
        &portfolio,
        &settings,
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Positions),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), json!([]));
}

struct ValuationQuotes;
#[async_trait]
impl QuoteApi for ValuationQuotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Ok(serde_json::from_value(json!([
            {"symbol":"SHORT.US", "latest_price":"80"},
            {"symbol":"ZERO.US", "latest_price":"0.001"},
            {"symbol":"MISSING.HK", "error":"Quote permission unavailable"},
            {"symbol":"700.HK", "latest_price":"120"}
        ]))
        .unwrap())
    }
}

#[tokio::test]
async fn unavailable_quotes_do_not_fabricate_zero_values_or_partial_currency_totals() {
    let portfolio = Portfolio(
        json!({"list":[{"account_channel":"unknown_future_channel", "stock_info":[
            {"symbol":"SHORT.US", "symbol_name":"Short", "quantity":"-2", "available_quantity":null, "cost_price":"100", "currency":"USD"},
            {"symbol":"ZERO.US", "symbol_name":"Zero cost", "quantity":"1", "available_quantity":"1", "cost_price":"0", "currency":"USD"},
            {"symbol":"MISSING.HK", "symbol_name":"No permission", "quantity":"1", "available_quantity":"1", "cost_price":"20", "currency":"HKD"},
            {"symbol":"700.HK", "symbol_name":"Tencent", "quantity":"10", "available_quantity":"10", "cost_price":"100", "currency":"HKD"}
        ]}]}),
    );
    let mut output = Vec::new();
    let mut updates = TickStream::in_memory(TickKind::Positions);
    positions::run_once(
        &OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
        &ValuationQuotes,
        &portfolio,
        &Settings::default(),
        false,
        &mut output,
        &mut updates,
    )
    .await
    .unwrap();
    let rows: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows[0]["market_value"], "-160");
    assert!(rows[0]["available_quantity"].is_null());
    assert_eq!(rows[0]["profit_loss"], "40");
    assert_eq!(rows[0]["profit_loss_percent"], "20");
    assert_eq!(rows[1]["latest_price"], "0.001");
    assert!(rows[1]["profit_loss_percent"].is_null());
    assert!(rows[2]["latest_price"].is_null());
    assert!(rows[2]["profit_loss"].is_null());
    assert_eq!(rows[4]["symbol"], "Total HKD");
    assert!(rows[4]["market_value"].is_null());
    assert!(rows[4]["profit_loss"].is_null());
    let event: Value = updates.records().all().remove(0);
    assert_eq!(event["data"]["account_mode"], "unknown");
}

#[tokio::test]
async fn table_banner_uses_response_account_channel_while_mixed_books_are_rejected() {
    let settings = Settings {
        language: tickloop_ticks::config::Language::En,
        ..Settings::default()
    };
    for (channel, label) in [
        ("lb_papertrading", "Paper account"),
        ("lb", "Live account"),
        ("future_channel", "Account mode unknown"),
    ] {
        let portfolio = Portfolio(json!({"list":[{"account_channel":channel, "stock_info":[]}]}));
        let mut output = Vec::new();
        positions::run_once(
            &OnceArgs {
                once: true,
                format: OutputFormat::Table,
            },
            &NoQuotes,
            &portfolio,
            &settings,
            false,
            &mut output,
            &mut TickStream::in_memory(TickKind::Positions),
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("{label}\nNo positions\n")
        );
    }
    let mixed = Portfolio(
        json!({"list":[{"account_channel":"lb"}, {"account_channel":"lb_papertrading"}]}),
    );
    let mut output = Vec::new();
    let error = positions::run_once(
        &OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
        &NoQuotes,
        &mixed,
        &settings,
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Positions),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, 4);
    assert!(output.is_empty());
}
