use std::{ffi::OsString, future::Future, io::IsTerminal, process::ExitCode, sync::Arc};

use clap::Parser;
use futures_util::Stream;
use tick_market::{
    args::ReaderArgs,
    commands,
    config::Settings,
    metadata::TickKind,
    reader::Library,
    terminal::{TerminalSession, VisibleTerminalSession},
};
use tick_provider::{
    api::{QuoteServices, TradeApi},
    community::CommunityApi,
    error::{Error, Result},
    exit_code,
    news::NewsApi,
    options::OptionsApi,
};
use tick_sdk::stream::TickStream;

use crate::{cli::TickCommand, locale};

/// Runs a Tick Command: parses its command line as `C`, runs `execute` on an async runtime and
/// exits with the code of the outcome, naming the program when it fails.
pub fn run<C: Parser, F: Future<Output = Result<()>>>(execute: impl FnOnce(C) -> F) -> ExitCode {
    let program = C::command().get_name().to_owned();
    let arguments = std::iter::once(OsString::from(&program)).chain(std::env::args_os().skip(1));
    let cli = match C::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(exit_code as u8);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{program}: could not start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(execute(cli));
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    match result {
        Ok(()) => ExitCode::from(exit_code::SUCCESS),
        Err(error) => {
            eprintln!("{program}: {error}");
            ExitCode::from(error.code)
        }
    }
}

/// Whether a host launched this process as a Tick Instance.
pub fn in_instance() -> bool {
    std::env::var_os(tick_protocol::INSTANCE_ID_ENV).is_some()
}

/// Refuses to run a command that runs once and exits, such as `--once` or `describe`, in a Tick
/// Instance: the host launched this process for a live Tick.
pub fn refuse_in_instance() -> Result<()> {
    if in_instance() {
        return Err(Error::usage(
            "Native Tick stdio requires a live Tick; --once, describe and the other one-shot commands are separate entry points",
        ));
    }
    Ok(())
}

/// Prints the descriptors of a program's Ticks as JSON, refusing one that does not validate.
pub fn describe(kinds: &[TickKind]) -> Result<()> {
    let descriptors: Vec<_> = kinds
        .iter()
        .map(|tick| {
            let descriptor = tick.descriptor();
            descriptor.validate().map(|()| descriptor)
        })
        .collect::<Result<_>>()?;
    serde_json::to_writer_pretty(std::io::stdout(), &descriptors)
        .map_err(|error| Error::usage(error.to_string()))?;
    println!();
    Ok(())
}

pub fn check_locale() -> Result<()> {
    locale::check_utf8().map_err(|error| Error::usage(error.to_string()))
}

/// The provider services a Tick reads through; a program leaves out the interfaces none of its
/// Ticks read.
pub struct Services<'a> {
    pub quote: Option<&'a QuoteServices>,
    pub trade: Option<&'a dyn TradeApi>,
    pub news: Option<&'a dyn NewsApi>,
    pub options: Option<Arc<dyn OptionsApi>>,
    pub community: Option<Arc<dyn CommunityApi>>,
}

impl<'a> Services<'a> {
    fn quote(&self) -> &'a QuoteServices {
        self.quote
            .expect("a program serves QuoteApi to its Ticks that read it")
    }

    fn trade(&self) -> &'a dyn TradeApi {
        self.trade
            .expect("a program serves TradeApi to its Ticks that read it")
    }

    fn news(&self) -> &'a dyn NewsApi {
        self.news
            .expect("a program serves NewsApi to its Ticks that read it")
    }

    fn options(&self) -> Arc<dyn OptionsApi> {
        self.options
            .clone()
            .expect("a program serves OptionsApi to its Ticks that read it")
    }

    fn community(&self) -> Arc<dyn CommunityApi> {
        self.community
            .clone()
            .expect("a program serves CommunityApi to its Ticks that read it")
    }
}

/// A Tick and what it reads: a market Tick with its services, or the Reader with its documents.
pub enum Running<'a> {
    Provider(TickCommand, Services<'a>),
    Reader(ReaderArgs, Library),
}

