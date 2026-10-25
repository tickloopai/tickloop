//! `tickloop tick`: installs, removes and lists the Tick packages under the Tickloop config root,
//! from the Tick marketplace or from local files, and logs them in and out of their data sources.

use std::{
    ffi::OsString,
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use tick_packages::{
    ApiKey, CURRENT_TARGET, InstallOutcome, Installation, InstalledPackage, InstalledPackages,
    LoginState, OAuthIssuer, PackageAuth, PackageCredentials, PackageError, PackageStore,
    validate_package_name,
};
use tickloop_code_console::terminal::safe_line;
use tickloop_tick_installer::{
    InstalledVersion, MarketplaceSource, log_out, login_label, needs_no_login, start_login,
};
use tokio_util::sync::CancellationToken;

const TICK_USAGE: &str = "Usage: tickloop tick install PACKAGE
       tickloop tick install --path PATH
       tickloop tick uninstall PACKAGE
       tickloop tick login PACKAGE [--issuer LABEL]
       tickloop tick logout PACKAGE
       tickloop tick list [--available]";
const TICK_GUIDE: &str = "Manage the Tick packages in ~/.tickloop/ticks, or in TICKLOOP_CONFIG_DIR/ticks when it is set.
install downloads a package's prebuilt program for this platform from the Tick marketplace, checks its SHA-256 and installs it; an installed package of the version the marketplace offers stays as it is, any other version is replaced.
install --path installs a package directory or its .tar.gz archive, replacing the installed package of that name even at the same version.
uninstall removes a package's program and Skills; its saved login, in credentials/PACKAGE beside ticks, stays.
login logs a package in to its data source as its manifest declares: for OAuth it prints a link and a code to approve in a browser on any device and waits, asking which authorization server to use when the package has several unless --issuer names one; for an API key it reads the key without showing it. The Agent's /ticks logs packages in too. logout deletes the saved login, revoking an OAuth token where its server allows.
list shows the installed packages, their versions and their login state; --available shows the packages in the Tick marketplace, which of them are installed and logged in, and whether this platform can install them.
The Tick marketplace is the official one, or the https URL or local file TICKLOOP_TICK_MARKETPLACE names.
Installing opens no Tick and grants no market data, account or trading access.
  -h, --help  Show this help.";
const SUBCOMMANDS: [&str; 5] = ["install", "uninstall", "login", "logout", "list"];

enum PackageCommand<'a> {
    Help,
    Install(String),
    InstallPath(&'a Path),
    Uninstall(String),
    Login {
        name: String,
        issuer: Option<String>,
    },
    Logout(String),
    List,
    ListAvailable,
}

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    match parse(arguments)? {
        PackageCommand::Help => println!("{TICK_USAGE}\n\n{TICK_GUIDE}"),
        PackageCommand::Install(name) => install(&store()?, &name).await?,
        PackageCommand::InstallPath(path) => install_path(&store()?, path)?,
        PackageCommand::Uninstall(name) => uninstall(&store()?, &name)?,
        PackageCommand::Login { name, issuer } => {
            login(&store()?, &name, issuer.as_deref()).await?
        }
        PackageCommand::Logout(name) => logout(&store()?, &name).await?,
        PackageCommand::List => list(&store()?)?,
        PackageCommand::ListAvailable => list_available(&store()?).await?,
    }
    Ok(())
}

fn parse(arguments: &[OsString]) -> Result<PackageCommand<'_>, String> {
    let is_help = |argument: &OsString| argument == "--help" || argument == "-h";
    let is_option = |argument: &OsString| argument.to_string_lossy().starts_with('-');
    Ok(match arguments {
        [help] if is_help(help) => PackageCommand::Help,
        [command, help] if is_help(help) && SUBCOMMANDS.iter().any(|name| command == name) => {
            PackageCommand::Help
        }
        [command, option, path] if command == "install" && option == "--path" => {
            PackageCommand::InstallPath(Path::new(path))
        }
        [command, name] if command == "install" && !is_option(name) => {
            PackageCommand::Install(name.to_string_lossy().into_owned())
        }
        [command, name] if command == "uninstall" => {
            PackageCommand::Uninstall(name.to_string_lossy().into_owned())
        }
        [command, name] if command == "login" && !is_option(name) => PackageCommand::Login {
            name: name.to_string_lossy().into_owned(),
            issuer: None,
        },
        [command, name, option, issuer]
            if command == "login" && !is_option(name) && option == "--issuer" =>
        {
            PackageCommand::Login {
                name: name.to_string_lossy().into_owned(),
                issuer: Some(issuer.to_string_lossy().into_owned()),
            }
        }
        [command, name] if command == "logout" && !is_option(name) => {
            PackageCommand::Logout(name.to_string_lossy().into_owned())
        }
        [command] if command == "list" => PackageCommand::List,
        [command, option] if command == "list" && option == "--available" => {
            PackageCommand::ListAvailable
        }
        _ => return Err(format!("Unknown tick command. {TICK_USAGE}")),
    })
}

