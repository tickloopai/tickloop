use std::{
    hash::{DefaultHasher, Hash, Hasher},
    io::IsTerminal,
    sync::Arc,
};

use tick_command::{
    cli::{GlobalArgs, TickCommand},
    connecting::{self, DataSource},
    entry::{self, Running, Services},
};
use tick_market::{config::Settings, terminal::TerminalSession};
use tick_packages::PackageCredentials;
use tick_provider::{
    community::CommunityApi, error::Result, exit_code, news::NewsApi, preferences::Language,
};

use crate::{
    auth::{self, Authorized},
    cli::{Cli, Command, ProbeArgs, TICKS},
    community_api::SdkCommunityApi,
    login,
    news_api::SdkNewsApi,
    not_authorized,
    rate_limit::RateLimiter,
    region::Issuers,
    sdk::{SdkQuoteApi, SdkTradeApi},
};

pub async fn execute(cli: Cli) -> Result<()> {
    let Cli { global, command } = cli;
    match command {
        Command::Tick(tick) => {
            let tick = TickCommand::from(tick);
            if tick.is_once() {
                entry::refuse_in_instance()?;
            }
            let (settings, credentials) = prepare(&global)?;
            if entry::in_instance() {
                // Only what needs no network runs before the Tick answers its host.
                auth::check_saved(&credentials)?;
                let source = Longbridge {
                    tick: tick.clone(),
                    settings: settings.clone(),
                    credentials,
                };
                return connecting::run_hosted(tick, &settings, global.size, source).await;
            }
            run_longbridge_tick(tick, &settings, &credentials, global.size).await
        }
        Command::Auth { action } => {
            entry::refuse_in_instance()?;
            let (settings, credentials) = prepare(&global)?;
            match action {
                Some(action) => login::inspect(action, &credentials, &mut std::io::stdout()),
                None => match interactive_login(&settings, &credentials).await? {
                    login::LoginOutcome::Authorized => {
                        println!("Longbridge authorization saved.");
                        Ok(())
                    }
                    login::LoginOutcome::Cancelled => Ok(()),
                },
            }
        }
        Command::ProbeFundamentals(args) => {
            entry::refuse_in_instance()?;
            let (settings, credentials) = prepare(&global)?;
            let authorized = auth::authorized_oauth(&credentials).await?;
            run_fundamentals_probe(&args, authorized, &settings).await
        }
        Command::Describe => {
            entry::refuse_in_instance()?;
            entry::describe(&TICKS)
        }
    }
}

/// Checks the locale, then loads the configuration and finds the saved Longbridge login.
fn prepare(global: &GlobalArgs) -> Result<(Settings, PackageCredentials)> {
    entry::check_locale()?;
    Ok((global.settings()?, auth::credentials()?))
}

