use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tick_command::cli::{CONFIGURATION_HELP, GlobalArgs, TickCommand};
use tick_market::{
    args::{CommunityArgs, EarningsArgs, KlineArgs, NewsArgs, OnceArgs, QuoteArgs, WatchlistArgs},
    metadata::TickKind,
};

const ABOUT: &str = "Longbridge Ticks: quotes, watchlist, candlesticks, positions, news, earnings \
     and community posts as standalone terminal commands; log in with /ticks in the Agent or \
     `tickloop tick login longbridge`, or with `tick-longbridge auth`, which shares that login";

const EXAMPLES: &str = "Examples:
  tick-longbridge auth
  tick-longbridge watchlist
  tick-longbridge quote TSLA.US 700.HK
  tick-longbridge quote TSLA 700 --once --format json
  tick-longbridge kline 700.HK --period day
  tick-longbridge positions --once
  tick-longbridge news 700.HK --limit 10
  tick-longbridge earnings TSLA.US --count 12
  tick-longbridge community 91";

const EXIT_CODES: &str = "Exit codes:
  0  success
  2  usage or configuration error
  3  not authorized; log in with /ticks in the Agent, or `tickloop tick login longbridge` in a terminal
  4  network or API error";

/// The Ticks `tick-longbridge` runs, in the order `describe` lists them.
pub const TICKS: [TickKind; 7] = [
    TickKind::Quote,
    TickKind::Watchlist,
    TickKind::Kline,
    TickKind::Positions,
    TickKind::News,
    TickKind::Earnings,
    TickKind::Community,
];

#[derive(Parser, Debug)]
#[command(
    name = "tick-longbridge",
    version,
    about = ABOUT,
    after_help = format!("{EXAMPLES}\n\n{CONFIGURATION_HELP}\n\n{EXIT_CODES}")
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Clone, Debug)]
pub enum Command {
    #[command(flatten)]
    Tick(LongbridgeTick),
    /// Log in to Longbridge with a device code, or inspect and clear the login
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Record Longbridge fundamentals and calendar entitlements as a Markdown report
    #[command(hide = true)]
    ProbeFundamentals(ProbeArgs),
    /// Print Tick descriptors and minimum Panel sizes as JSON; no login needed
    Describe,
}

/// The Ticks that read Longbridge quotes, account, news and community posts.
#[derive(Subcommand, Clone, Debug)]
pub enum LongbridgeTick {
    /// Quotes for one or more symbols
    Quote(QuoteArgs),
    /// Watchlist groups with live quotes
    Watchlist(WatchlistArgs),
    /// Candlestick chart for one symbol
    Kline(KlineArgs),
    /// Stock positions with live profit and loss
    Positions(OnceArgs),
    /// News for one symbol or the whole market
    News(NewsArgs),
    /// Past earnings with estimates, surprises and the market reaction
    Earnings(EarningsArgs),
    /// One community member's newest posts, with a tool to reply to them
    Community(CommunityArgs),
}

impl From<LongbridgeTick> for TickCommand {
    fn from(tick: LongbridgeTick) -> Self {
        match tick {
            LongbridgeTick::Quote(args) => Self::Quote(args),
            LongbridgeTick::Watchlist(args) => Self::Watchlist(args),
            LongbridgeTick::Kline(args) => Self::Kline(args),
            LongbridgeTick::Positions(args) => Self::Positions(args),
            LongbridgeTick::News(args) => Self::News(args),
            LongbridgeTick::Earnings(args) => Self::Earnings(args),
            LongbridgeTick::Community(args) => Self::Community(args),
        }
    }
}

#[derive(Subcommand, Clone, Copy, Debug)]
pub enum AuthAction {
    /// Show login state, expiry time and region without refreshing
    Status,
    /// Delete the token saved on this computer, which Tickloop shares
    Logout,
}

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "The probe only reads; it never writes to the account.\nExample: tick-longbridge probe-fundamentals TSLA.US --out .scratch/earnings-tick/probe.md"
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
