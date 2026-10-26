//! Installing, updating and removing Tick packages, from the Tick marketplace or from local
//! files, and logging them in to and out of their data sources. The Console's `/ticks` and
//! `tickloop tick` both go through it, so they check and fail alike: a failed install leaves no
//! temporary file and the installed package as it was.

mod login;
mod marketplace;
mod marketplaces;

use std::path::Path;

use tick_packages::{CURRENT_TARGET, LoginState, Marketplace, MarketplacePackage};
use tick_plugins::{
    ExpectedArchive, InstallOutcome, Installation, InstalledPlugins, PluginError, PluginStore,
    TICKLOOP_MARKETPLACE, Uninstalled,
};

pub use login::{LoggedOut, log_out, start_login};
pub use marketplace::MarketplaceSource;
pub use marketplaces::{
    MarketplaceOffer, PluginRequest, install_from, installed_status, plugin_http, read_marketplace,
    read_marketplaces, source_label, unavailable_reason,
};
pub use tick_oauth::{DeviceLogin, LoginError};

/// Installs `package` as the marketplace at `source` offers it: an installed package of its
/// version stays as it is and is not downloaded again; any other version is replaced.
pub async fn install(
    http: &HttpClient,
    store: &PluginStore,
    source: &MarketplaceSource,
    package: &MarketplacePackage,
) -> Result<Installation, String> {
    let MarketplacePackage { name, version, .. } = package;
    match store.get(name) {
        Ok(installed) if installed.marketplace != TICKLOOP_MARKETPLACE => {
            return Err(PluginError::NameTaken {
                name: name.clone(),
                marketplace: installed.marketplace,
            }
            .to_string());
        }
        Ok(installed) if &installed.version == version => {
            return Ok(Installation {
                plugin: installed,
                outcome: InstallOutcome::AlreadyInstalled,
            });
        }
        Ok(_) | Err(PluginError::NotInstalled(_) | PluginError::Unusable { .. }) => {}
        Err(error) => return Err(error.to_string()),
    }
    let download = package.current_download().ok_or_else(|| {
        format!("Tick package {name} {version} has no prebuilt program for {CURRENT_TARGET}")
    })?;
    let url = source
        .download_url(&download.url)
        .map_err(|reason| format!("cannot download {name} {version}: {reason}"))?;
    let mut file = store.download_file().map_err(|error| error.to_string())?;
    marketplace::download(http, &url, &mut file)
        .await
        .map_err(|reason| format!("cannot download {name} {version} from {url}: {reason}"))?;
    let store = store.clone();
    let (name, version, sha256) = (name.clone(), version.clone(), download.sha256.clone());
    // Checking and unpacking the archive is file work that may take a while.
    tokio::task::spawn_blocking(move || {
        let expected = ExpectedArchive {
            name: &name,
            version: &version,
            sha256: &sha256,
            marketplace: TICKLOOP_MARKETPLACE,
            url: url.as_str(),
        };
        store
            .install_archive(file.path(), expected)
            .map_err(|error| format!("cannot install {name} {version} from {url}: {error}"))
    })
    .await
    .map_err(|error| format!("cannot install {}: {error}", package.name))?
}

/// Installs the package directory or `.tar.gz` archive at `path`, replacing the installed package
/// of its name even at the same version.
pub fn install_path(store: &PluginStore, path: &Path) -> Result<Installation, String> {
    store
        .install_path(path)
        .map_err(|error| format!("cannot install {}: {error}", path.display()))
}

/// Removes the package's program and Skills; its data source credentials stay.
pub fn uninstall(store: &PluginStore, name: &str) -> Result<Uninstalled, String> {
    store.uninstall(name, false).map_err(|error| match error {
        PluginError::NotInstalled(_) => error.to_string(),
        _ => format!("cannot uninstall {name}: {error}"),
    })
}

/// What is installed of a package the marketplace offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstalledVersion {
    NotInstalled,
    /// The version the marketplace offers.
    Offered,
    /// Another version, which installing the offered one replaces.
    Other(String),
    /// A registered version whose files can no longer be used.
    Unusable(String),
}

/// A package the marketplace offers, beside what is installed of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub package: MarketplacePackage,
    pub installed: InstalledVersion,
    /// Whether the marketplace has a prebuilt program for this platform.
    pub is_installable: bool,
}

/// The marketplace's packages by name, each with what is installed of it.
pub fn offers(marketplace: &Marketplace, installed: &InstalledPlugins) -> Vec<Offer> {
    let mut offers = marketplace
        .packages
        .iter()
        .map(|package| {
            let usable = installed
                .plugins
                .iter()
                .find(|installed| installed.manifest.name == package.name);
            let unusable = installed
                .broken
                .iter()
                .find(|broken| broken.name == package.name);
            let installed = match (usable, unusable) {
                (Some(usable), _) if usable.version == package.version => InstalledVersion::Offered,
                (Some(usable), _) => InstalledVersion::Other(usable.version.clone()),
                (None, Some(broken)) => InstalledVersion::Unusable(broken.version.clone()),
                (None, None) => InstalledVersion::NotInstalled,
            };
            Offer {
                package: package.clone(),
                installed,
                is_installable: package.current_download().is_some(),
            }
        })
        .collect::<Vec<_>>();
    offers.sort_by(|left, right| left.package.name.cmp(&right.package.name));
    offers
}

/// Why a package whose `auth` is `none` cannot be logged in or out.
pub fn needs_no_login(name: &str) -> String {
    format!("Tick package {name} needs no login")
}

/// How `tickloop tick list` and `/ticks` name a package's login state.
pub fn login_label(state: LoginState) -> &'static str {
    match state {
        LoginState::LoggedIn => "logged in",
        LoginState::LoggedOut => "needs login",
        LoginState::Expired => "login expired",
        LoginState::NotRequired => "no login needed",
        LoginState::Unknown => "login unknown",
    }
}

/// The caller supplies clients with its proxy, TLS and network policy for each destination.
pub type HttpClient =
    dyn Fn(&reqwest::Url, HttpRequest) -> Result<reqwest::Client, String> + Send + Sync;

#[derive(Clone, Copy, Debug)]
pub enum HttpRequest {
    Marketplace,
    Download,
    OAuth,
}