/// Runs a Longbridge Tick, opening the login screen first, and again whenever authorization
/// lapses, when the Tick runs interactively.
async fn run_longbridge_tick(
    tick: TickCommand,
    settings: &Settings,
    credentials: &PackageCredentials,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let interactive =
        !tick.is_once() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut login_required = false;
    loop {
        if login_required
            && interactive_login(settings, credentials).await? == login::LoginOutcome::Cancelled
        {
            return Ok(());
        }
        let authorized = match auth::authorized_oauth(credentials).await {
            Ok(authorized) => authorized,
            Err(error) if error.code == exit_code::NOT_AUTHORIZED && interactive => {
                login_required = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        match run_longbridge_market(tick.clone(), authorized, settings, size).await {
            Err(error) if error.code == exit_code::NOT_AUTHORIZED && interactive => {
                login_required = true;
            }
            result => return result,
        }
    }
}

async fn run_longbridge_market(
    tick: TickCommand,
    authorized: Authorized,
    settings: &Settings,
    size: Option<ratatui::layout::Size>,
) -> Result<()> {
    let connection = connect(&tick, authorized, settings).await?;
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
    Authorized { oauth, region }: Authorized,
    settings: &Settings,
) -> Result<Connection> {
    let token = oauth
        .access_token()
        .await
        .map_err(|_| not_authorized("authorization expired"))?;
    let is_us = longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us;
    let config = crate::sdk::config(oauth.clone(), settings.language, region);
    let limiter = Arc::new(RateLimiter::default());
    let quote = SdkQuoteApi::new(
        config.clone(),
        oauth.clone(),
        settings.language,
        region,
        limiter.clone(),
    );
    let (trade_context, trade_events) = longbridge::trade::TradeContext::new(config.clone());
    let trade = SdkTradeApi {
        context: trade_context,
        limiter: limiter.clone(),
        is_us,
    };
    let news = if matches!(tick, TickCommand::News(_)) {
        Some(SdkNewsApi::new(
            config.clone(),
            oauth,
            settings.language,
            region,
            limiter.clone(),
        )?)
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

/// Longbridge as the data source of a Tick its host launched, connected with the saved login.
struct Longbridge {
    tick: TickCommand,
    settings: Settings,
    credentials: PackageCredentials,
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
        let saved = std::fs::read(self.credentials.oauth_token_path()).ok()?;
        let mut fingerprint = DefaultHasher::new();
        saved.hash(&mut fingerprint);
        Some(fingerprint.finish())
    }

    async fn connect(&mut self) -> Result<Connection> {
        let authorized = auth::authorized_oauth(&self.credentials).await?;
        connect(&self.tick, authorized, &self.settings).await
    }

    fn services(connection: &Connection) -> Services<'_> {
        connection.services()
    }
}

async fn run_fundamentals_probe(
    args: &ProbeArgs,
    Authorized { oauth, region }: Authorized,
    settings: &Settings,
) -> Result<()> {
    let token = oauth
        .access_token()
        .await
        .map_err(|_| not_authorized("authorization expired"))?;
    let data_center = longbridge::DcRegion::from_credential(&token)
        .as_str()
        .to_owned();
    let config = crate::sdk::config(oauth, settings.language, region);
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
    credentials: &PackageCredentials,
) -> Result<login::LoginOutcome> {
    let mut session = TerminalSession::enter()?;
    let input = tick_market::terminal::input_events()?;
    login::run(
        settings,
        credentials,
        &Issuers::default(),
        &mut session.terminal,
        input,
    )
    .await
}

#[cfg(test)]
mod tests {
    use tick_market::args::{OnceArgs, QuoteArgs};
    use tick_packages::OAuthToken;
    use tick_provider::error::Error;

    use super::*;
    use crate::{asks_to_log_in, region::Region};

    #[tokio::test]
    async fn a_login_that_lapses_before_a_tick_or_probe_starts_asks_the_user_to_log_in_again() {
        let lapsing = || async {
            Authorized {
                oauth: crate::auth::lapsing_oauth().await,
                region: Region::Global,
            }
        };
        let quote = TickCommand::Quote(QuoteArgs {
            symbols: vec!["TSLA.US".into()],
            output: OnceArgs::default(),
        });
        let error = run_longbridge_market(quote, lapsing().await, &Settings::default(), None)
            .await
            .unwrap_err();
        assert!(asks_to_log_in(&error), "{error}");
        let probe = ProbeArgs {
            symbol: crate::probe::DEFAULT_SYMBOL.into(),
            as_of: None,
            out: None,
        };
        let error = run_fundamentals_probe(&probe, lapsing().await, &Settings::default())
            .await
            .unwrap_err();
        assert!(asks_to_log_in(&error), "{error}");
    }

    /// A token endpoint of a stand-in for Longbridge that refuses every login it is asked to
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

    /// A lapsed token whose server is `issuer`.
    fn lapsed(issuer: &str, access_token: &str) -> OAuthToken {
        OAuthToken {
            access_token: access_token.into(),
            token_type: "Bearer".into(),
            refresh_token: Some("ap_refresh".into()),
            scope: None,
            issuer: issuer.into(),
            client_id: "test".into(),
            token_endpoint: format!("{issuer}/token"),
            revocation_endpoint: None,
            expires_at: 1,
        }
    }

    /// A hosted Tick tells a login Longbridge refuses to renew from a Longbridge it cannot
    /// reach, and notices on this computer alone when the user logged in again.
    #[tokio::test]
    async fn renewing_the_saved_login_tells_a_refused_login_from_an_unreachable_longbridge() {
        let root = tempfile::tempdir().unwrap();
        let credentials = PackageCredentials::in_config_root(root.path(), "longbridge").unwrap();
        let mut source = Longbridge {
            tick: TickCommand::Quote(QuoteArgs {
                symbols: vec!["TSLA.US".into()],
                output: OnceArgs::default(),
            }),
            settings: Settings::default(),
            credentials: credentials.clone(),
        };
        assert_eq!(source.stored_login(), None);
        let save = |token: OAuthToken| {
            credentials
                .lock_oauth()
                .unwrap()
                .save_token(&token)
                .unwrap()
        };

        let refusing = refusing_login_renewals().await;
        save(lapsed(&refusing, "ap_lapsed"));
        let lapsed_login = source.stored_login();
        assert!(lapsed_login.is_some());
        let refused = connect_result(source.connect().await);
        assert!(asks_to_log_in(&refused), "{refused}");

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unreachable = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        save(lapsed(&unreachable, "ap_lapsed"));
        let failed = connect_result(source.connect().await);
        assert_eq!(failed.code, exit_code::NETWORK, "{failed}");

        save(lapsed(&unreachable, "ap_new"));
        assert!(
            source
                .stored_login()
                .is_some_and(|login| Some(login) != lapsed_login)
        );
    }
}
