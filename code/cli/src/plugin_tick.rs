//! Official Tick marketplace installation and plugin login for `tickloop plugin`.

use std::io::{self, BufRead, IsTerminal, Write};

use super::plugin_command::{print_table, report};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use tick_packages::{
    ApiKey, CURRENT_TARGET, LoginState, OAuthIssuer, PackageAuth, PackageCredentials,
    validate_package_name,
};
use tick_plugins::{InstalledPlugin, PluginError, PluginStore};
use tickloop_code_console::terminal::safe_line;
use tickloop_tick_installer::{
    InstalledVersion, MarketplaceSource, log_out, login_label, needs_no_login, start_login,
};
use tokio_util::sync::CancellationToken;

/// Installs the version of `name` the marketplace offers, unless it is installed already.
pub(super) async fn install(store: &PluginStore, name: &str) -> Result<(), String> {
    validate_package_name(name).map_err(|error| error.to_string())?;
    let source = MarketplaceSource::configured()?;
    let marketplace = source
        .read(&tickloop_code_settings::package_http::client)
        .await?;
    let package = marketplace.package(name).ok_or_else(|| {
        format!(
            "Tick package {name} is not in the Tick marketplace at {}",
            safe_line(&source.to_string())
        )
    })?;
    report(
        &tickloop_tick_installer::install(
            &tickloop_code_settings::package_http::client,
            store,
            &source,
            package,
        )
        .await?,
    );
    Ok(())
}

/// Logs the package in to its data source as its manifest declares.
pub(super) async fn login(
    store: &PluginStore,
    name: &str,
    issuer: Option<&str>,
) -> Result<(), String> {
    let package = installed_package(store, name)?;
    let credentials = store.credentials(name).map_err(|error| error.to_string())?;
    match (&package.manifest.auth, issuer) {
        (PackageAuth::None, _) => Err(needs_no_login(name)),
        (PackageAuth::ApiKey { .. }, Some(_)) => Err(format!(
            "Tick package {name} logs in with a key; --issuer is for OAuth logins"
        )),
        (PackageAuth::ApiKey { label, .. }, None) => save_key(&credentials, name, label),
        (PackageAuth::OAuth2 { issuers, scopes }, issuer) => {
            let issuer = choose_issuer(name, issuers, issuer)?;
            log_in_with(&credentials, name, issuer, scopes).await
        }
    }
}

/// The issuer `label` names, ignoring case; without one, the only issuer, or the one the user
/// types on this terminal.
fn choose_issuer<'a>(
    name: &str,
    issuers: &'a [OAuthIssuer],
    label: Option<&str>,
) -> Result<&'a OAuthIssuer, String> {
    let labels = issuers
        .iter()
        .map(|issuer| issuer.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let named = |label: &str| {
        issuers
            .iter()
            .find(|issuer| issuer.label.eq_ignore_ascii_case(label.trim()))
    };
    match (label, issuers) {
        (Some(label), _) => named(label).ok_or_else(|| {
            format!(
                "Tick package {name} has no issuer called {}; choose one of: {labels}",
                safe_line(label)
            )
        }),
        (None, [only]) => Ok(only),
        (None, _) if !io::stdin().is_terminal() => Err(format!(
            "Tick package {name} logs in with one of: {labels}; choose one with \
             tickloop plugin login {name} --issuer LABEL"
        )),
        (None, _) => {
            println!("Tick package {name} logs in with one of: {labels}.");
            let mut input = io::stdin().lock();
            loop {
                print!("Type the one to use: ");
                io::stdout().flush().map_err(|error| error.to_string())?;
                let mut typed = String::new();
                if input
                    .read_line(&mut typed)
                    .map_err(|error| error.to_string())?
                    == 0
                {
                    return Err(format!("no issuer was chosen to log in to {name}"));
                }
                match named(&typed) {
                    Some(issuer) => return Ok(issuer),
                    None => println!("That is none of: {labels}."),
                }
            }
        }
    }
}

/// Shows the link and code of a device authorization and waits for the user to approve it in a
/// browser on any device; Ctrl+C cancels.
async fn log_in_with(
    credentials: &PackageCredentials,
    name: &str,
    issuer: &OAuthIssuer,
    scopes: &[String],
) -> Result<(), String> {
    let failed = |error: tickloop_tick_installer::LoginError| {
        format!("cannot log in to {name} ({}): {error}", issuer.label)
    };
    let login = start_login(
        &tickloop_code_settings::package_http::client,
        credentials,
        issuer,
        scopes,
    )
    .await
    .map_err(failed)?;
    println!(
        "Log in to {name} ({}): open this link in a browser on any device and confirm the code.\n\n  \
         {}\n\n  Code: {}\n\nWaiting for the approval; the code expires in {} minutes. Ctrl+C cancels.",
        issuer.label,
        login.link(),
        login.user_code(),
        login.expires_in().as_secs().div_ceil(60)
    );
    let cancellation = CancellationToken::new();
    let finished = login.finish(&cancellation, |_| {});
    tokio::pin!(finished);
    let result = tokio::select! {
        result = &mut finished => result,
        _ = tokio::signal::ctrl_c() => {
            cancellation.cancel();
            finished.await
        }
    };
    result.map_err(failed)?;
    println!("Logged in to {name} ({}).", issuer.label);
    Ok(())
}

