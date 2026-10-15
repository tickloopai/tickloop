use std::{fmt, path::PathBuf, str::FromStr};

use clap::{Args, Parser, Subcommand};
use tick_market::{
    args::{
        CommunityArgs, EarningsArgs, KlineArgs, NewsArgs, OnceArgs, OptionsArgs, QuoteArgs,
        ReaderArgs, WatchlistArgs,
    },
    config::{Overrides, Settings},
    metadata::TickKind,
};
use tick_provider::{
    Interface,
    preferences::{Language, Region},
};
use tickloop_ui::settings::{AmbiguousWidth, ColorMode};

const ABOUT: &str = "Tickloop Ticks: market, account, news and community views as standalone \
     terminal commands, run as `tick <provider> <tick>`";

const AFTER_HELP: &str = "Providers:
  builtin     simulated data for every Tick; no account or network
  longbridge  Longbridge quotes, account, news and community; log in with `tick longbridge auth`
  futu        option pricing from a running Futu OpenD

Examples:
  tick builtin kline 700.HK
  tick longbridge auth
  tick longbridge watchlist
  tick longbridge quote TSLA.US 700.HK
  tick longbridge quote TSLA 700 --once --format json
  tick longbridge kline 700.HK --period day
  tick longbridge positions --once
  tick longbridge news 700.HK --limit 10
  tick longbridge earnings TSLA.US --count 12
  tick longbridge community 91
  tick futu options AAPL.US --port 11111
  tick reader ./earnings-release.pdf

Configuration is read from ~/.tickloop/config.toml; every --color-mode style
flag overrides the matching file field for this run only.

";

const EXIT_CODES: &str = "Exit codes:
  0  success
  2  usage or configuration error
  3  not authorized; run `tick longbridge auth`
  4  network or API error";

#[derive(Parser, Debug)]
#[command(name = "tick", version, about = ABOUT, after_help = format!("{AFTER_HELP}{EXIT_CODES}"))]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Default)]
pub struct GlobalArgs {
    /// Configuration file to load instead of ~/.tickloop/config.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Which direction is painted red: red-up (default) or green-up
    #[arg(long, global = true, value_name = "MODE", value_parser = parse_variant::<ColorMode>)]
    pub color_mode: Option<ColorMode>,
    /// Display language for API names and number units: zh-CN (default) or en
    #[arg(long, global = true, value_name = "LANG", value_parser = parse_variant::<Language>)]
    pub language: Option<Language>,
    /// Width of East Asian Ambiguous characters: narrow (default) or wide
    #[arg(long, global = true, value_name = "WIDTH", value_parser = parse_variant::<AmbiguousWidth>)]
    pub ambiguous_width: Option<AmbiguousWidth>,
    /// Longbridge data center: cn or global
    #[arg(long, global = true, value_name = "REGION", value_parser = parse_variant::<Region>)]
    pub region: Option<Region>,
    /// Limit the Panel to COLUMNSxROWS, e.g. 40x12
    #[arg(long, global = true, value_name = "COLUMNSxROWS", value_parser = parse_size)]
    pub size: Option<ratatui::layout::Size>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum Command {
    /// Run a Tick on simulated data, without logging in or accessing the network
    Builtin {
        #[command(subcommand)]
        command: BuiltinCommand,
    },
    /// Run a Tick on Longbridge quotes, account, news and community, or manage the Longbridge login
    Longbridge {
        #[command(subcommand)]
        command: LongbridgeCommand,
    },
    /// Run a Tick on option pricing from a running Futu OpenD
    Futu {
        #[command(flatten)]
        connection: FutuConnection,
        #[command(subcommand)]
        tick: TickCommand,
    },
    /// Read original financial documents from URLs or local files
    Reader(ReaderArgs),
    /// Print Tick descriptors and minimum Panel sizes as JSON; no login needed
    Describe,
}

/// The financial Ticks a provider can run, given the interfaces it implements.
#[derive(Subcommand, Clone, Debug)]
pub enum TickCommand {
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
    /// Option pricing, IV and saved snapshots for one underlying
    Options(OptionsArgs),
    /// One community member's newest posts, with a tool to reply to them
    Community(CommunityArgs),
}

#[derive(Subcommand, Clone, Debug)]
pub enum BuiltinCommand {
    #[command(flatten)]
    Tick(TickCommand),
    /// Read a simulated earnings release in the Reader Tick
    Reader(OnceArgs),
}

#[derive(Subcommand, Clone, Debug)]
pub enum LongbridgeCommand {
    #[command(flatten)]
    Tick(TickCommand),
    /// Log in to Longbridge with a device code, or inspect and clear the login
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Record Longbridge fundamentals and calendar entitlements as a Markdown report
    #[command(hide = true)]
    ProbeFundamentals(ProbeArgs),
}

#[derive(Args, Clone, Debug)]
pub struct FutuConnection {
    /// OpenD host
    #[arg(long, global = true, default_value = "127.0.0.1")]
    pub host: String,
    /// OpenD port
    #[arg(long, global = true, default_value_t = 11111)]
    pub port: u16,
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

#[derive(Args, Clone, Debug)]
#[command(
    after_help = "The probe only reads; it never writes to the account.\nExample: tick longbridge probe-fundamentals TSLA.US --out .scratch/earnings-tick/probe.md"
)]
pub struct ProbeArgs {
    /// Symbol to probe; the Earnings Tick covers US symbols only
    #[arg(default_value = tick_longbridge::probe::DEFAULT_SYMBOL)]
    pub symbol: String,
    /// Anchor the probed windows at YYYY-MM-DD instead of today
    #[arg(long, value_name = "DATE", value_parser = parse_probe_date)]
    pub as_of: Option<time::Date>,
    /// Write the Markdown report to PATH instead of standard output
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

/// Where a provider Tick gets its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Builtin,
    Longbridge,
    Futu,
}

