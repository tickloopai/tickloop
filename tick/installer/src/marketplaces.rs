//! Plugins by name from every registered marketplace. The `tickloop` marketplace is read afresh on
//! every use and installs through [`crate::install`], as before; the others are read through the
//! plugin store, which caches each one until it is refreshed.

use tick_packages::{CURRENT_TARGET, Marketplace, validate_package_name};
use tick_plugins::{
    Installation, InstalledPlugins, MarketplacePlugin, MarketplacePluginSource,
    MarketplaceRegistration, MarketplaceSource as PluginMarketplaceSource, PluginError,
    PluginStore, TICKLOOP_MARKETPLACE, validate_plugin_name,
};

use crate::{HttpClient, HttpRequest, MarketplaceSource};

const SHORT_COMMIT_LENGTH: usize = 12;

/// A plugin named on the command line: `NAME` or `NAME@MARKETPLACE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRequest {
    pub name: String,
    pub marketplace: Option<String>,
}

impl PluginRequest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let (name, marketplace) = match text.split_once('@') {
            Some((name, marketplace)) => (name, Some(marketplace)),
            None => (text, None),
        };
        validate_plugin_name(name).map_err(|error| error.to_string())?;
        if let Some(marketplace) = marketplace {
            validate_plugin_name(marketplace)
                .map_err(|_| format!("{marketplace:?} is not a marketplace name"))?;
        }
        Ok(Self {
            name: name.to_owned(),
            marketplace: marketplace.map(str::to_owned),
        })
    }
}

/// What a registered marketplace offers, or why it cannot be read.
#[derive(Clone, Debug)]
pub struct MarketplaceOffer {
    pub registration: MarketplaceRegistration,
    pub plugins: Result<Vec<MarketplacePlugin>, String>,
}

/// The plugin store's HTTP client for the caller's.
pub fn plugin_http(
    http: &'static HttpClient,
) -> impl Fn(&reqwest::Url, tick_plugins::HttpRequest) -> Result<reqwest::Client, String>
+ Send
+ Sync
+ 'static {
    move |url, request| {
        http(
            url,
            match request {
                tick_plugins::HttpRequest::Marketplace => HttpRequest::Marketplace,
                tick_plugins::HttpRequest::Download => HttpRequest::Download,
            },
        )
    }
}

/// Where a marketplace is read from, as a user would type it.
pub fn source_label(source: &PluginMarketplaceSource) -> String {
    match source {
        PluginMarketplaceSource::Git(git) => match &git.reference {
            Some(reference) => format!("{}#{reference}", git.url),
            None => git.url.clone(),
        },
        PluginMarketplaceSource::Directory(path) | PluginMarketplaceSource::File(path) => {
            path.display().to_string()
        }
        PluginMarketplaceSource::Url(url) => url.clone(),
    }
}

/// Whether the marketplace `registration` is built in, and when it was last read.
pub fn marketplace_state(registration: &MarketplaceRegistration) -> String {
    let fetched = match (&registration.refreshed_at, &registration.commit) {
        _ if registration.name == TICKLOOP_MARKETPLACE => "read on every use".to_owned(),
        (None, _) => "not fetched yet".to_owned(),
        (Some(at), None) => format!("updated {at}"),
        (Some(at), Some(commit)) => format!(
            "updated {at} at {}",
            &commit[..commit.len().min(SHORT_COMMIT_LENGTH)]
        ),
    };
    if registration.builtin {
        format!("built in; {fetched}")
    } else {
        fetched
    }
}

pub fn plugin_count(count: usize) -> String {
    match count {
        1 => "1 plugin".to_owned(),
        count => format!("{count} plugins"),
    }
}

/// Unregisters the marketplace `name`; the plugins installed from it stay, and their IDs are
/// returned.
pub fn remove_marketplace(store: &PluginStore, name: &str) -> Result<Vec<String>, String> {
    store
        .remove_marketplace(name)
        .map_err(|error| error.to_string())?;
    let installed = store.installed().map_err(|error| error.to_string())?;
    Ok(installed
        .plugins
        .iter()
        .filter(|plugin| plugin.marketplace == name)
        .map(|plugin| plugin.id())
        .collect())
}

/// Why `plugin` cannot be installed, if it cannot.
pub fn unavailable_reason(plugin: &MarketplacePlugin) -> Option<String> {
    match &plugin.source {
        MarketplacePluginSource::Archive(package) if package.current_download().is_none() => {
            Some(format!("no prebuilt program for {CURRENT_TARGET}"))
        }
        MarketplacePluginSource::Unsupported(source) => Some(
            match source.get("source").and_then(serde_json::Value::as_str) {
                Some(kind) if validate_plugin_name(kind).is_ok() => {
                    format!("{kind} sources are not supported yet")
                }
                _ => "its source is not supported yet".to_owned(),
            },
        ),
        _ => None,
    }
}