/// Reads the package's key, hidden on a terminal or as the first line of the input otherwise,
/// and saves it.
fn save_key(credentials: &PackageCredentials, name: &str, label: &str) -> Result<(), String> {
    let entered = if io::stdin().is_terminal() {
        print!("{label} for {name} (hidden as you type): ");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let entered = read_hidden_line().map_err(|error| error.to_string())?;
        println!();
        entered.ok_or_else(|| format!("the login to {name} was cancelled"))?
    } else {
        let mut entered = String::new();
        io::stdin()
            .lock()
            .read_line(&mut entered)
            .map_err(|error| error.to_string())?;
        entered
    };
    let key = ApiKey::new(&entered).map_err(|error| format!("cannot log in to {name}: {error}"))?;
    credentials
        .save_api_key(&key)
        .map_err(|error| format!("cannot log in to {name}: {error}"))?;
    println!("Saved the {label} for {name}.");
    Ok(())
}

/// Reads a line from the terminal without showing it; `None` when Esc or Ctrl+C cancels.
fn read_hidden_line() -> io::Result<Option<String>> {
    terminal::enable_raw_mode()?;
    let entered = read_hidden_keys();
    terminal::disable_raw_mode()?;
    entered
}

fn read_hidden_keys() -> io::Result<Option<String>> {
    let mut entered = String::new();
    loop {
        match event::read()? {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }) => match code {
                KeyCode::Enter => return Ok(Some(entered)),
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    entered.clear();
                }
                KeyCode::Backspace => {
                    entered.pop();
                }
                KeyCode::Char(character) => entered.push(character),
                _ => {}
            },
            Event::Paste(text) => entered.push_str(&text),
            _ => {}
        }
    }
}

/// Deletes the package's saved login, revoking an OAuth token where its server allows.
pub(super) async fn logout(store: &PluginStore, name: &str) -> Result<(), String> {
    let package = installed_package(store, name)?;
    if package.manifest.auth == PackageAuth::None {
        return Err(needs_no_login(name));
    }
    let credentials = store.credentials(name).map_err(|error| error.to_string())?;
    let logged_out = log_out(
        &tickloop_code_settings::package_http::client,
        &credentials,
        &package.manifest.auth,
    )
    .await
    .map_err(|error| format!("cannot log out of {name}: {error}"))?;
    if let Some(failure) = logged_out.revocation_failure {
        println!(
            "The authorization server did not revoke the token ({}); it is deleted here all the same.",
            safe_line(&failure)
        );
    }
    if logged_out.had_credentials {
        println!("Logged out of {name}.");
    } else {
        println!("Tick package {name} was not logged in.");
    }
    Ok(())
}

fn installed_package(store: &PluginStore, name: &str) -> Result<InstalledPlugin, String> {
    store.get(name).map_err(|error| match error {
        PluginError::NotInstalled(_) => {
            format!("{error}; install it with tickloop plugin install {name}")
        }
        _ => error.to_string(),
    })
}

/// Lists the marketplace's packages with the version each offers, the version installed and its
/// login state, and whether this platform has a download.
pub(super) async fn list_available(store: &PluginStore) -> Result<(), String> {
    let source = MarketplaceSource::configured()?;
    let marketplace = source
        .read(&tickloop_code_settings::package_http::client)
        .await?;
    let installed = store.installed().map_err(|error| error.to_string())?;
    println!(
        "Tick marketplace {} by {}: {}",
        marketplace.name,
        marketplace.owner.name,
        safe_line(&source.to_string())
    );
    if marketplace.packages.is_empty() {
        println!("It offers no Tick packages yet.");
        return Ok(());
    }
    let login_of = |name: &str| {
        installed
            .plugins
            .iter()
            .find(|package| package.manifest.name == name)
            .map_or(LoginState::Unknown, |package| store.login_state(package))
    };
    let rows = tickloop_tick_installer::offers(&marketplace, &installed)
        .into_iter()
        .map(|offer| {
            let package = offer.package;
            let (mut status, login) = match offer.installed {
                InstalledVersion::Offered => {
                    ("installed".to_owned(), login_label(login_of(&package.name)))
                }
                InstalledVersion::Other(version) => (
                    format!("installed {version}"),
                    login_label(login_of(&package.name)),
                ),
                InstalledVersion::Unusable(version) => (
                    format!("installed {version} but unusable"),
                    login_label(LoginState::Unknown),
                ),
                InstalledVersion::NotInstalled => ("not installed".to_owned(), "-"),
            };
            if !offer.is_installable {
                status.push_str(&format!("; no prebuilt program for {CURRENT_TARGET}"));
            }
            [
                package.name,
                package.version,
                status,
                login.to_owned(),
                package.description,
            ]
        })
        .collect::<Vec<_>>();
    print_table(&rows);
    Ok(())
}
