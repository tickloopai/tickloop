use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::sync::Mutex;
use tick_market::{
    args::{OnceArgs, QuoteArgs, WatchlistArgs},
    commands::{quote, watchlist},
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    api::{MarketEvent, Quote, QuoteApi, WatchlistGroup},
    error::Result,
};
use tick_sdk::stream::TickStream;

struct MissingQuote(Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>);
#[async_trait]
impl QuoteApi for MissingQuote {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Ok(vec![Quote {
            symbol: "BAD.US".into(),
            error: Some("Unknown security".into()),
            ..Quote::default()
        }])
    }
    async fn watchlist_groups(&self) -> Result<Vec<WatchlistGroup>> {
        Ok(vec![WatchlistGroup {
            id: 1,
            name: "Group".into(),
            symbols: vec!["BAD.US".into()],
        }])
    }
    async fn subscribe_quotes(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.0.lock().unwrap().take().unwrap())
    }
}

#[tokio::test]
async fn minimum_panels_show_quote_errors_even_when_name_and_error_columns_are_hidden() {
    for is_watchlist in [false, true] {
        let (_sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let api = MissingQuote(Mutex::new(Some(receiver)));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(32, 6)).unwrap();
        let input = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))]);
        let mut updates = TickStream::in_memory(TickKind::Quote, "longbridge");
        if is_watchlist {
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
        } else {
            quote::run_live(
                &QuoteArgs {
                    symbols: vec!["BAD.US".into()],
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
        }
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("! BAD.US"), "{screen}");
        assert!(screen.contains("Error"), "{screen}");
        let record: serde_json::Value = updates.records().all().remove(0);
        let row = if is_watchlist {
            &record["data"]["quotes"][0]
        } else {
            &record["data"][0]
        };
        assert_eq!(row["symbol"], "BAD.US");
        assert!(row["latest_price"].is_null());
    }
}
