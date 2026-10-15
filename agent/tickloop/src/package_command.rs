//! `tickloop tick`: installs, removes and lists the Tick packages under the Tickloop config root,
//! from the Tick marketplace or from local files.

use std::{ffi::OsString, path::Path};

use tick_packages::{
    CURRENT_TARGET, ExpectedArchive, InstallOutcome, Installation, InstalledPackages, PackageError,
    PackageStore, validate_package_name,
};
use tickloop_agent_console::terminal::safe_line;

use crate::marketplace::{self, MarketplaceSource};

const TICK_USAGE: &str = "Usage: tickloop tick install PACKAGE
       tickloop tick install --path PATH
       tickloop tick uninstall PACKAGE
       tickloop tick list [--available]";
const TICK_GUIDE: &str = "Manage the Tick packages in ~/.tickloop/ticks, or in TICKLOOP_CONFIG_DIR/ticks when it is set.
install downloads a package's prebuilt program for this platform from the Tick marketplace, checks its SHA-256 and installs it; an installed package of the version the marketplace offers stays as it is, any other version is replaced.
install --path installs a package directory or its .tar.gz archive, replacing the installed package of that name even at the same version.
uninstall removes a package's program and Skills; data source credentials it keeps elsewhere stay.
list shows the installed packages and their versions; --available shows the packages in the Tick marketplace, which of them are installed, and whether this platform can install them.
The Tick marketplace is the official one, or the https URL or local file TICKLOOP_TICK_MARKETPLACE names.
Installing opens no Tick and grants no market data, account or trading access.
  -h, --help  Show this help.";
const SUBCOMMANDS: [&str; 3] = ["install", "uninstall", "list"];

enum PackageCommand<'a> {
    Help,
    Install(String),
    InstallPath(&'a Path),
    Uninstall(String),
    List,
    ListAvailable,
}

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    match parse(arguments)? {
        PackageCommand::Help => println!("{TICK_USAGE}\n\n{TICK_GUIDE}"),
        PackageCommand::Install(name) => install(&store()?, &name).await?,
        PackageCommand::InstallPath(path) => install_path(&store()?, path)?,
        PackageCommand::Uninstall(name) => uninstall(&store()?, &name)?,
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
        [command] if command == "list" => PackageCommand::List,
        [command, option] if command == "list" && option == "--available" => {
            PackageCommand::ListAvailable
        }
        _ => return Err(format!("Unknown tick command. {TICK_USAGE}")),
    })
}

fn store() -> Result<PackageStore, String> {
    let config_root = tickloop_agent_config::config_root_directory()
        .ok_or("cannot find the Tickloop config directory; set HOME or TICKLOOP_CONFIG_DIR")?;
    Ok(PackageStore::in_config_root(&config_root))
}

/// Installs the version of `name` the marketplace offers, unless it is installed already.
async fn install(store: &PackageStore, name: &str) -> Result<(), String> {
    validate_package_name(name).map_err(|error| error.to_string())?;
    let source = MarketplaceSource::configured()?;
    let marketplace = source.read().await?;
    let package = marketplace.package(name).ok_or_else(|| {
        format!(
            "Tick package {name} is not in the Tick marketplace at {}",
            safe_line(&source.to_string())
        )
    })?;
    let version = &package.version;
    match store.get(name) {
        Ok(installed) if &installed.manifest.version == version => {
            report(Installation {
                package: installed,
                outcome: InstallOutcome::AlreadyInstalled,
            });
            return Ok(());
        }
        Ok(_) | Err(PackageError::NotInstalled(_) | PackageError::Unusable { .. }) => {}
        Err(error) => return Err(error.to_string()),
    }
    let download = package.current_download().ok_or_else(|| {
        format!("Tick package {name} {version} has no prebuilt program for {CURRENT_TARGET}")
    })?;
    let url = source
        .download_url(&download.url)
        .map_err(|reason| format!("cannot download {name} {version}: {reason}"))?;
    let mut file = store.download_file().map_err(|error| error.to_string())?;
    marketplace::download(&url, &mut file)
        .await
        .map_err(|reason| format!("cannot download {name} {version} from {url}: {reason}"))?;
    let expected = ExpectedArchive {
        name,
        version,
        sha256: &download.sha256,
        source: url.as_str(),
    };
    let installation = store
        .install_archive(file.path(), expected)
        .map_err(|error| format!("cannot install {name} {version} from {url}: {error}"))?;
    report(installation);
    Ok(())
}

