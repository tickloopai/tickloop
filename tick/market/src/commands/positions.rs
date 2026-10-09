use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    args::{OnceArgs, OutputFormat},
    config::Settings,
    output::TableData,
    runtime::LiveView,
};
use tick_provider::{
    api::{MarketEvent, Quote, QuoteServices, TradeApi},
    error::{Error, Result},
    preferences::Language,
};
use tick_sdk::stream::TickStream;

pub const HEADERS: &[&str] = &[
    "Symbol",
    "Name",
    "Quantity",
    "Available Quantity",
    "Cost Price",
    "Latest Price",
    "Market Value",
    "Profit Loss",
    "Profit Loss Percent",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountMode {
    Paper,
    Live,
    Unknown,
}

impl AccountMode {
    fn name(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Live => "live",
            Self::Unknown => "unknown",
        }
    }

    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Paper, Language::ZhCn) => "模拟盘账户",
            (Self::Live, Language::ZhCn) => "实盘账户",
            (Self::Unknown, Language::ZhCn) => "账户模式未知",
            (Self::Paper, Language::En) => "Paper account",
            (Self::Live, Language::En) => "Live account",
            (Self::Unknown, Language::En) => "Account mode unknown",
        }
    }
}

#[derive(Deserialize)]
struct PositionsResponse {
    list: Vec<PositionChannel>,
}

#[derive(Deserialize)]
struct PositionChannel {
    account_channel: String,
    #[serde(default)]
    stock_info: Vec<Position>,
}

#[derive(Deserialize)]
struct Position {
    symbol: String,
    symbol_name: String,
    quantity: Decimal,
    available_quantity: Option<Decimal>,
    cost_price: Decimal,
    currency: String,
}

struct CurrencyTotal {
    market_value: Decimal,
    profit_loss: Decimal,
    has_all_quotes: bool,
}

pub struct PositionsView {
    account_mode: AccountMode,
    positions: Vec<Position>,
    quotes: BTreeMap<String, Quote>,
    language: Language,
}

impl PositionsView {
    pub fn from_snapshot(response: Value, quotes: Vec<Quote>, language: Language) -> Result<Self> {
        // Project the API response into business fields so internal account metadata cannot reach any output.
        let response: PositionsResponse = serde_json::from_value(response)
            .map_err(|error| Error::network(format!("invalid positions response: {error}")))?;
        let modes: BTreeSet<&str> = response
            .list
            .iter()
            .map(|channel| channel.account_channel.as_str())
            .collect();
        let account_mode = match modes.iter().copied().collect::<Vec<_>>().as_slice() {
            ["lb_papertrading"] => AccountMode::Paper,
            ["lb"] => AccountMode::Live,
            modes if modes.contains(&"lb") && modes.contains(&"lb_papertrading") => {
                return Err(Error::network(
                    "positions response combines live and paper accounts",
                ));
            }
            _ => AccountMode::Unknown,
        };
        Ok(Self {
            account_mode,
            positions: response
                .list
                .into_iter()
                .flat_map(|channel| channel.stock_info)
                .collect(),
            quotes: quotes
                .into_iter()
                .map(|quote| (quote.symbol.clone(), quote))
                .collect(),
            language,
        })
    }

    pub fn content(&self) -> Value {
        json!({"account_mode": self.account_mode.name(), "positions": self.table().json()})
    }

    fn symbols(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.positions
            .iter()
            .filter(|position| seen.insert(position.symbol.clone()))
            .map(|position| position.symbol.clone())
            .collect()
    }
}

