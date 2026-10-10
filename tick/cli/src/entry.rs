use std::{ffi::OsString, io::IsTerminal, process::ExitCode, sync::Arc};

use clap::Parser;
use futures_util::Stream;
use tick_builtin::{DemoApi, DemoOptions};
use tick_futu::FutuOptions;
use tick_longbridge::{
    auth::{self, storage::AuthStorage},
    news_api::SdkNewsApi,
    rate_limit::RateLimiter,
    sdk::{SdkQuoteApi, SdkTradeApi},
};
use tick_market::{
    args::{OnceArgs, ReaderArgs},
    commands,
    config::Settings,
    metadata::TickKind,
    reader::Library,
    terminal::{TerminalSession, VisibleTerminalSession},
};
use tick_provider::{
    api::{QuoteServices, TradeApi},
    error::{Error, Result},
    exit_code,
    news::NewsApi,
    options::OptionsApi,
    preferences::Region,
};
use tick_sdk::stream::TickStream;

use crate::{
    cli::{
        BuiltinCommand, Cli, Command, FutuConnection, LongbridgeCommand, ProbeArgs, TickCommand,
    },
    locale, login,
};

const DEMO_DOCUMENT: &str = "demo://earnings/FY2026Q2";
const DEMO_SYMBOL: &str = "DEMO.US";
const DEMO_FISCAL_PERIOD: &str = "FY2026Q2";

pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let cli = match Cli::try_parse_from(std::iter::once(OsString::from("tick")).chain(arguments)) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(exit_code as u8);
        }
    };
    if let Some((provider, tick)) = cli.command.provider_tick()
        && let Err(error) = provider.check(tick.kind())
    {
        eprintln!("tick: {error}");
        return ExitCode::from(exit_code::USAGE);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("tick: could not start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(execute(cli));
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    match result {
        Ok(()) => ExitCode::from(exit_code::SUCCESS),
        Err(error) => {
            eprintln!("tick: {error}");
            ExitCode::from(error.code)
        }
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let native = std::env::var_os(tick_protocol::INSTANCE_ID_ENV).is_some();
    if native && (cli.command.is_once() || cli.command.is_separate_entry_point()) {
        return Err(Error::usage(
            "Native Tick stdio requires a live Tick; --once, describe, auth and probe are separate entry points",
        ));
    }
    if matches!(cli.command, Command::Describe) {
        return describe();
    }
    locale::check_utf8().map_err(|error| Error::usage(error.to_string()))?;
    let settings = cli
        .global
        .settings()
        .map_err(|error| Error::usage(format!("configuration error: {error}")))?;
    let size = cli.global.size;
    match cli.command {
        Command::Builtin { command } => run_builtin(command, settings, size).await,
        Command::Futu { connection, tick } => run_futu(connection, tick, &settings, size).await,
        Command::Reader(args) => run_reader(args, &settings, size).await,
        Command::Longbridge { command } => {
            let config_path = cli
                .global
                .config
                .clone()
                .or_else(tick_market::config::default_path)
                .ok_or_else(|| Error::usage("cannot determine configuration path"))?;
            run_longbridge(command, settings, &config_path, size).await
        }
        Command::Describe => Err(Error::usage("expected a market Tick Command")),
    }
}

fn describe() -> Result<()> {
    let descriptors: Vec<_> = TickKind::ALL
        .into_iter()
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

async fn run_builtin(
    command: BuiltinCommand,
    settings: Settings,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let settings = Settings {
        is_demo: true,
        ..settings
    };
    match command {
        BuiltinCommand::Tick(tick) => {
            let api = DemoApi::new()?;
            let services = Services {
                quote: Some(&api),
                trade: Some(&api),
                news: Some(&api),
                options: Some(Arc::new(DemoOptions)),
            };
            let running = Running::Provider(tick, services);
            run_tick(running, &settings, size).await
        }
        BuiltinCommand::Reader(output) => {
            let library = tick_market::reader::demo_library();
            let running = Running::Reader(demo_reader_args(output), library);
            run_tick(running, &settings, size).await
        }
    }
}

/// The simulated earnings release the builtin Reader opens.
fn demo_reader_args(output: OnceArgs) -> ReaderArgs {
    ReaderArgs {
        check: false,
        documents: vec![DEMO_DOCUMENT.into()],
        symbol: Some(DEMO_SYMBOL.into()),
        fiscal_period: Some(DEMO_FISCAL_PERIOD.into()),
        published_at: None,
        output,
    }
}

async fn run_futu(
    connection: FutuConnection,
    tick: TickCommand,
    settings: &Settings,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let api: Arc<dyn OptionsApi> = Arc::new(FutuOptions::lazy(&connection.host, connection.port)?);
    if let TickCommand::Options(args) = &tick
        && args.check
    {
        let request = tick_market::options::OptionsRequest::new(
            &args.symbol,
            args.expiry.as_deref(),
            args.strikes,
        )?;
        tick_market::options::load(api.as_ref(), &request).await?;
        return Ok(());
    }
    let services = Services {
        quote: None,
        trade: None,
        news: None,
        options: Some(api),
    };
    run_tick(Running::Provider(tick, services), settings, size).await
}

async fn run_reader(
    args: ReaderArgs,
    settings: &Settings,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let context = tick_market::research::ResearchContext::from_environment()?;
    let library = if args.check || args.output.once {
        tick_market::reader::load(&args).await?
    } else {
        tick_market::reader::load_for_context(&args, context.as_ref()).await?
    };
    if args.check {
        return Ok(());
    }
    run_tick(Running::Reader(args, library), settings, size).await
}

async fn run_longbridge(
    command: LongbridgeCommand,
    mut settings: Settings,
    config_path: &std::path::Path,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let storage = auth::default_storage()?;
    match command {
        LongbridgeCommand::Auth {
            action: Some(action),
        } => login::inspect(action, &storage, settings.region, &mut std::io::stdout()),
        LongbridgeCommand::Auth { action: None } => {
            match interactive_login(&settings, config_path, &storage).await? {
                login::LoginOutcome::Authorized(_) => {
                    println!("Longbridge authorization saved.");
                    Ok(())
                }
                login::LoginOutcome::Cancelled => Ok(()),
            }
        }
        LongbridgeCommand::ProbeFundamentals(args) => {
            run_fundamentals_probe(&args, &settings, &storage).await
        }
        LongbridgeCommand::Tick(tick) => {
            run_longbridge_tick(tick, &mut settings, config_path, &storage, size).await
        }
    }
}

/// Runs a Longbridge Tick, opening the login screen first, and again whenever authorization
/// lapses, when the Tick runs interactively.
async fn run_longbridge_tick(
    tick: TickCommand,
    settings: &mut Settings,
    config_path: &std::path::Path,
    storage: &AuthStorage,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let interactive = std::env::var_os(auth::NONINTERACTIVE_AUTH_ENV).is_none()
        && !tick.is_once()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let mut login_required = false;
    loop {
        if login_required {
            match interactive_login(settings, config_path, storage).await? {
                login::LoginOutcome::Authorized(region) => settings.region = Some(region),
                login::LoginOutcome::Cancelled => return Ok(()),
            }
        }
        let region = settings.region.unwrap_or(Region::Global);
        let oauth = match auth::authorized_oauth(storage, region, None).await {
            Ok(oauth) => oauth,
            Err(error) if error.code == exit_code::NOT_AUTHORIZED && interactive => {
                login_required = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        match run_longbridge_market(tick.clone(), oauth, settings, size).await {
            Err(error) if error.code == exit_code::NOT_AUTHORIZED && interactive => {
                login_required = true;
            }
            result => return result,
        }
    }
}

async fn run_longbridge_market(
    tick: TickCommand,
    oauth: longbridge::oauth::OAuth,
    settings: &Settings,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let token = oauth
        .access_token()
        .await
        .map_err(|_| Error::auth("authorization expired"))?;
    let is_us = longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us;
    let config =
        tick_longbridge::sdk::config(oauth.clone(), settings.language, settings.region).await?;
    let limiter = Arc::new(RateLimiter::default());
    let api = SdkQuoteApi::new(
        config.clone(),
        oauth.clone(),
        settings.language,
        settings.region,
        limiter.clone(),
    )
    .await?;
    let (trade_context, _trade_events) = longbridge::trade::TradeContext::new(config.clone());
    let trade = SdkTradeApi {
        context: trade_context,
        limiter: limiter.clone(),
        is_us,
    };
    let news = if matches!(tick, TickCommand::News(_)) {
        Some(SdkNewsApi::new(config, oauth, settings.language, settings.region, limiter).await?)
    } else {
        None
    };
    let services = Services {
        quote: Some(&api),
        trade: Some(&trade),
        news: news.as_ref().map(|news| news as &dyn NewsApi),
        options: None,
    };
    run_tick(Running::Provider(tick, services), settings, size).await
}

async fn run_fundamentals_probe(
    args: &ProbeArgs,
    settings: &Settings,
    storage: &AuthStorage,
) -> Result<()> {
    let region = settings.region.unwrap_or(Region::Global);
    let oauth = auth::authorized_oauth(storage, region, None).await?;
    let token = oauth
        .access_token()
        .await
        .map_err(|_| Error::auth("authorization expired"))?;
    let data_center = longbridge::DcRegion::from_credential(&token)
        .as_str()
        .to_owned();
    let config = tick_longbridge::sdk::config(oauth, settings.language, settings.region).await?;
    tick_longbridge::probe::run(
        &args.symbol,
        args.as_of,
        args.out.as_deref(),
        config,
        Arc::new(RateLimiter::default()),
        &data_center,
    )
    .await
}

async fn interactive_login(
    settings: &Settings,
    config_path: &std::path::Path,
    storage: &AuthStorage,
) -> Result<login::LoginOutcome> {
    let mut session = TerminalSession::enter()?;
    let input = tick_market::terminal::input_events()?;
    login::run(
        settings,
        config_path,
        storage,
        None,
        &mut session.terminal,
        input,
    )
    .await
}

/// The provider services a Tick reads through; a provider leaves out the interfaces it does not
/// implement, and [`crate::cli::Provider::check`] has refused every Tick that needs one of them.
struct Services<'a> {
    quote: Option<&'a QuoteServices>,
    trade: Option<&'a dyn TradeApi>,
    news: Option<&'a dyn NewsApi>,
    options: Option<Arc<dyn OptionsApi>>,
}

impl<'a> Services<'a> {
    fn quote(&self) -> &'a QuoteServices {
        self.quote.expect("checked provider implements QuoteApi")
    }

    fn trade(&self) -> &'a dyn TradeApi {
        self.trade.expect("checked provider implements TradeApi")
    }

    fn news(&self) -> &'a dyn NewsApi {
        self.news.expect("checked provider implements NewsApi")
    }

    fn options(&self) -> Arc<dyn OptionsApi> {
        self.options
            .clone()
            .expect("checked provider implements OptionsApi")
    }
}

/// A Tick and what it reads: a provider Tick with its services, or the Reader with its documents.
enum Running<'a> {
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
async fn run_tick(
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

async fn run_live<
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
    }
}