fn store() -> Result<PackageStore, String> {
    let config_root = tickloop_code_settings::config_root_directory()
        .ok_or("cannot find the Tickloop config directory; set HOME or TICKLOOP_CONFIG_DIR")?;
    Ok(PackageStore::in_config_root(&config_root))
}

/// Installs the version of `name` the marketplace offers, unless it is installed already.
async fn install(store: &PackageStore, name: &str) -> Result<(), String> {
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
        tickloop_tick_installer::install(
            &tickloop_code_settings::package_http::client,
            store,
            &source,
            package,
        )
        .await?,
    );
    Ok(())
}

fn install_path(store: &PackageStore, path: &Path) -> Result<(), String> {
    report(tickloop_tick_installer::install_path(store, path)?);
    Ok(())
}

fn report(installation: Installation) {
    let package = &installation.package;
    let name = &package.manifest.name;
    let version = &package.manifest.version;
    let root = safe_line(&package.root.display().to_string());
    match installation.outcome {
        InstallOutcome::Installed => println!("Installed Tick package {name} {version} in {root}."),
        InstallOutcome::Replaced { previous_version } => {
            println!("Replaced Tick package {name} {previous_version} with {version} in {root}.")
        }
        InstallOutcome::Reinstalled => {
            println!("Reinstalled Tick package {name} {version} in {root}.")
        }
        InstallOutcome::AlreadyInstalled => {
            println!(
                "Tick package {name} {version} is already installed in {root}; nothing changed."
            )
        }
    }
}

fn uninstall(store: &PackageStore, name: &str) -> Result<(), String> {
    let uninstalled = tickloop_tick_installer::uninstall(store, name)?;
    println!(
        "Uninstalled Tick package {} {}.",
        uninstalled.name, uninstalled.version
    );
    if let Some(credentials) = uninstalled.credentials {
        println!(
            "Its data source login stays in {}.",
            safe_line(&credentials.display().to_string())
        );
    }
    Ok(())
}

/// Logs the package in to its data source as its manifest declares.
async fn login(store: &PackageStore, name: &str, issuer: Option<&str>) -> Result<(), String> {
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
             tickloop tick login {name} --issuer LABEL"
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
async fn logout(store: &PackageStore, name: &str) -> Result<(), String> {
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

fn installed_package(store: &PackageStore, name: &str) -> Result<InstalledPackage, String> {
    store.get(name).map_err(|error| match error {
        PackageError::NotInstalled(_) => {
            format!("{error}; install it with tickloop tick install {name}")
        }
        _ => error.to_string(),
    })
}

fn list(store: &PackageStore) -> Result<(), String> {
    let InstalledPackages { packages, broken } =
        store.installed().map_err(|error| error.to_string())?;
    let mut rows = packages
        .iter()
        .map(|package| {
            let manifest = &package.manifest;
            [
                manifest.name.clone(),
                manifest.version.clone(),
                login_label(store.login_state(package)).to_owned(),
                manifest.description.clone(),
            ]
        })
        .chain(broken.iter().map(|package| {
            [
                package.name.clone(),
                package.version.clone(),
                login_label(LoginState::Unknown).to_owned(),
                format!("unusable: {}; install it again", safe_line(&package.reason)),
            ]
        }))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        println!("No Tick packages are installed.");
        return Ok(());
    }
    rows.sort();
    print_table(&rows);
    Ok(())
}

/// Lists the marketplace's packages with the version each offers, the version installed and its
/// login state, and whether this platform has a download.
async fn list_available(store: &PackageStore) -> Result<(), String> {
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
            .packages
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

/// Prints `rows` with every column but the last padded to its widest cell.
fn print_table<const COLUMNS: usize>(rows: &[[String; COLUMNS]]) {
    let widths: [usize; COLUMNS] =
        std::array::from_fn(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0));
    for row in rows {
        let mut line = String::new();
        for (column, cell) in row.iter().enumerate() {
            if column + 1 == COLUMNS {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}  ", width = widths[column]));
            }
        }
        println!("{line}");
    }
}