fn install_path(store: &PackageStore, path: &Path) -> Result<(), String> {
    let installation = store
        .install_path(path)
        .map_err(|error| format!("cannot install {}: {error}", path.display()))?;
    report(installation);
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
    let uninstalled = store.uninstall(name).map_err(|error| match error {
        PackageError::NotInstalled(_) => error.to_string(),
        _ => format!("cannot uninstall {name}: {error}"),
    })?;
    println!(
        "Uninstalled Tick package {} {}.",
        uninstalled.name, uninstalled.version
    );
    if let Some(credentials) = uninstalled.credentials {
        println!("Its data source credentials, if any, stay in {credentials}.");
    }
    Ok(())
}

fn list(store: &PackageStore) -> Result<(), String> {
    let InstalledPackages { packages, broken } =
        store.installed().map_err(|error| error.to_string())?;
    let mut rows = packages
        .iter()
        .map(|package| {
            let manifest = &package.manifest;
            (
                manifest.name.as_str(),
                manifest.version.as_str(),
                manifest.description.clone(),
            )
        })
        .chain(broken.iter().map(|package| {
            (
                package.name.as_str(),
                package.version.as_str(),
                format!("unusable: {}; install it again", safe_line(&package.reason)),
            )
        }))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        println!("No Tick packages are installed.");
        return Ok(());
    }
    rows.sort_by(|left, right| left.0.cmp(right.0));
    let name_width = rows.iter().map(|row| row.0.len()).max().unwrap_or(0);
    let version_width = rows.iter().map(|row| row.1.len()).max().unwrap_or(0);
    for (name, version, detail) in rows {
        println!("{name:<name_width$}  {version:<version_width$}  {detail}");
    }
    Ok(())
}

/// Lists the marketplace's packages with the version each offers, the version installed, and
/// whether this platform has a download.
async fn list_available(store: &PackageStore) -> Result<(), String> {
    let source = MarketplaceSource::configured()?;
    let marketplace = source.read().await?;
    let InstalledPackages { packages, broken } =
        store.installed().map_err(|error| error.to_string())?;
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
    let mut rows = marketplace
        .packages
        .iter()
        .map(|offered| {
            let installed = packages
                .iter()
                .find(|package| package.manifest.name == offered.name);
            let unusable = broken.iter().find(|package| package.name == offered.name);
            let mut status = match (installed, unusable) {
                (Some(package), _) if package.manifest.version == offered.version => {
                    "installed".to_owned()
                }
                (Some(package), _) => format!("installed {}", package.manifest.version),
                (None, Some(package)) => format!("installed {} but unusable", package.version),
                (None, None) => "not installed".to_owned(),
            };
            if offered.current_download().is_none() {
                status.push_str(&format!("; no prebuilt program for {CURRENT_TARGET}"));
            }
            (
                offered.name.as_str(),
                offered.version.as_str(),
                status,
                offered.description.as_str(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.0.cmp(right.0));
    let name_width = rows.iter().map(|row| row.0.len()).max().unwrap_or(0);
    let version_width = rows.iter().map(|row| row.1.len()).max().unwrap_or(0);
    let status_width = rows.iter().map(|row| row.2.len()).max().unwrap_or(0);
    for (name, version, status, description) in rows {
        println!(
            "{name:<name_width$}  {version:<version_width$}  {status:<status_width$}  {description}"
        );
    }
    Ok(())
}
