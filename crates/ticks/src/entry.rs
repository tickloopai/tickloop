use std::{ffi::OsString, process::ExitCode};

use crate::{
    auth,
    cli::{Cli, Command},
    error::{Error, Result},
    exit_code, locale,
    stream::{LONGBRIDGE_SOURCE, TickStream},
};
use clap::Parser;

pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut arguments: Vec<_> = arguments.into_iter().collect();
    let preflight = arguments.first().is_some_and(|arg| arg == "__preflight");
    if preflight {
        arguments.remove(0);
    }
    let cli =
        match Cli::try_parse_from(std::iter::once(OsString::from("tick")).chain(arguments.clone()))
        {
            Ok(cli) => cli,
            Err(error) => {
                let exit_code = error.exit_code();
                let _ = error.print();
                return ExitCode::from(exit_code as u8);
            }
        };
    if preflight {
        return match crate::native_launcher::preflight(&cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("tick: {error}");
                exit_code::exit(error.code)
            }
        };
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
    let result = runtime.block_on(async {
        if let Some(result) = crate::native_launcher::maybe_launch(&cli, &arguments).await {
            result
        } else {
            execute(cli).await
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    match result {
        Ok(()) => exit_code::exit(exit_code::SUCCESS),
        Err(error) => {
            eprintln!("tick: {error}");
            exit_code::exit(error.code)
        }
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let native = std::env::var_os(tick_connect::protocol::INSTANCE_ID_ENV).is_some();
    if native
        && (cli.command.is_once()
            || matches!(
                cli.command,
                Command::Describe | Command::Auth { .. } | Command::ProbeFundamentals(_)
            ))
    {
        return Err(Error::usage(
            "Native Tick stdio requires a live Tick; --once, describe, auth and probe are separate entry points",
        ));
    }
    if matches!(cli.command, Command::Describe) {
        let descriptors: Vec<_> = crate::metadata::TickKind::ALL
            .into_iter()
            .map(|tick| {
                let descriptor = tick.descriptor();
                descriptor.validate().map(|()| descriptor)
            })
            .collect::<Result<_>>()?;
        serde_json::to_writer_pretty(std::io::stdout(), &descriptors)
            .map_err(|error| Error::usage(error.to_string()))?;
        println!();
        return Ok(());
    }
    locale::check_utf8().map_err(|error| Error::usage(error.to_string()))?;
    let mut settings = cli
        .global
        .settings()
        .map_err(|error| Error::usage(format!("configuration error: {error}")))?;
    if let Command::Demo(args) = cli.command {
        return crate::demo::run(args, &settings).await;
    }
    if matches!(cli.command, Command::Options(_) | Command::Reader(_)) {
        return run_independent(cli.command, &settings).await;
    }
    let storage = auth::default_storage()?;
    let config_path = cli
        .global
        .config
        .clone()
        .or_else(crate::config::default_path)
        .ok_or_else(|| Error::usage("cannot determine configuration path"))?;
    match cli.command {
        Command::Auth {
            action: Some(action),
        } => auth::inspect(action, &storage, settings.region, &mut std::io::stdout()),
        Command::Auth { action: None } => {
            match interactive_login(&settings, &config_path, &storage).await? {
                auth::LoginOutcome::Authorized(_) => {
                    println!("Longbridge authorization saved.");
                    Ok(())
                }
                auth::LoginOutcome::Cancelled => Ok(()),
            }
        }
        Command::ProbeFundamentals(args) => {
            run_fundamentals_probe(&args, &settings, &storage).await
        }
        command => {
            let kind = command
                .kind()
                .ok_or_else(|| Error::usage("expected a market Tick Command"))?;
            let mut updates = TickStream::from_process(kind, LONGBRIDGE_SOURCE)?;
            let result =
                run_longbridge_tick(command, &mut settings, &config_path, &storage, &mut updates)
                    .await;
            updates.close().await;
            result
        }
    }
}

async fn run_independent(command: Command, settings: &crate::config::Settings) -> Result<()> {
    use std::{io::IsTerminal, sync::Arc};
    let (kind, source) = match &command {
        Command::Options(_) => (
            crate::metadata::TickKind::Options,
            crate::stream::FUTU_SOURCE,
        ),
        Command::Reader(_) => (
            crate::metadata::TickKind::Reader,
            crate::stream::DOCUMENT_SOURCE,
        ),
        _ => return Err(Error::usage("expected an independent data Tick")),
    };
    let options = if let Command::Options(args) = &command {
        let request = crate::options::OptionsRequest::new(
            &args.symbol,
            args.expiry.as_deref(),
            args.strikes,
        )?;
        let api: Arc<dyn crate::options::OptionsApi> =
            Arc::new(crate::futu::FutuOptions::lazy(&args.host, args.port)?);
        if args.check {
            crate::options::load(api.as_ref(), &request).await?;
            return Ok(());
        }
        Some(api)
    } else {
        None
    };
    let documents = if let Command::Reader(args) = &command {
        let context = crate::research::ResearchContext::from_environment()?;
        let library = if args.check || args.output.once {
            crate::reader::load(args).await?
        } else {
            crate::reader::load_for_context(args, context.as_ref()).await?
        };
        if args.check {
            return Ok(());
        }
        Some(library)
    } else {
        None
    };
    let mut updates = TickStream::from_process(kind, source)?;
    let result = if command.is_once() {
        let mut output = std::io::stdout();
        let color = output.is_terminal();
        match &command {
            Command::Options(args) => {
                crate::commands::options::run_once(
                    args,
                    options.expect("options loaded"),
                    settings,
                    color,
                    &mut output,
                    &mut updates,
                )
                .await
            }
            Command::Reader(args) => {
                crate::commands::reader::run_once(
                    args,
                    documents.expect("documents loaded"),
                    settings,
                    color,
                    &mut output,
                    &mut updates,
                )
                .await
            }
            _ => unreachable!(),
        }
    } else if updates.is_native() {
        let mut session = crate::terminal::VisibleTerminalSession::enter(None)?;
        let input = crate::terminal::visible_input_events()?;
        match &command {
            Command::Options(args) => {
                crate::commands::options::run_live(
                    args,
                    options.expect("options loaded"),
                    settings,
                    &mut session.terminal,
                    input,
                    &mut updates,
                )
                .await
            }
            Command::Reader(_) => {
                crate::commands::reader::run_live(
                    documents.expect("documents loaded"),
                    settings,
                    &mut session.terminal,
                    input,
                    &mut updates,
                )
                .await
            }
            _ => unreachable!(),
        }
    } else {
        let mut session = crate::terminal::TerminalSession::enter()?;
        let input = crate::terminal::input_events()?;
        match &command {
            Command::Options(args) => {
                crate::commands::options::run_live(
                    args,
                    options.expect("options loaded"),
                    settings,
                    &mut session.terminal,
                    input,
                    &mut updates,
                )
                .await
            }
            Command::Reader(_) => {
                crate::commands::reader::run_live(
                    documents.expect("documents loaded"),
                    settings,
                    &mut session.terminal,
                    input,
                    &mut updates,
                )
                .await
            }
            _ => unreachable!(),
        }
    };
    updates.close().await;
    result
}

async fn run_longbridge_tick(
    command: Command,
    settings: &mut crate::config::Settings,
    config_path: &std::path::Path,
    storage: &crate::auth::storage::AuthStorage,
    updates: &mut TickStream,
) -> Result<()> {
    let interactive = std::env::var_os(auth::NONINTERACTIVE_AUTH_ENV).is_none()
        && !command.is_once()
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let mut login_required = false;
    loop {
        if login_required {
            match interactive_login(settings, config_path, storage).await? {
                auth::LoginOutcome::Authorized(region) => settings.region = Some(region),
                auth::LoginOutcome::Cancelled => return Ok(()),
            }
        }
        let region = settings.region.unwrap_or(crate::config::Region::Global);
        let oauth = match auth::authorized_oauth(storage, region, None).await {
            Ok(oauth) => oauth,
            Err(error) if error.code == exit_code::NOT_AUTHORIZED && interactive => {
                login_required = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        match run_market(command.clone(), oauth, settings, updates).await {
            Err(error) if error.code == exit_code::NOT_AUTHORIZED && interactive => {
                login_required = true;
            }
            result => return result,
        }
    }
}

async fn run_fundamentals_probe(
    args: &crate::cli::ProbeArgs,
    settings: &crate::config::Settings,
    storage: &crate::auth::storage::AuthStorage,
) -> Result<()> {
    use std::sync::Arc;

    let region = settings.region.unwrap_or(crate::config::Region::Global);
    let oauth = auth::authorized_oauth(storage, region, None).await?;
    let token = oauth
        .access_token()
        .await
        .map_err(|_| Error::auth("authorization expired"))?;
    let data_center = longbridge::DcRegion::from_credential(&token)
        .as_str()
        .to_owned();
    let config = crate::sdk::config(oauth, settings).await?;
    crate::probe::run(
        args,
        config,
        Arc::new(crate::rate_limit::RateLimiter::default()),
        &data_center,
    )
    .await
}

async fn interactive_login(
    settings: &crate::config::Settings,
    config_path: &std::path::Path,
    storage: &crate::auth::storage::AuthStorage,
) -> Result<auth::LoginOutcome> {
    let mut session = crate::terminal::TerminalSession::enter()?;
    let input = crate::terminal::input_events()?;
    auth::run_tui(
        settings,
        config_path,
        storage,
        None,
        &mut session.terminal,
        input,
    )
    .await
}

async fn run_market(
    command: Command,
    oauth: longbridge::oauth::OAuth,
    settings: &crate::config::Settings,
    updates: &mut TickStream,
) -> Result<()> {
    use crate::{
        rate_limit::RateLimiter,
        sdk::{SdkQuoteApi, SdkTradeApi},
        terminal::TerminalSession,
    };
    use std::{io::IsTerminal, sync::Arc};
    let once = command.is_once();
    let token = oauth
        .access_token()
        .await
        .map_err(|_| Error::auth("authorization expired"))?;
    let is_us = longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us;
    let config = crate::sdk::config(oauth.clone(), settings).await?;
    let limiter = Arc::new(RateLimiter::default());
    let api = SdkQuoteApi::new(config.clone(), oauth.clone(), settings, limiter.clone()).await?;
    let (trade_context, _trade_events) = longbridge::trade::TradeContext::new(config.clone());
    let trade = SdkTradeApi {
        context: trade_context,
        limiter: limiter.clone(),
        is_us,
    };
    let news = if matches!(command, Command::News(_)) {
        Some(crate::news_api::SdkNewsApi::new(config, oauth, settings, limiter).await?)
    } else {
        None
    };
    let news = news
        .as_ref()
        .map(|news| news as &dyn crate::news_api::NewsApi);
    if once {
        let mut output = std::io::stdout();
        let color = output.is_terminal();
        return run_once(
            command,
            &api,
            &trade,
            news,
            settings,
            color,
            &mut output,
            updates,
        )
        .await;
    }
    if updates.is_native() {
        let mut session = crate::terminal::VisibleTerminalSession::enter(None)?;
        let input = crate::terminal::visible_input_events()?;
        return run_live(
            command,
            &api,
            &trade,
            news,
            settings,
            &mut session.terminal,
            input,
            updates,
        )
        .await;
    }
    let mut session = TerminalSession::enter()?;
    let input = crate::terminal::input_events()?;
    run_live(
        command,
        &api,
        &trade,
        news,
        settings,
        &mut session.terminal,
        input,
        updates,
    )
    .await
}

/// The live dispatch shared by market and demo commands.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    command: Command,
    api: &crate::api::QuoteServices,
    trade: &dyn crate::api::TradeApi,
    news: Option<&dyn crate::news_api::NewsApi>,
    settings: &crate::config::Settings,
    terminal: &mut ratatui::Terminal<B>,
    input: S,
    updates: &mut TickStream,
) -> Result<()> {
    use crate::commands;
    match command {
        Command::Quote(args) => {
            commands::quote::run_live(&args, api, settings, terminal, input, updates).await
        }
        Command::Watchlist(args) => {
            commands::watchlist::run_live(&args, api, settings, terminal, input, updates).await
        }
        Command::Kline(args) => {
            commands::kline::run_live(&args, api, settings, terminal, input, updates).await
        }
        Command::Positions(args) => {
            commands::positions::run_live(&args, api, trade, settings, terminal, input, updates)
                .await
        }
        Command::News(args) => {
            commands::news::run_live(
                &args,
                api,
                news.expect("news context initialized"),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        Command::Earnings(args) => {
            commands::earnings::run_live(&args, api, settings, terminal, input, updates).await
        }
        Command::Options(args) if settings.is_demo => {
            commands::options::run_live(
                &args,
                commands::options::demo_api(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        Command::Reader(_) if settings.is_demo => {
            commands::reader::run_live(
                crate::reader::demo_library(),
                settings,
                terminal,
                input,
                updates,
            )
            .await
        }
        _ => Err(Error::usage("expected a market Tick Command")),
    }
}

/// The same five-way dispatch for `--once` output.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_once(
    command: Command,
    api: &crate::api::QuoteServices,
    trade: &dyn crate::api::TradeApi,
    news: Option<&dyn crate::news_api::NewsApi>,
    settings: &crate::config::Settings,
    color: bool,
    output: &mut impl std::io::Write,
    updates: &mut TickStream,
) -> Result<()> {
    use crate::commands;
    match command {
        Command::Quote(args) => {
            commands::quote::run_once(&args, api, settings, color, output, updates).await
        }
        Command::Watchlist(args) => {
            commands::watchlist::run_once(&args, api, settings, color, output, updates).await
        }
        Command::Kline(args) => {
            commands::kline::run_once(&args, api, settings, color, output, updates).await
        }
        Command::Positions(args) => {
            commands::positions::run_once(&args, api, trade, settings, color, output, updates).await
        }
        Command::News(args) => {
            commands::news::run_once(
                &args,
                news.expect("news context initialized"),
                settings,
                color,
                output,
                updates,
            )
            .await
        }
        Command::Earnings(args) => {
            commands::earnings::run_once(&args, api, settings, color, output, updates).await
        }
        Command::Options(args) if settings.is_demo => {
            commands::options::run_once(
                &args,
                commands::options::demo_api(),
                settings,
                color,
                output,
                updates,
            )
            .await
        }
        Command::Reader(args) if settings.is_demo => {
            commands::reader::run_once(
                &args,
                crate::reader::demo_library(),
                settings,
                color,
                output,
                updates,
            )
            .await
        }
        _ => Err(Error::usage("expected a market Tick Command")),
    }
}
