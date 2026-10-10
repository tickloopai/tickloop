use std::io::Write;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_ui::{icons::Icon, settings::IconTheme};

use crate::{
    args::{OutputFormat, WatchlistArgs},
    config::Settings,
    output::TableData,
    tools::Tools,
};
use tick_provider::{
    api::{MarketState, MarketStatus, Quote, QuoteServices, WatchlistGroup},
    error::{Error, Result},
};
use tick_sdk::stream::TickStream;

const QUOTE_COLUMNS: &[usize] = &[0, 1, 2, 3, 4, 9];

pub struct WatchlistView {
    pub groups: Vec<WatchlistGroup>,
    pub selected_group: usize,
    pub quotes: Vec<Quote>,
    pub market_states: Vec<MarketState>,
    pub subscriptions: Vec<String>,
}

impl WatchlistView {
    pub fn table(&self) -> TableData {
        let quotes = super::quote::table(&self.quotes);
        let mut rows: Vec<Vec<Value>> = quotes
            .rows
            .iter()
            .map(|row| {
                QUOTE_COLUMNS
                    .iter()
                    .map(|&index| row[index].clone())
                    .collect()
            })
            .collect();
        for (row, quote) in rows.iter_mut().zip(&self.quotes) {
            if let Some(error) = &quote.error {
                row[1] = json!(format!("Error: {error}"));
            }
        }
        TableData {
            headers: QUOTE_COLUMNS
                .iter()
                .map(|&index| quotes.headers[index])
                .collect(),
            rows,
        }
    }

    pub fn content(&self) -> Value {
        let group = self
            .groups
            .get(self.selected_group)
            .map(|group| json!({"id":group.id,"name":group.name}));
        let quote_times: Vec<Value> = self
            .quotes
            .iter()
            .map(|quote| json!({"symbol":quote.symbol,"updated_at":quote.updated_at}))
            .collect();
        json!({"group":group,"quotes":self.table().json(),"quote_times":quote_times,"market_states":self.market_states})
    }

    fn display_table(&self, theme: IconTheme) -> TableData {
        let mut table = self.table();
        for (row, quote) in table.rows.iter_mut().zip(&self.quotes) {
            let status = self
                .market_states
                .iter()
                .find(|state| Some(state.market.as_str()) == symbol_market(&quote.symbol))
                .map_or(MarketStatus::Unknown, |state| state.status);
            let glyph = match status {
                MarketStatus::Pre => Icon::MarketPre.glyph(theme),
                MarketStatus::Open => Icon::MarketOpen.glyph(theme),
                MarketStatus::Closed => Icon::MarketClosed.glyph(theme),
                MarketStatus::Unknown => "?",
            };
            if quote.error.is_some() {
                row[0] = json!(format!("! {}", quote.symbol));
                row[2] = json!("Error");
            } else {
                row[0] = json!(format!("{} {}", glyph.trim_end(), quote.symbol));
            }
        }
        table
    }
}

fn symbol_market(symbol: &str) -> Option<&str> {
    symbol.rsplit_once('.').map(|(_, market)| match market {
        "SH" | "SZ" => "CN",
        market => market,
    })
}

fn relevant_states(states: Vec<MarketState>, symbols: &[String]) -> Vec<MarketState> {
    let mut states: Vec<MarketState> = states
        .into_iter()
        .filter(|state| {
            symbols
                .iter()
                .any(|symbol| symbol_market(symbol) == Some(&state.market))
        })
        .collect();
    states.sort_by(|left, right| left.market.cmp(&right.market));
    states
}

/// Arguments of the `rows` Tick Tool; the schema the Agent reads is derived from it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RowsArgs {
    /// Watchlist group name or id; the first group when left out.
    pub group: Option<String>,
}

pub fn tools() -> Tools {
    Tools::default()
        .query::<RowsArgs>(
            "rows",
            "Current quotes of a Longbridge watchlist group, in the Watchlist Panel's shape.",
            |api: &QuoteServices, args: RowsArgs| {
                Box::pin(async move { Ok(group_view(args.group.as_deref(), api).await?.content()) })
            },
        )
        .recover_with("rows")
}

