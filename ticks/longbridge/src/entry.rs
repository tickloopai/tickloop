use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::Arc,
};

use tick_command::{
    cli::{GlobalArgs, TickCommand},
    entry::{self, Running, Services},
};
use tick_market::{
    config::{Overrides, Settings},
    terminal::TerminalSession,
};
use tick_provider::{
    community::CommunityApi,
    error::{Error, Result},
    exit_code,
    news::NewsApi,
    preferences::Region,
};

use crate::{
    auth::{self, storage::AuthStorage},
    cli::{Cli, Command, ProbeArgs, TICKS},
    community_api::SdkCommunityApi,
    login,
    news_api::SdkNewsApi,
    not_authorized,
    rate_limit::RateLimiter,
    sdk::{SdkQuoteApi, SdkTradeApi},
};

pub async fn execute(cli: Cli) -> Result<()> {
    let Cli {
        global,
        region,
        command,
    } = cli;
    match command {
        Command::Tick(tick) => {
            let tick = TickCommand::from(tick);
            if tick.is_once() {
                entry::refuse_in_instance()?;
            }
            let (mut settings, storage) = prepare(&global, region)?;
            run_longbridge_tick(
                tick,
                &mut settings,
                &config_path(&global)?,
                &storage,
                global.size,
            )
            .await
        }
        Command::Auth { action } => {
            entry::refuse_in_instance()?;
            let (settings, storage) = prepare(&global, region)?;
            match action {
                Some(action) => {
                    login::inspect(action, &storage, settings.region, &mut std::io::stdout())
                }
                None => match interactive_login(&settings, &config_path(&global)?, &storage).await?
                {
                    login::LoginOutcome::Authorized(_) => {
                        println!("Longbridge authorization saved.");
                        Ok(())
                    }
                    login::LoginOutcome::Cancelled => Ok(()),
                },
            }
        }
        Command::ProbeFundamentals(args) => {
            entry::refuse_in_instance()?;
            let (settings, storage) = prepare(&global, region)?;
            let region = settings.region.unwrap_or(Region::Global);
            let oauth = auth::authorized_oauth(&storage, region, None).await?;
            run_fundamentals_probe(&args, oauth, &settings).await
        }
        Command::Describe => {
            entry::refuse_in_instance()?;
            entry::describe(&TICKS)
        }
    }
}

/// Checks the locale, then loads the configuration with the region this run asks for, and the
/// stored Longbridge login.
fn prepare(global: &GlobalArgs, region: Option<Region>) -> Result<(Settings, AuthStorage)> {
    entry::check_locale()?;
    let settings = global.settings()?.with_overrides(Overrides {
        region,
        ..Overrides::default()
    });
    Ok((settings, auth::default_storage()?))
}

/// The configuration file a login saves the chosen region to.
fn config_path(global: &GlobalArgs) -> Result<PathBuf> {
    global
        .config
        .clone()
        .or_else(tick_market::config::default_path)
        .ok_or_else(|| Error::usage("cannot determine configuration path"))
}

/// Runs a Longbridge Tick, opening the login screen first, and again whenever authorization
/// lapses, when the Tick runs interactively.
async fn run_longbridge_tick(
    tick: TickCommand,
    settings: &mut Settings,
    config_path: &Path,
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
        .map_err(|_| not_authorized("authorization expired"))?;
    let is_us = longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us;
    let config = crate::sdk::config(oauth.clone(), settings.language, settings.region).await?;
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
        Some(
            SdkNewsApi::new(
                config.clone(),
                oauth,
                settings.language,
                settings.region,
                limiter.clone(),
            )
            .await?,
        )
    } else {
        None
    };
    let community = if matches!(tick, TickCommand::Community(_)) {
        Some(Arc::new(SdkCommunityApi::new(config, limiter)?) as Arc<dyn CommunityApi>)
    } else {
        None
    };
    let services = Services {
        quote: Some(&api),
        trade: Some(&trade),
        news: news.as_ref().map(|news| news as &dyn NewsApi),
        options: None,
        community,
    };
    entry::run_tick(Running::Provider(tick, services), settings, size).await
}

async fn run_fundamentals_probe(
    args: &ProbeArgs,
    oauth: longbridge::oauth::OAuth,
    settings: &Settings,
) -> Result<()> {
    let token = oauth
        .access_token()
        .await
        .map_err(|_| not_authorized("authorization expired"))?;
    let data_center = longbridge::DcRegion::from_credential(&token)
        .as_str()
        .to_owned();
    let config = crate::sdk::config(oauth, settings.language, settings.region).await?;
    crate::probe::run(
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
    config_path: &Path,
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

#[cfg(test)]
mod tests {
    use tick_market::args::{OnceArgs, QuoteArgs};

    use super::*;
    use crate::asks_to_log_in;

    #[tokio::test]
    async fn a_login_that_lapses_before_a_tick_or_probe_starts_asks_the_user_to_log_in_again() {
        let oauth = crate::auth::lapsing_oauth().await;
        let quote = TickCommand::Quote(QuoteArgs {
            symbols: vec!["TSLA.US".into()],
            output: OnceArgs::default(),
        });
        let error = run_longbridge_market(quote, oauth.clone(), &Settings::default(), None)
            .await
            .unwrap_err();
        assert!(asks_to_log_in(&error), "{error}");
        let probe = ProbeArgs {
            symbol: crate::probe::DEFAULT_SYMBOL.into(),
            as_of: None,
            out: None,
        };
        let error = run_fundamentals_probe(&probe, oauth, &Settings::default())
            .await
            .unwrap_err();
        assert!(asks_to_log_in(&error), "{error}");
    }
}