#[async_trait]
impl LiveView for PositionsView {
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Positions
    }
    fn content(&self) -> Value {
        PositionsView::content(self)
    }

    fn title(&self) -> String {
        format!("Positions · {}", self.account_mode.label(self.language))
    }

    fn table(&self) -> TableData {
        let mut totals: BTreeMap<&str, CurrencyTotal> = BTreeMap::new();
        let mut rows: Vec<Vec<Value>> = self
            .positions
            .iter()
            .map(|position| {
                let price = self
                    .quotes
                    .get(&position.symbol)
                    .filter(|quote| quote.error.is_none())
                    .map(|quote| quote.latest_price);
                let market_value = price.map(|price| price * position.quantity);
                let profit_loss =
                    price.map(|price| (price - position.cost_price) * position.quantity);
                let cost_basis = (position.cost_price * position.quantity).abs();
                let profit_loss_percent = profit_loss
                    .filter(|_| !cost_basis.is_zero())
                    .map(|profit_loss| profit_loss / cost_basis * Decimal::ONE_HUNDRED);
                let total = totals.entry(&position.currency).or_insert(CurrencyTotal {
                    market_value: Decimal::ZERO,
                    profit_loss: Decimal::ZERO,
                    has_all_quotes: true,
                });
                if let (Some(market_value), Some(profit_loss)) = (market_value, profit_loss) {
                    total.market_value += market_value;
                    total.profit_loss += profit_loss;
                } else {
                    total.has_all_quotes = false;
                }
                vec![
                    json!(position.symbol),
                    json!(position.symbol_name),
                    decimal(Some(position.quantity)),
                    decimal(position.available_quantity),
                    decimal(Some(position.cost_price)),
                    decimal(price),
                    decimal(market_value),
                    decimal(profit_loss),
                    decimal(profit_loss_percent),
                ]
            })
            .collect();
        for (currency, total) in totals {
            rows.push(vec![
                json!(format!("Total {currency}")),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                decimal(total.has_all_quotes.then_some(total.market_value)),
                decimal(total.has_all_quotes.then_some(total.profit_loss)),
                Value::Null,
            ]);
        }
        TableData {
            headers: HEADERS.to_vec(),
            rows,
        }
    }

    fn priority(&self) -> &[usize] {
        &[0, 5, 7, 8, 2, 6, 1, 4, 3]
    }

    fn draw(
        &self,
        frame: &mut ratatui::Frame<'_>,
        state: &mut crate::runtime::ViewState,
        settings: &Settings,
    ) {
        crate::runtime::draw_table(
            frame,
            &self.title(),
            &self.table(),
            self.priority(),
            state,
            settings,
        );
        let area = frame.area();
        if self.positions.is_empty() && area.width >= 8 && area.height >= 4 {
            let content_area = crate::runtime::table_content_area(area);
            let message = match self.language {
                Language::ZhCn => "暂无持仓",
                Language::En => "No positions",
            };
            let text = tickloop_ui::format::pad(
                message,
                content_area.width as usize,
                settings.ambiguous_width,
                false,
            );
            tickloop_ui::format::write_text(
                frame.buffer_mut(),
                content_area.x,
                content_area.y + 1,
                &text,
                content_area.width as usize,
                settings.ambiguous_width,
                settings
                    .styles()
                    .panel_base()
                    .patch(settings.styles().muted_text),
            );
        }
    }

    fn on_market(&mut self, event: &MarketEvent) {
        let symbol = match event {
            MarketEvent::Quote(update) | MarketEvent::SessionQuote { update, .. } => &update.symbol,
            MarketEvent::QuoteSnapshot(quote) => &quote.symbol,
            _ => return,
        };
        if !self
            .positions
            .iter()
            .any(|position| position.symbol == *symbol)
        {
            return;
        }
        self.quotes
            .entry(symbol.clone())
            .or_insert_with(|| Quote {
                symbol: symbol.clone(),
                ..Quote::default()
            })
            .update_event(event);
    }
}

fn decimal(value: Option<Decimal>) -> Value {
    value
        .map(|value| json!(value.normalize().to_string()))
        .unwrap_or(Value::Null)
}

async fn load(
    quote_api: &QuoteServices,
    trade_api: &dyn TradeApi,
    settings: &Settings,
) -> Result<PositionsView> {
    let mut view =
        PositionsView::from_snapshot(trade_api.positions().await?, Vec::new(), settings.language)?;
    let symbols = view.symbols();
    if !symbols.is_empty() {
        view.quotes = quote_api
            .quotes(&symbols)
            .await?
            .into_iter()
            .map(|quote| (quote.symbol.clone(), quote))
            .collect();
    }
    Ok(view)
}

pub async fn run_once(
    args: &OnceArgs,
    quote_api: &QuoteServices,
    trade_api: &dyn TradeApi,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut TickStream,
) -> Result<()> {
    let view = load(quote_api, trade_api, settings).await?;
    updates.publish(view.content())?;
    if args.format == OutputFormat::Table {
        writeln!(output, "{}", view.account_mode.label(settings.language))?;
        if view.positions.is_empty() {
            writeln!(
                output,
                "{}",
                match settings.language {
                    Language::ZhCn => "暂无持仓",
                    Language::En => "No positions",
                }
            )?;
            return Ok(());
        }
    }
    view.table().write(args.format, settings, color, output)
}

pub async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    _args: &OnceArgs,
    quote_api: &QuoteServices,
    trade_api: &dyn TradeApi,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    mut input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let Some(mut view) =
        crate::runtime::cancellable(load(quote_api, trade_api, settings), &mut input).await?
    else {
        return Ok(());
    };
    let subscribed = view.symbols();
    let result = async {
        let (_idle_sender, idle_events) = tokio::sync::mpsc::unbounded_channel();
        let market = if subscribed.is_empty() {
            idle_events
        } else {
            let Some(market) =
                crate::runtime::cancellable(quote_api.take_events(), &mut input).await?
            else {
                return Ok(());
            };
            if crate::runtime::cancellable(quote_api.subscribe_quotes(&subscribed), &mut input)
                .await?
                .is_none()
            {
                return Ok(());
            }
            market
        };
        crate::runtime::run(
            &mut view, quote_api, settings, terminal, input, market, updates,
        )
        .await
    }
    .await;
    let cleanup = if subscribed.is_empty() {
        Ok(())
    } else {
        tokio::time::timeout(
            crate::runtime::CLEANUP_TIMEOUT,
            quote_api.unsubscribe_quotes(&subscribed),
        )
        .await
        .unwrap_or_else(|_| Err(Error::network("subscription cleanup timed out")))
    };
    result.and(cleanup)
}