async fn initial_view(args: &WatchlistArgs, api: &QuoteServices) -> Result<WatchlistView> {
    group_view(args.group.as_deref(), api).await
}

/// The Panel's whole content for one group: the group list, its quotes and their market states.
async fn group_view(group: Option<&str>, api: &QuoteServices) -> Result<WatchlistView> {
    let groups = api.watchlist_groups().await?;
    let selected_group = match group {
        Some(requested) => groups
            .iter()
            .position(|group| group.name == requested || group.id.to_string() == requested)
            .ok_or_else(|| Error::usage(format!("watchlist group not found: {requested}")))?,
        None => 0,
    };
    let symbols = groups
        .get(selected_group)
        .map_or(&[][..], |group| group.symbols.as_slice());
    let quotes = if symbols.is_empty() {
        Vec::new()
    } else {
        api.quotes(symbols).await?
    };
    let market_states = if symbols.is_empty() {
        Vec::new()
    } else {
        relevant_states(api.market_states(symbols).await?, symbols)
    };
    Ok(WatchlistView {
        groups,
        selected_group,
        quotes,
        market_states,
        subscriptions: Vec::new(),
    })
}

pub async fn run_once(
    args: &WatchlistArgs,
    api: &QuoteServices,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut TickStream,
) -> Result<()> {
    let view = initial_view(args, api).await?;
    updates.publish(view.content())?;
    if args.output.format == OutputFormat::Json {
        return view
            .table()
            .write(args.output.format, settings, false, output);
    }
    writeln!(
        output,
        "Watchlist: {}",
        tickloop_ui::format::clean(
            view.groups
                .get(view.selected_group)
                .map_or("No groups", |group| group.name.as_str())
        )
    )?;
    view.display_table(settings.icon_theme)
        .write(args.output.format, settings, color, output)
}

