use std::{
    hash::{DefaultHasher, Hash, Hasher},
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::Arc,
};

use tick_command::{
    cli::{GlobalArgs, TickCommand},
    connecting::{self, DataSource},
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
    preferences::{Language, Region},
};

use crate::{
    auth::{
        self, AuthEndpoints,
        storage::{AuthStorage, TOKEN_FILE_NAME},
    },
    cli::{AuthAction, Cli, Command, ProbeArgs, TICKS},
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
            if entry::in_instance() {
                // Only what needs no network runs before the Tick answers its host.
                auth::check_stored(&storage)?;
                let source = Longbridge {
                    tick: tick.clone(),
                    settings: settings.clone(),
                    storage,
                    endpoints: None,
                };
                return connecting::run_hosted(tick, &settings, global.size, source).await;
            }
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
            if let Some(AuthAction::Status { json: true }) = action {
                // Read by Tickloop, so neither the locale nor the configuration may fail it.
                return login::print_status_json(&auth::default_storage()?, &mut std::io::stdout());
            }
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
    let connection = connect(&tick, oauth, settings).await?;
    entry::run_tick(
        Running::Provider(tick, connection.services()),
        settings,
        size,
    )
    .await
}

/// The Longbridge services a Tick reads, set up with one login.
struct Connection {
    quote: SdkQuoteApi,
    trade: SdkTradeApi,
    /// Kept for as long as the trade context: the SDK sends the account's pushes here.
    _trade_events: tokio::sync::mpsc::UnboundedReceiver<longbridge::trade::PushEvent>,
    news: Option<SdkNewsApi>,
    community: Option<Arc<dyn CommunityApi>>,
}

impl Connection {
    fn services(&self) -> Services<'_> {
        Services {
            quote: Some(&self.quote),
            trade: Some(&self.trade),
            news: self.news.as_ref().map(|news| news as &dyn NewsApi),
            options: None,
            community: self.community.clone(),
        }
    }
}

async fn connect(
    tick: &TickCommand,
    oauth: longbridge::oauth::OAuth,
    settings: &Settings,
) -> Result<Connection> {
    let token = oauth
        .access_token()
        .await
        .map_err(|_| not_authorized("authorization expired"))?;
    let is_us = longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us;
    let config = crate::sdk::config(oauth.clone(), settings.language, settings.region).await?;
    let limiter = Arc::new(RateLimiter::default());
    let quote = SdkQuoteApi::new(
        config.clone(),
        oauth.clone(),
        settings.language,
        settings.region,
        limiter.clone(),
    )
    .await?;
    let (trade_context, trade_events) = longbridge::trade::TradeContext::new(config.clone());
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
    Ok(Connection {
        quote,
        trade,
        _trade_events: trade_events,
        news,
        community,
    })
}

/// Longbridge as the data source of a Tick its host launched, connected with the login stored on
/// this computer.
struct Longbridge {
    tick: TickCommand,
    settings: Settings,
    storage: AuthStorage,
    /// Where the login is renewed; `None` for the region's own endpoints.
    endpoints: Option<AuthEndpoints>,
}

impl DataSource for Longbridge {
    type Connection = Connection;

    fn name(&self) -> &str {
        match self.settings.language {
            Language::ZhCn => "长桥",
            Language::En => "Longbridge",
        }
    }

    fn login_hint(&self) -> &str {
        match self.settings.language {
            Language::ZhCn => "在 Agent 中用 /ticks 或用 `tickloop tick login longbridge` 登录",
            Language::En => "log in with /ticks in the Agent or `tickloop tick login longbridge`",
        }
    }

    fn stored_login(&self) -> Option<u64> {
        let saved = std::fs::read(self.storage.directory.join(TOKEN_FILE_NAME)).ok()?;
        let mut fingerprint = DefaultHasher::new();
        saved.hash(&mut fingerprint);
        Some(fingerprint.finish())
    }

    async fn connect(&mut self) -> Result<Connection> {
        let region = self.settings.region.unwrap_or(Region::Global);
        let oauth = auth::authorized_oauth(&self.storage, region, self.endpoints.clone()).await?;
        connect(&self.tick, oauth, &self.settings).await
    }

    fn services(connection: &Connection) -> Services<'_> {
        connection.services()
    }
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

    /// The token endpoint of a stand-in for Longbridge that refuses every login it is asked to
    /// renew.
    async fn refusing_login_renewals() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut connection, _)) = listener.accept().await {
                let mut request = [0; 4096];
                let _ = connection.read(&mut request).await;
                let _ = connection
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            }
        });
        format!("http://{address}")
    }

    fn connect_result(result: Result<Connection>) -> Error {
        match result {
            Ok(_) => panic!("connected without Longbridge"),
            Err(error) => error,
        }
    }

    /// A hosted Tick tells a login Longbridge refuses to renew from a Longbridge it cannot
    /// reach, and notices on this computer alone when the user logged in again.
    #[tokio::test]
    async fn renewing_the_stored_login_tells_a_refused_login_from_an_unreachable_longbridge() {
        let home = tempfile::tempdir().unwrap();
        let storage = AuthStorage::in_home(home.path());
        let mut source = Longbridge {
            tick: TickCommand::Quote(QuoteArgs {
                symbols: vec!["TSLA.US".into()],
                output: OnceArgs::default(),
            }),
            settings: Settings::default(),
            storage: storage.clone(),
            endpoints: None,
        };
        assert_eq!(source.stored_login(), None);
        let token = storage.directory.join(TOKEN_FILE_NAME);
        std::fs::create_dir_all(&storage.directory).unwrap();
        std::fs::write(
            &token,
            r#"{"client_id":"test","access_token":"ap_lapsed","refresh_token":"ap_refresh","expires_at":1,"logged_in_at":1}"#,
        )
        .unwrap();
        let lapsed = source.stored_login();
        assert!(lapsed.is_some());

        let refusing = refusing_login_renewals().await;
        source.endpoints = Some(AuthEndpoints {
            ap: refusing.clone(),
            us: refusing,
        });
        let refused = connect_result(source.connect().await);
        assert!(asks_to_log_in(&refused), "{refused}");

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unreachable = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        source.endpoints = Some(AuthEndpoints {
            ap: unreachable.clone(),
            us: unreachable,
        });
        let failed = connect_result(source.connect().await);
        assert_eq!(failed.code, exit_code::NETWORK, "{failed}");

        std::fs::write(
            &token,
            r#"{"client_id":"test","access_token":"ap_new","refresh_token":"ap_refresh","expires_at":1,"logged_in_at":2}"#,
        )
        .unwrap();
        assert!(
            source
                .stored_login()
                .is_some_and(|login| Some(login) != lapsed)
        );
    }
}