/// Reads what the marketplace `registration` offers: afresh when `refresh` is set or for the
/// `tickloop` marketplace, from the store's cache otherwise, fetching it the first time.
pub async fn read_marketplace(
    http: &'static HttpClient,
    store: &PluginStore,
    registration: &MarketplaceRegistration,
    refresh: bool,
) -> Result<Vec<MarketplacePlugin>, String> {
    if registration.name == TICKLOOP_MARKETPLACE {
        let marketplace = MarketplaceSource::configured()?.read(http).await?;
        return Ok(tick_marketplace_plugins(marketplace));
    }
    let plugin_http = plugin_http(http);
    let catalog = if refresh {
        store
            .refresh_marketplace(&registration.name, &plugin_http)
            .await
    } else {
        store.marketplace(&registration.name, &plugin_http).await
    };
    catalog
        .map(|catalog| catalog.plugins)
        .map_err(|error| error.to_string())
}

fn tick_marketplace_plugins(marketplace: Marketplace) -> Vec<MarketplacePlugin> {
    marketplace
        .packages
        .into_iter()
        .map(|package| MarketplacePlugin {
            name: package.name.clone(),
            description: Some(package.description.clone()),
            version: Some(package.version.clone()),
            source: MarketplacePluginSource::Archive(package),
            manifest: Default::default(),
        })
        .collect()
}

/// Reads every registered marketplace, each on its own, so one that cannot be read leaves the
/// others; each offer carries its registration as the read left it.
pub async fn read_marketplaces(
    http: &'static HttpClient,
    store: &PluginStore,
    refresh: bool,
) -> Result<Vec<MarketplaceOffer>, String> {
    let mut offers = Vec::new();
    for registration in store.marketplaces().map_err(|error| error.to_string())? {
        let plugins = read_marketplace(http, store, &registration, refresh).await;
        offers.push(MarketplaceOffer {
            registration,
            plugins,
        });
    }
    let current = store.marketplaces().map_err(|error| error.to_string())?;
    for offer in &mut offers {
        if let Some(registration) = current
            .iter()
            .find(|registration| registration.name == offer.registration.name)
        {
            offer.registration = registration.clone();
        }
    }
    Ok(offers)
}

/// Installs `plugin` as the marketplace `marketplace` lists it; a plugin of its name from another
/// marketplace is refused before anything is fetched.
pub async fn install_from(
    http: &'static HttpClient,
    store: &PluginStore,
    marketplace: &str,
    plugin: &MarketplacePlugin,
) -> Result<Installation, String> {
    let id = format!("{}@{marketplace}", plugin.name);
    if let Some(reason) = unavailable_reason(plugin) {
        return Err(format!("cannot install {id}: {reason}"));
    }
    match store.get(&plugin.name) {
        Ok(installed) if installed.marketplace != marketplace => {
            return Err(PluginError::NameTaken {
                name: plugin.name.clone(),
                marketplace: installed.marketplace,
            }
            .to_string());
        }
        Ok(_) | Err(PluginError::NotInstalled(_) | PluginError::Unusable { .. }) => {}
        Err(error) => return Err(error.to_string()),
    }
    if let (TICKLOOP_MARKETPLACE, MarketplacePluginSource::Archive(package)) =
        (marketplace, &plugin.source)
    {
        validate_package_name(&package.name).map_err(|error| error.to_string())?;
        return crate::install(http, store, &MarketplaceSource::configured()?, package).await;
    }
    store
        .install_marketplace_plugin(marketplace, &plugin.name, &plugin_http(http))
        .await
        .map_err(|error| format!("cannot install {id}: {error}"))
}

/// What is installed of the plugin `offered` the marketplace `marketplace` lists.
pub fn installed_status(
    installed: &InstalledPlugins,
    marketplace: &str,
    offered: &MarketplacePlugin,
) -> String {
    let usable = installed
        .plugins
        .iter()
        .find(|plugin| plugin.name() == offered.name);
    let unusable = installed
        .broken
        .iter()
        .find(|plugin| plugin.name == offered.name);
    match (usable, unusable) {
        (Some(plugin), _) if plugin.marketplace == marketplace => {
            if offered.version.as_ref() == Some(&plugin.version) {
                "installed".to_owned()
            } else {
                format!("installed {}", plugin.version)
            }
        }
        (None, Some(plugin)) if plugin.marketplace == marketplace => {
            format!("installed {} but unusable", plugin.version)
        }
        (Some(plugin), _) => format!("not installed; {} is installed", plugin.id()),
        (None, Some(plugin)) => format!(
            "not installed; {}@{} is installed",
            plugin.name, plugin.marketplace
        ),
        (None, None) => "not installed".to_owned(),
    }
}