#[async_trait::async_trait]
impl crate::runtime::LiveView for WatchlistView {
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Watchlist
    }
    fn title(&self) -> String {
        format!(
            "Watchlist · {}",
            self.groups
                .get(self.selected_group)
                .map_or("No groups", |group| group.name.as_str())
        )
    }
    fn table(&self) -> TableData {
        WatchlistView::table(self)
    }
    fn content(&self) -> Value {
        WatchlistView::content(self)
    }
    fn priority(&self) -> &[usize] {
        &[0, 4, 2, 1, 3, 5]
    }
    fn refresh_interval(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(30))
    }

    async fn refresh(&mut self, api: &QuoteServices) -> Result<()> {
        if let Some(group) = self.groups.get(self.selected_group)
            && !group.symbols.is_empty()
        {
            self.market_states =
                relevant_states(api.market_states(&group.symbols).await?, &group.symbols);
        }
        Ok(())
    }

    fn on_market(&mut self, event: &tick_provider::api::MarketEvent) {
        match event {
            tick_provider::api::MarketEvent::Quote(_)
            | tick_provider::api::MarketEvent::SessionQuote { .. }
            | tick_provider::api::MarketEvent::QuoteSnapshot(_) => {
                for quote in &mut self.quotes {
                    quote.update_event(event);
                }
            }
            tick_provider::api::MarketEvent::Market(update) => {
                if !self
                    .quotes
                    .iter()
                    .any(|quote| symbol_market(&quote.symbol) == Some(update.market.as_str()))
                {
                    return;
                }
                if let Some(state) = self
                    .market_states
                    .iter_mut()
                    .find(|state| state.market == update.market)
                {
                    *state = update.clone();
                } else {
                    self.market_states.push(update.clone());
                }
            }
            _ => (),
        }
    }

    async fn on_key(
        &mut self,
        key: crossterm::event::KeyCode,
        api: &QuoteServices,
        state: &mut crate::runtime::ViewState,
    ) -> Result<bool> {
        use crossterm::event::KeyCode;
        let forward = match key {
            KeyCode::Right | KeyCode::Char(']') => true,
            KeyCode::Left | KeyCode::Char('[') => false,
            _ => return Ok(false),
        };
        if self.groups.len() < 2 {
            return Ok(true);
        }
        let next = if forward {
            (self.selected_group + 1) % self.groups.len()
        } else {
            (self.selected_group + self.groups.len() - 1) % self.groups.len()
        };
        let symbols = &self.groups[next].symbols;
        let quotes = if symbols.is_empty() {
            Vec::new()
        } else {
            api.quotes(symbols).await?
        };
        let market_states = if symbols.is_empty() {
            Vec::new()
        } else {
            relevant_states(api.market_states(symbols).await?, symbols)
        };
        let next_subscriptions: Vec<String> = quotes
            .iter()
            .filter(|quote| quote.error.is_none())
            .map(|quote| quote.symbol.clone())
            .collect();
        let added: Vec<String> = next_subscriptions
            .iter()
            .filter(|symbol| !self.subscriptions.contains(symbol))
            .cloned()
            .collect();
        let removed: Vec<String> = self
            .subscriptions
            .iter()
            .filter(|symbol| !next_subscriptions.contains(symbol))
            .cloned()
            .collect();
        if !added.is_empty()
            && let Err(error) = api.subscribe_quotes(&added).await
        {
            let cleanup = tokio::time::timeout(
                crate::runtime::CLEANUP_TIMEOUT,
                api.unsubscribe_quotes(&added),
            )
            .await;
            if !matches!(cleanup, Ok(Ok(()))) {
                self.subscriptions.extend(added);
            }
            return Err(error);
        }
        if !removed.is_empty()
            && let Err(error) = api.unsubscribe_quotes(&removed).await
        {
            let _ = tokio::time::timeout(
                crate::runtime::CLEANUP_TIMEOUT,
                api.subscribe_quotes(&removed),
            )
            .await;
            let cleanup = tokio::time::timeout(
                crate::runtime::CLEANUP_TIMEOUT,
                api.unsubscribe_quotes(&added),
            )
            .await;
            if !matches!(cleanup, Ok(Ok(()))) {
                self.subscriptions.extend(added);
            }
            return Err(error);
        }
        self.subscriptions = next_subscriptions;
        self.selected_group = next;
        self.quotes = quotes;
        self.market_states = market_states;
        state.selected = 0;
        state.offset = 0;
        Ok(true)
    }

    fn help(&self) -> &'static str {
        "q / Ctrl+C  Quit\n?  Help\nj / k / Ctrl+N / Ctrl+P / arrows  Move\nPageUp / PageDown  Page\ng / G  First / last\n[ / ] or Left / Right  Group\nMarket: ◌ Pre · ▶ Trading · ■ Closed\n? Unknown market status"
    }

    fn draw(
        &self,
        frame: &mut ratatui::Frame<'_>,
        state: &mut crate::runtime::ViewState,
        settings: &Settings,
    ) {
        let mut table = self.display_table(settings.icon_theme);
        table.headers.push("Error");
        for (row, quote) in table.rows.iter_mut().zip(&self.quotes) {
            row.push(json!(quote.error));
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
}

pub async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    args: &WatchlistArgs,
    api: &QuoteServices,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    mut input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let Some(mut view) = crate::runtime::cancellable(initial_view(args, api), &mut input).await?
    else {
        return Ok(());
    };
    let result = async {
        let Some(market) = crate::runtime::cancellable(api.take_events(), &mut input).await? else {
            return Ok(());
        };
        view.subscriptions = view
            .quotes
            .iter()
            .filter(|quote| quote.error.is_none())
            .map(|quote| quote.symbol.clone())
            .collect();
        if !view.subscriptions.is_empty()
            && crate::runtime::cancellable(api.subscribe_quotes(&view.subscriptions), &mut input)
                .await?
                .is_none()
        {
            return Ok(());
        }
        crate::runtime::run(&mut view, api, settings, terminal, input, market, updates).await
    }
    .await;
    let cleanup = if view.subscriptions.is_empty() {
        Ok(())
    } else {
        tokio::time::timeout(
            crate::runtime::CLEANUP_TIMEOUT,
            api.unsubscribe_quotes(&view.subscriptions),
        )
        .await
        .unwrap_or_else(|_| Err(Error::network("watchlist subscription cleanup timed out")))
    };
    result.and(cleanup)
}
