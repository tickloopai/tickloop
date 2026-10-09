//! The arguments of each financial Tick, shared by every provider that runs it.

use std::path::PathBuf;

use clap::Args;

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "Keys: q/Ctrl+C quit; ? help; j/k or arrows move; PageUp/PageDown; g/G.\nExamples: tick longbridge quote TSLA 700 --once --format json"
)]
pub struct QuoteArgs {
    #[arg(required = true, num_args = 1..)]
    pub symbols: Vec<String>,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug, Default)]
#[command(
    after_help = "Keys: [/] or Left/Right switch groups; q quits; ? help.\nExample: tick longbridge watchlist --group Technology --once --format json"
)]
pub struct WatchlistArgs {
    /// Watchlist group ID or name (defaults to the first group)
    #[arg(long)]
    pub group: Option<String>,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "Keys: Enter opens article; Backspace returns to list; q quits; ? help.\nExample: tick longbridge news 700.HK --limit 10 --once --format json"
)]
pub struct NewsArgs {
    pub symbol: Option<String>,
    #[arg(long, default_value_t = 20, value_parser = parse_news_limit)]
    pub limit: usize,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "Keys: s changes the symbol (Enter applies, Ctrl+C cancels); q quits; Backspace back; ? help; j/k or arrows move.\nExample: tick longbridge earnings TSLA.US --count 12 --once --format json"
)]
pub struct EarningsArgs {
    /// US symbol, for example TSLA.US
    pub symbol: String,
    /// Released periods to show, newest first
    #[arg(long, value_parser = parse_earnings_count)]
    pub count: Option<usize>,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug)]
pub struct ReaderArgs {
    /// Validate document access and extraction without starting the Tick
    #[arg(long, hide = true)]
    pub check: bool,
    /// HTTP(S) URLs or local HTML, Markdown, text or PDF files
    #[arg(num_args = 0..)]
    pub documents: Vec<String>,
    #[arg(long)]
    pub symbol: Option<String>,
    #[arg(long)]
    pub fiscal_period: Option<String>,
    #[arg(long)]
    pub published_at: Option<String>,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug)]
pub struct OptionsArgs {
    /// Verify OpenD access and the initial option chain without starting the Tick
    #[arg(long, hide = true)]
    pub check: bool,
    pub symbol: String,
    #[arg(long)]
    pub expiry: Option<String>,
    #[arg(long)]
    pub snapshot_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 21, value_parser = parse_option_strikes)]
    pub strikes: usize,
    #[command(flatten)]
    pub output: OnceArgs,
}

fn parse_option_strikes(value: &str) -> Result<usize, String> {
    parse_count(value, 100)
}

fn parse_earnings_count(value: &str) -> Result<usize, String> {
    parse_count(value, crate::earnings::MAX_PERIOD_COUNT)
}

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "Keys: Left/Right select candle; 1-8 switch period; m toggles moving averages; q quits; Backspace back; ? help.\nExample: tick longbridge kline 700.HK --period 5m --count 80 --adjust forward"
)]
pub struct KlineArgs {
    pub symbol: String,
    #[arg(long, value_enum, default_value = "day")]
    pub period: tick_provider::api::CandlePeriod,
    #[arg(long, value_parser = parse_candle_count)]
    pub count: Option<usize>,
    #[arg(long, value_enum, default_value = "none")]
    pub adjust: tick_provider::api::CandleAdjust,
    #[command(flatten)]
    pub event: EventChartArgs,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug, Default)]
pub struct EventChartArgs {
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub event_date: Option<String>,
    #[arg(long, value_enum, default_value = "unknown", requires = "event_date")]
    pub event_session: crate::event_chart::EventSession,
    /// Actual publication timestamp with UTC offset; never an estimated timestamp
    #[arg(long, requires = "event_date")]
    pub published_at: Option<String>,
    #[arg(long, requires = "event_date")]
    pub conference_at: Option<String>,
    #[arg(long, value_enum, default_value = "line")]
    pub chart_mode: crate::event_chart::EventChartMode,
    #[arg(long, value_enum, default_value = "auto")]
    pub event_range: crate::event_chart::EventRange,
}

fn parse_news_limit(value: &str) -> Result<usize, String> {
    parse_count(value, 100)
}
fn parse_candle_count(value: &str) -> Result<usize, String> {
    parse_count(value, 1000)
}
fn parse_count(value: &str, maximum: usize) -> Result<usize, String> {
    let count: usize = value
        .parse()
        .map_err(|_| format!("expected a count from 1 to {maximum}"))?;
    if (1..=maximum).contains(&count) {
        Ok(count)
    } else {
        Err(format!("expected a count from 1 to {maximum}"))
    }
}

#[derive(Args, Clone, Debug, Default)]
pub struct OnceArgs {
    /// Print a snapshot and exit; no TTY needed
    #[arg(long)]
    pub once: bool,
    /// Snapshot output format
    #[arg(long, value_enum, default_value = "table", requires = "once")]
    pub format: OutputFormat,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}
