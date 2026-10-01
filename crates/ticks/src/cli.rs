use std::{path::PathBuf, str::FromStr};

use clap::{Args, Parser, Subcommand};
use tickloop_ui::settings::{AmbiguousWidth, ColorMode, IconTheme, UnknownVariant};

use crate::{
    config::{Language, Overrides, Region, Settings},
    exit_code,
};

const ABOUT: &str =
    "Tick Loop Tick Commands: market, account and news views as standalone terminal commands";

const AFTER_HELP: &str = "Examples:
  tick auth
  tick watchlist
  tick quote TSLA.US 700.HK
  tick quote TSLA 700 --once --format json
  tick kline 700.HK --period day
  tick positions --once
  tick news 700.HK --limit 10
  tick earnings TSLA.US --count 12

Configuration is read from ~/.tickloop/config.toml; every --color-mode style
flag overrides the matching file field for this run only.

";

#[derive(Parser, Debug)]
#[command(name = "tick", version, about = ABOUT, after_help = after_help())]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

fn after_help() -> String {
    format!("{AFTER_HELP}{}", exit_code::REFERENCE)
}

#[derive(Args, Debug, Default)]
pub struct GlobalArgs {
    /// Configuration file to load instead of ~/.tickloop/config.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Which direction is painted red: red-up (default) or green-up
    #[arg(long, global = true, value_name = "MODE", value_parser = parse_variant::<ColorMode>)]
    pub color_mode: Option<ColorMode>,
    /// Status icon glyph set: unicode (default) or nerd
    #[arg(long, global = true, value_name = "THEME", value_parser = parse_variant::<IconTheme>)]
    pub icon_theme: Option<IconTheme>,
    /// Display language for API names and number units: zh-CN (default) or en
    #[arg(long, global = true, value_name = "LANG", value_parser = parse_variant::<Language>)]
    pub language: Option<Language>,
    /// Width of East Asian Ambiguous characters: narrow (default) or wide
    #[arg(long, global = true, value_name = "WIDTH", value_parser = parse_variant::<AmbiguousWidth>)]
    pub ambiguous_width: Option<AmbiguousWidth>,
    /// Longbridge data center: cn or global
    #[arg(long, global = true, value_name = "REGION", value_parser = parse_variant::<Region>)]
    pub region: Option<Region>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum Command {
    /// Connect an external Tick's stdin/stdout to Agent discovery inside Herdr
    Run {
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Print Tick descriptors and minimum Panel sizes as JSON; no login needed
    Describe,
    /// Run a real TUI with simulated data, without logging in or accessing the network
    Demo(DemoArgs),
    /// Log in to Longbridge with a device code, or inspect and clear the login
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Watchlist groups with live quotes
    Watchlist(WatchlistArgs),
    /// Quotes for one or more symbols
    Quote(QuoteArgs),
    /// Candlestick chart for one symbol
    Kline(KlineArgs),
    /// Stock positions with live profit and loss
    Positions(OnceArgs),
    /// News for one symbol or the whole market
    News(NewsArgs),
    /// Past earnings with estimates, surprises and the market reaction
    Earnings(EarningsArgs),
    /// Read original financial documents from URLs or local files
    Reader(ReaderArgs),
    /// Inspect Futu option pricing through a running OpenD connection
    Options(OptionsArgs),
    /// Record Longbridge fundamentals and calendar entitlements as a Markdown report
    #[command(hide = true)]
    ProbeFundamentals(ProbeArgs),
}

impl Command {
    pub fn kind(&self) -> Option<crate::metadata::TickKind> {
        use crate::metadata::TickKind;
        match self {
            Self::Watchlist(_) => Some(TickKind::Watchlist),
            Self::Quote(_) => Some(TickKind::Quote),
            Self::Kline(_) => Some(TickKind::Kline),
            Self::Positions(_) => Some(TickKind::Positions),
            Self::News(_) => Some(TickKind::News),
            Self::Earnings(_) => Some(TickKind::Earnings),
            Self::Reader(_) => Some(TickKind::Reader),
            Self::Options(_) => Some(TickKind::Options),
            Self::Run { .. }
            | Self::Describe
            | Self::Demo(_)
            | Self::Auth { .. }
            | Self::ProbeFundamentals(_) => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Describe => "describe",
            Self::Demo(_) => "demo",
            Self::Auth { .. } => "auth",
            Self::Watchlist(_) => "watchlist",
            Self::Quote(_) => "quote",
            Self::Kline(_) => "kline",
            Self::Positions(_) => "positions",
            Self::News(_) => "news",
            Self::Earnings(_) => "earnings",
            Self::Reader(_) => "reader",
            Self::Options(_) => "options",
            Self::ProbeFundamentals(_) => "probe-fundamentals",
        }
    }

    pub fn is_once(&self) -> bool {
        match self {
            Self::Quote(args) => args.output.once,
            Self::Watchlist(args) => args.output.once,
            Self::Kline(args) => args.output.once,
            Self::Positions(args) => args.once,
            Self::News(args) => args.output.once,
            Self::Earnings(args) => args.output.once,
            Self::Reader(args) => args.output.once,
            Self::Options(args) => args.output.once,
            Self::Demo(args) => args.output.once,
            Self::ProbeFundamentals(_) => true,
            Self::Run { .. } | Self::Describe | Self::Auth { .. } => false,
        }
    }
}

#[derive(Subcommand, Clone, Copy, Debug)]
pub enum AuthAction {
    /// Show login state, expiry time and region without refreshing
    Status,
    /// Verify stored authorization for a noninteractive workspace launch
    #[command(hide = true)]
    Check,
    /// Delete the token stored on this computer
    Logout,
}

impl GlobalArgs {
    pub fn overrides(&self) -> Overrides {
        Overrides {
            color_mode: self.color_mode,
            icon_theme: self.icon_theme,
            language: self.language,
            ambiguous_width: self.ambiguous_width,
            region: self.region,
        }
    }

    pub fn settings(&self) -> Result<Settings, String> {
        let (path, is_explicit) = match &self.config {
            Some(path) => (path.clone(), true),
            None => match crate::config::default_path() {
                Some(path) => (path, false),
                None => return Ok(Settings::default().with_overrides(self.overrides())),
            },
        };
        Settings::load(&path, is_explicit).map(|settings| settings.with_overrides(self.overrides()))
    }
}

fn parse_variant<T: FromStr<Err = UnknownVariant>>(text: &str) -> Result<T, String> {
    text.parse()
        .map_err(|error: UnknownVariant| error.to_string())
}

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "Keys: q/Ctrl+C quit; Esc back; ? help; j/k or arrows move; PageUp/PageDown; g/G.\nExamples: tick quote TSLA 700 --once --format json"
)]
pub struct QuoteArgs {
    #[arg(required = true, num_args = 1..)]
    pub symbols: Vec<String>,
    #[command(flatten)]
    pub output: OnceArgs,
}

