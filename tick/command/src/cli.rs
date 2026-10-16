use std::{fmt, path::PathBuf, str::FromStr};

use clap::{Args, Subcommand};
use tick_market::{
    args::{
        CommunityArgs, EarningsArgs, KlineArgs, NewsArgs, OnceArgs, OptionsArgs, QuoteArgs,
        WatchlistArgs,
    },
    config::{Overrides, Settings},
    metadata::TickKind,
};
use tick_provider::{
    error::{Error, Result},
    preferences::Language,
};
use tickloop_ui::settings::{AmbiguousWidth, ColorMode};

/// The note on configuration every Tick Command's help carries after its examples.
pub const CONFIGURATION_HELP: &str =
    "Configuration is read from ~/.tickloop/config.toml; every --color-mode style
flag overrides the matching file field for this run only.";

/// The display flags every Tick Command takes.
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
    /// Limit the Panel to COLUMNSxROWS, e.g. 40x12
    #[arg(long, global = true, value_name = "COLUMNSxROWS", value_parser = parse_size)]
    pub size: Option<ratatui::layout::Size>,
}

/// A market Tick with its arguments, as [`crate::entry::run_tick`] runs it. Each program parses
/// its own Ticks, with their help, and converts them; `tick-mock` parses every one as is.
#[derive(Subcommand, Clone, Debug)]
pub enum TickCommand {
    Quote(QuoteArgs),
    Watchlist(WatchlistArgs),
    Kline(KlineArgs),
    Positions(OnceArgs),
    News(NewsArgs),
    Earnings(EarningsArgs),
    Options(OptionsArgs),
    Community(CommunityArgs),
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

impl GlobalArgs {
    fn overrides(&self) -> Overrides {
        Overrides {
            color_mode: self.color_mode,
            language: self.language,
            ambiguous_width: self.ambiguous_width,
        }
    }

    pub fn settings(&self) -> Result<Settings> {
        let (path, is_explicit) = match &self.config {
            Some(path) => (path.clone(), true),
            None => match tick_market::config::default_path() {
                Some(path) => (path, false),
                None => return Ok(Settings::default().with_overrides(self.overrides())),
            },
        };
        Settings::load(&path, is_explicit)
            .map(|settings| settings.with_overrides(self.overrides()))
            .map_err(|error| Error::usage(format!("configuration error: {error}")))
    }
}

pub fn parse_variant<T: FromStr<Err: fmt::Display>>(text: &str) -> std::result::Result<T, String> {
    text.parse().map_err(|error: T::Err| error.to_string())
}

fn parse_size(value: &str) -> std::result::Result<ratatui::layout::Size, String> {
    let parse = || {
        let (columns, rows) = value.split_once('x')?;
        let width: u16 = columns.parse().ok()?;
        let height: u16 = rows.parse().ok()?;
        (width > 0 && height > 0).then_some(ratatui::layout::Size::new(width, height))
    };
    parse().ok_or_else(|| "expected positive COLUMNSxROWS, e.g. 40x12".into())
}
