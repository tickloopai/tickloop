use crate::{
    api::{Quote, QuoteServices, normalize_symbol},
    cli::QuoteArgs,
    config::Settings,
    error::Result,
    output::TableData,
};
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;

pub const HEADERS: &[&str] = &[
    "Symbol",
    "Name",
    "Latest Price",
    "Change",
    "Change Percent",
    "Open",
    "High",
    "Low",
    "Previous Close",
    "Volume",
    "Turnover",
    "Updated At",
    "Error",
];

pub fn table(quotes: &[Quote]) -> TableData {
    TableData {
        headers: HEADERS.to_vec(),
        rows: quotes
            .iter()
            .map(|quote| {
                let mut row = vec![json!(quote.symbol), json!(quote.name)];
                if let Some(error) = &quote.error {
                    row.extend(vec![Value::Null; HEADERS.len() - 3]);
                    row.push(json!(error));
                    return row;
                }
                let change = quote.latest_price - quote.previous_close;
                let percentage = if quote.previous_close.is_zero() {
                    Value::Null
                } else {
                    json!(
                        (change / quote.previous_close * Decimal::ONE_HUNDRED)
                            .normalize()
                            .to_string()
                    )
                };
                row.extend([
                    json!(quote.latest_price.normalize().to_string()),
                    if quote.previous_close.is_zero() {
                        Value::Null
                    } else {
                        json!(change.normalize().to_string())
                    },
                    percentage,
                    if quote.open.is_zero() {
                        Value::Null
                    } else {
                        json!(quote.open)
                    },
                    json!(quote.high),
                    json!(quote.low),
                    if quote.previous_close.is_zero() {
                        Value::Null
                    } else {
                        json!(quote.previous_close)
                    },
                    json!(quote.volume),
                    json!(quote.turnover),
                    json!(quote.updated_at),
                    Value::Null,
                ]);
                row
            })
            .collect(),
    }
}

/// Arguments of the `quotes` Tick Tool; the schema the Agent reads is derived from it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotesArgs {
    /// Symbols to price, such as `AAPL.US` or `700.HK`.
    pub symbols: Vec<String>,
}

pub fn tools() -> crate::tools::Tools {
    crate::tools::Tools::default()
        .query::<QuotesArgs>(
            "quotes",
            "Latest price, change, range, and volume for the given symbols, in the Quote table's shape.",
            |api: &QuoteServices, args| {
                Box::pin(async move {
                    let symbols: Vec<String> =
                        args.symbols.iter().map(|symbol| normalize_symbol(symbol)).collect();
                    Ok(table(&api.quotes(&symbols).await?).json())
                })
            },
        )
        .recover_with("quotes")
}

pub async fn run_once(
    args: &QuoteArgs,
    api: &QuoteServices,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut crate::stream::TickStream,
) -> Result<()> {
    let symbols: Vec<String> = args
        .symbols
        .iter()
        .map(|symbol| normalize_symbol(symbol))
        .collect();
    let quotes = api.quotes(&symbols).await?;
    let table = table(&quotes);
    updates.publish(table.json())?;
    table.write(args.output.format, settings, color, output)
}

pub struct QuoteView {
    pub quotes: Vec<Quote>,
}

#[async_trait::async_trait]
impl crate::runtime::LiveView for QuoteView {
    fn title(&self) -> String {
        "Quotes".into()
    }
    fn table(&self) -> TableData {
        table(&self.quotes)
    }
    fn priority(&self) -> &[usize] {
        &[0, 2, 4, 1, 3, 12, 9, 8, 5, 6, 7, 10, 11]
    }
    fn draw(
        &self,
        frame: &mut ratatui::Frame<'_>,
        state: &mut crate::runtime::ViewState,
        settings: &Settings,
    ) {
        let mut table = self.table();
        for (row, quote) in table.rows.iter_mut().zip(&self.quotes) {
            if quote.error.is_some() {
                row[0] = json!(format!("! {}", quote.symbol));
                row[2] = json!("Error");
            }
        }
        crate::runtime::draw_table(
            frame,
            &self.title(),
            &table,
            self.priority(),
            state,
            settings,
        );
    }
    fn on_market(&mut self, event: &crate::api::MarketEvent) {
        for quote in &mut self.quotes {
            quote.update_event(event);
        }
    }
}

pub async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    args: &QuoteArgs,
    api: &QuoteServices,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    mut input: S,
    updates: &mut crate::stream::TickStream,
) -> Result<()> {
    let symbols: Vec<String> = args
        .symbols
        .iter()
        .map(|symbol| normalize_symbol(symbol))
        .collect();
    let Some(quotes) = crate::runtime::cancellable(api.quotes(&symbols), &mut input).await? else {
        return Ok(());
    };
    let subscribed: Vec<String> = quotes
        .iter()
        .filter(|quote| quote.error.is_none())
        .map(|quote| quote.symbol.clone())
        .collect();
    let result = async {
        let Some(market) = crate::runtime::cancellable(api.take_events(), &mut input).await? else {
            return Ok(());
        };
        if !subscribed.is_empty()
            && crate::runtime::cancellable(api.subscribe_quotes(&subscribed), &mut input)
                .await?
                .is_none()
        {
            return Ok(());
        }
        crate::runtime::run(
            &mut QuoteView { quotes },
            api,
            settings,
            terminal,
            input,
            market,
            updates,
        )
        .await
    }
    .await;
    let cleanup = if subscribed.is_empty() {
        Ok(())
    } else {
        tokio::time::timeout(
            crate::runtime::CLEANUP_TIMEOUT,
            api.unsubscribe_quotes(&subscribed),
        )
        .await
        .unwrap_or_else(|_| {
            Err(crate::error::Error::network(
                "subscription cleanup timed out",
            ))
        })
    };
    result.and(cleanup)
}