impl Provider {
    pub const ALL: [Self; 3] = [Self::Builtin, Self::Longbridge, Self::Futu];

    pub fn name(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Longbridge => "longbridge",
            Self::Futu => "futu",
        }
    }

    pub fn interfaces(self) -> &'static [Interface] {
        match self {
            Self::Builtin => tick_builtin::INTERFACES,
            Self::Longbridge => tick_longbridge::INTERFACES,
            Self::Futu => tick_futu::INTERFACES,
        }
    }

    fn missing(self, tick: TickKind) -> Vec<Interface> {
        tick.interfaces()
            .iter()
            .copied()
            .filter(|interface| !self.interfaces().contains(interface))
            .collect()
    }

    /// Refuses a Tick that reads an interface this provider does not implement, and names the
    /// providers that can run it instead.
    pub fn check(self, tick: TickKind) -> Result<(), UnsupportedTick> {
        let missing = self.missing(tick);
        if missing.is_empty() {
            return Ok(());
        }
        Err(UnsupportedTick {
            provider: self,
            tick,
            missing,
            alternatives: Self::ALL
                .into_iter()
                .filter(|provider| provider.missing(tick).is_empty())
                .collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedTick {
    pub provider: Provider,
    pub tick: TickKind,
    pub missing: Vec<Interface>,
    pub alternatives: Vec<Provider>,
}

impl fmt::Display for UnsupportedTick {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let missing: Vec<_> = self
            .missing
            .iter()
            .map(|interface| interface.trait_name())
            .collect();
        let tick = self.tick.name();
        write!(
            formatter,
            "{} does not implement {}, which the {tick} Tick needs",
            self.provider.name(),
            missing.join(" and ")
        )?;
        if !self.alternatives.is_empty() {
            let commands: Vec<_> = self
                .alternatives
                .iter()
                .map(|provider| format!("tick {} {tick}", provider.name()))
                .collect();
            write!(formatter, "; use {}", commands.join(" or "))?;
        }
        Ok(())
    }
}

impl TickCommand {
    pub fn kind(&self) -> TickKind {
        match self {
            Self::Quote(_) => TickKind::Quote,
            Self::Watchlist(_) => TickKind::Watchlist,
            Self::Kline(_) => TickKind::Kline,
            Self::Positions(_) => TickKind::Positions,
            Self::News(_) => TickKind::News,
            Self::Earnings(_) => TickKind::Earnings,
            Self::Options(_) => TickKind::Options,
            Self::Community(_) => TickKind::Community,
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
            Self::Options(args) => args.output.once,
            Self::Community(args) => args.output.once,
        }
    }
}

impl Command {
    /// The provider and Tick a provider command runs.
    pub fn provider_tick(&self) -> Option<(Provider, &TickCommand)> {
        match self {
            Self::Builtin {
                command: BuiltinCommand::Tick(tick),
            } => Some((Provider::Builtin, tick)),
            Self::Longbridge {
                command: LongbridgeCommand::Tick(tick),
            } => Some((Provider::Longbridge, tick)),
            Self::Futu { tick, .. } => Some((Provider::Futu, tick)),
            _ => None,
        }
    }

    /// Commands that run once and exit instead of showing a live Tick.
    pub fn is_separate_entry_point(&self) -> bool {
        matches!(
            self,
            Self::Describe
                | Self::Longbridge {
                    command: LongbridgeCommand::Auth { .. }
                        | LongbridgeCommand::ProbeFundamentals(_),
                }
        )
    }

    pub fn is_once(&self) -> bool {
        match self {
            Self::Builtin {
                command: BuiltinCommand::Reader(args),
            } => args.once,
            Self::Reader(args) => args.output.once,
            Self::Longbridge {
                command: LongbridgeCommand::ProbeFundamentals(_),
            } => true,
            command => command
                .provider_tick()
                .is_some_and(|(_, tick)| tick.is_once()),
        }
    }
}

impl GlobalArgs {
    pub fn overrides(&self) -> Overrides {
        Overrides {
            color_mode: self.color_mode,
            language: self.language,
            ambiguous_width: self.ambiguous_width,
            region: self.region,
        }
    }

    pub fn settings(&self) -> Result<Settings, String> {
        let (path, is_explicit) = match &self.config {
            Some(path) => (path.clone(), true),
            None => match tick_market::config::default_path() {
                Some(path) => (path, false),
                None => return Ok(Settings::default().with_overrides(self.overrides())),
            },
        };
        Settings::load(&path, is_explicit).map(|settings| settings.with_overrides(self.overrides()))
    }
}

fn parse_variant<T: FromStr<Err: fmt::Display>>(text: &str) -> Result<T, String> {
    text.parse().map_err(|error: T::Err| error.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_runs_exactly_the_ticks_whose_interfaces_it_implements() {
        let runnable = |provider: Provider| -> Vec<_> {
            TickKind::ALL
                .into_iter()
                .filter(|tick| *tick != TickKind::Reader && provider.check(*tick).is_ok())
                .map(TickKind::name)
                .collect()
        };
        let every_provider_tick = [
            "quote",
            "watchlist",
            "kline",
            "positions",
            "news",
            "earnings",
            "options",
            "community",
        ];
        assert_eq!(runnable(Provider::Builtin), every_provider_tick);
        assert_eq!(
            runnable(Provider::Longbridge),
            [
                "quote",
                "watchlist",
                "kline",
                "positions",
                "news",
                "earnings",
                "community"
            ]
        );
        assert_eq!(runnable(Provider::Futu), ["options"]);
    }

    #[test]
    fn a_missing_interface_names_it_and_the_providers_that_have_it() {
        let error = Provider::Longbridge.check(TickKind::Options).unwrap_err();
        assert_eq!(error.missing, [Interface::Options]);
        assert_eq!(
            error.to_string(),
            "longbridge does not implement OptionsApi, which the options Tick needs; \
             use tick builtin options or tick futu options"
        );
        assert_eq!(
            Provider::Futu
                .check(TickKind::Positions)
                .unwrap_err()
                .to_string(),
            "futu does not implement QuoteApi and TradeApi, which the positions Tick needs; \
             use tick builtin positions or tick longbridge positions"
        );
    }
}