#[derive(Args, Clone, Debug, Default)]
#[command(
    after_help = "Keys: [/] or Left/Right switch groups; q quits; Esc back; ? help.\nExample: tick watchlist --group Technology --once --format json"
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
    after_help = "Keys: Enter opens article; Esc returns to list; q quits; ? help.\nExample: tick news 700.HK --limit 10 --once --format json"
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
    after_help = "Keys: s changes the symbol; q quits; Esc back; ? help; j/k or arrows move.\nExample: tick earnings TSLA.US --count 12 --once --format json"
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
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 11111)]
    pub port: u16,
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
    after_help = "The probe only reads; it never writes to the account.\nExample: tick probe-fundamentals TSLA.US --out .scratch/earnings-tick/probe.md"
)]
pub struct ProbeArgs {
    /// Symbol to probe; the Earnings Tick covers US symbols only
    #[arg(default_value = crate::probe::DEFAULT_SYMBOL)]
    pub symbol: String,
    /// Anchor the probed windows at YYYY-MM-DD instead of today
    #[arg(long, value_name = "DATE", value_parser = parse_probe_date)]
    pub as_of: Option<time::Date>,
    /// Write the Markdown report to PATH instead of standard output
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

pub fn parse_probe_date(value: &str) -> Result<time::Date, String> {
    let expected = || format!("expected a date as YYYY-MM-DD, got {value}");
    let mut parts = value.split('-');
    let mut next = |bounds: std::ops::RangeInclusive<i64>| {
        parts
            .next()
            .and_then(|part| part.parse::<i64>().ok())
            .filter(|number| bounds.contains(number))
            .ok_or_else(expected)
    };
    let year = next(0..=9999)? as i32;
    let month = next(1..=12)? as u8;
    let day = next(1..=31)? as u8;
    if parts.next().is_some() {
        return Err(expected());
    }
    let month = time::Month::try_from(month).map_err(|_| expected())?;
    time::Date::from_calendar_date(year, month, day).map_err(|_| expected())
}

#[derive(Args, Clone, Debug)]
pub struct DemoArgs {
    #[arg(value_enum, default_value = "quote")]
    pub tick: crate::metadata::TickKind,
    /// Limit the demo Panel to COLUMNSxROWS, e.g. 40x12
    #[arg(long, value_parser = parse_size)]
    pub size: Option<ratatui::layout::Size>,
    #[command(flatten)]
    pub output: OnceArgs,
}

fn parse_size(value: &str) -> Result<ratatui::layout::Size, String> {
    let parse = || {
        let (columns, rows) = value.split_once('x')?;
        let width: u16 = columns.parse().ok()?;
        let height: u16 = rows.parse().ok()?;
        (width > 0 && height > 0).then_some(ratatui::layout::Size::new(width, height))
    };
    parse().ok_or_else(|| "expected positive COLUMNSxROWS, e.g. 40x12".into())
}

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "Keys: Left/Right select candle; 1-8 switch period; m toggles moving averages; q quits; Esc back; ? help.\nExample: tick kline 700.HK --period 5m --count 80 --adjust forward"
)]
pub struct KlineArgs {
    pub symbol: String,
    #[arg(long, value_enum, default_value = "day")]
    pub period: crate::api::CandlePeriod,
    #[arg(long, value_parser = parse_candle_count)]
    pub count: Option<usize>,
    #[arg(long, value_enum, default_value = "none")]
    pub adjust: crate::api::CandleAdjust,
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