impl Running<'_> {
    fn kind(&self) -> TickKind {
        match self {
            Self::Provider(tick, _) => tick.kind(),
            Self::Reader(..) => TickKind::Reader,
        }
    }

    fn is_once(&self) -> bool {
        match self {
            Self::Provider(tick, _) => tick.is_once(),
            Self::Reader(args, _) => args.output.once,
        }
    }
}

/// Runs a Tick in the presentation its invocation asks for: a one-shot snapshot, the visible
/// terminal of a hosted Tick, or a standalone terminal session.
pub async fn run_tick(
    running: Running<'_>,
    settings: &Settings,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let mut updates = TickStream::from_process(running.kind())?;
    let result = if running.is_once() {
        let mut output = std::io::stdout();
        let color = output.is_terminal();
        run_once(running, settings, color, &mut output, &mut updates).await
    } else if updates.is_native() {
        let mut session = VisibleTerminalSession::enter(size)?;
        let input = tick_market::terminal::visible_input_events()?;
        run_live(
            running,
            settings,
            &mut session.terminal,
            input,
            &mut updates,
        )
        .await
    } else {
        let mut session = TerminalSession::enter_with_size(size)?;
        let input = tick_market::terminal::input_events()?;
        run_live(
            running,
            settings,
            &mut session.terminal,
            input,
            &mut updates,
        )
        .await
    };
    updates.close().await;
    result
}

pub(crate) async fn run_live<
    B: ratatui::backend::Backend,
    S: Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    running: Running<'_>,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let (tick, services) = match running {
        Running::Provider(tick, services) => (tick, services),
        Running::Reader(_, library) => {
            return commands::reader::run_live(library, settings, terminal, input, updates).await;
        }
    };
    match tick {
        TickCommand::Quote(args) => {
            commands::quote::run_live(&args, services.quote(), settings, terminal, input, updates)
                .await
        }
        TickCommand::Watchlist(args) => {
            commands::watchlist::run_live(
                &args,
                services.quote(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        TickCommand::Kline(args) => {
            commands::kline::run_live(&args, services.quote(), settings, terminal, input, updates)
                .await
        }
        TickCommand::Positions(args) => {
            commands::positions::run_live(
                &args,
                services.quote(),
                services.trade(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        TickCommand::News(args) => {
            commands::news::run_live(
                &args,
                services.quote(),
                services.news(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        TickCommand::Earnings(args) => {
            commands::earnings::run_live(
                &args,
                services.quote(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        TickCommand::Options(args) => {
            commands::options::run_live(
                &args,
                services.options(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        TickCommand::Community(args) => {
            commands::community::run_live(
                &args,
                services.community(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
    }
}

async fn run_once(
    running: Running<'_>,
    settings: &Settings,
    color: bool,
    output: &mut impl std::io::Write,
    updates: &mut TickStream,
) -> Result<()> {
    let (tick, services) = match running {
        Running::Provider(tick, services) => (tick, services),
        Running::Reader(args, library) => {
            return commands::reader::run_once(&args, library, settings, color, output, updates)
                .await;
        }
    };
    match tick {
        TickCommand::Quote(args) => {
            commands::quote::run_once(&args, services.quote(), settings, color, output, updates)
                .await
        }
        TickCommand::Watchlist(args) => {
            commands::watchlist::run_once(&args, services.quote(), settings, color, output, updates)
                .await
        }
        TickCommand::Kline(args) => {
            commands::kline::run_once(&args, services.quote(), settings, color, output, updates)
                .await
        }
        TickCommand::Positions(args) => {
            commands::positions::run_once(
                &args,
                services.quote(),
                services.trade(),
                settings,
                color,
                output,
                updates,
            )
            .await
        }
        TickCommand::News(args) => {
            commands::news::run_once(&args, services.news(), settings, color, output, updates).await
        }
        TickCommand::Earnings(args) => {
            commands::earnings::run_once(&args, services.quote(), settings, color, output, updates)
                .await
        }
        TickCommand::Options(args) => {
            commands::options::run_once(&args, services.options(), settings, color, output, updates)
                .await
        }
        TickCommand::Community(args) => {
            commands::community::run_once(
                &args,
                services.community().as_ref(),
                settings,
                color,
                output,
                updates,
            )
            .await
        }
    }
}
