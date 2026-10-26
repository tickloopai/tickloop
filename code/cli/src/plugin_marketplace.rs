//! `tickloop plugin marketplace …`, and installing, updating and listing plugins by name from
//! every registered marketplace.

use std::collections::BTreeMap;

use tick_plugins::{
    InstallOutcome, Installation, LOCAL_MARKETPLACE, MarketplacePlugin, MarketplaceRegistration,
    MarketplaceSource, PluginStore, TICKLOOP_MARKETPLACE,
};
use tickloop_code_console::terminal::safe_line;
use tickloop_tick_installer::{
    PluginRequest, install_from, plugin_http, read_marketplace, read_marketplaces, source_label,
};

use super::plugin_command::{print_table, report, report_notes};

const SHORT_COMMIT_LENGTH: usize = 12;
const HTTP: &tickloop_tick_installer::HttpClient = &tickloop_code_settings::package_http::client;

pub(super) async fn add_marketplace(store: &PluginStore, text: &str) -> Result<(), String> {
    let failed = |reason: String| format!("cannot add marketplace {}: {reason}", safe_line(text));
    let source = MarketplaceSource::parse(text).map_err(|error| failed(error.to_string()))?;
    let catalog = store
        .add_marketplace(&source, &plugin_http(HTTP))
        .await
        .map_err(|error| failed(error.to_string()))?;
    println!(
        "Added marketplace {} from {}; it offers {}.",
        catalog.registration.name,
        safe_line(&source_label(&catalog.registration.source)),
        plugin_count(catalog.plugins.len())
    );
    Ok(())
}

/// Reads one marketplace afresh, or every registered one; one that cannot be read leaves the
/// others.
pub(super) async fn update_marketplaces(
    store: &PluginStore,
    name: Option<&str>,
) -> Result<(), String> {
    let registrations = store.marketplaces().map_err(|error| error.to_string())?;
    let Some(name) = name else {
        let mut failures = 0;
        for registration in &registrations {
            match read_marketplace(HTTP, store, registration, true).await {
                Ok(plugins) => report_updated_marketplace(&registration.name, plugins.len()),
                Err(reason) => {
                    failures += 1;
                    eprintln!(
                        "Error: cannot update marketplace {}: {}",
                        registration.name,
                        safe_line(&reason)
                    );
                }
            }
        }
        return match failures {
            0 => Ok(()),
            _ => Err(format!(
                "{failures} of {} marketplaces could not be updated",
                registrations.len()
            )),
        };
    };
    let failed = |reason: String| format!("cannot update marketplace {name}: {reason}");
    let registration = registrations
        .iter()
        .find(|registration| registration.name == name)
        .ok_or_else(|| failed(format!("marketplace {name} is not registered")))?;
    let plugins = read_marketplace(HTTP, store, registration, true)
        .await
        .map_err(failed)?;
    report_updated_marketplace(name, plugins.len());
    Ok(())
}

fn report_updated_marketplace(name: &str, count: usize) {
    println!(
        "Updated marketplace {name}; it offers {}.",
        plugin_count(count)
    );
}

fn plugin_count(count: usize) -> String {
    match count {
        1 => "1 plugin".to_owned(),
        count => format!("{count} plugins"),
    }
}

pub(super) fn list_marketplaces(store: &PluginStore) -> Result<(), String> {
    let rows = store
        .marketplaces()
        .map_err(|error| error.to_string())?
        .iter()
        .map(|registration| {
            [
                registration.name.clone(),
                safe_line(&source_label(&registration.source)),
                marketplace_state(registration),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&rows);
    Ok(())
}

fn marketplace_state(registration: &MarketplaceRegistration) -> String {
    let fetched = match (&registration.refreshed_at, &registration.commit) {
        _ if registration.name == TICKLOOP_MARKETPLACE => "read on every use".to_owned(),
        (None, _) => "not fetched yet".to_owned(),
        (Some(at), None) => format!("updated {at}"),
        (Some(at), Some(commit)) => format!("updated {at} at {}", short_commit(commit)),
    };
    if registration.builtin {
        format!("built in; {fetched}")
    } else {
        fetched
    }
}

fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(SHORT_COMMIT_LENGTH)]
}

pub(super) fn remove_marketplace(store: &PluginStore, name: &str) -> Result<(), String> {
    store
        .remove_marketplace(name)
        .map_err(|error| error.to_string())?;
    println!("Removed marketplace {name}.");
    let installed = store.installed().map_err(|error| error.to_string())?;
    let remaining = installed
        .plugins
        .iter()
        .filter(|plugin| plugin.marketplace == name)
        .map(|plugin| plugin.id())
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        println!(
            "Its plugins stay installed: {}; tickloop plugin uninstall removes one.",
            remaining.join(", ")
        );
    }
    Ok(())
}

fn registration(store: &PluginStore, name: &str) -> Result<MarketplaceRegistration, String> {
    store
        .marketplaces()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|registration| registration.name == name)
        .ok_or_else(|| format!("marketplace {name} is not registered"))
}

/// Installs `NAME@MARKETPLACE`, or `NAME` from the one marketplace that offers it.
pub(super) async fn install(store: &PluginStore, text: &str) -> Result<(), String> {
    let request = PluginRequest::parse(text)?;
    let name = &request.name;
    let installation = match request.marketplace.as_deref() {
        Some(LOCAL_MARKETPLACE) => {
            return Err(format!(
                "plugins of the {LOCAL_MARKETPLACE} marketplace install with tickloop plugin install --path PATH"
            ));
        }
        Some(TICKLOOP_MARKETPLACE) => return super::plugin_tick::install(store, name).await,
        Some(marketplace) => {
            let registration = registration(store, marketplace)?;
            let plugins = read_marketplace(HTTP, store, &registration, false)
                .await
                .map_err(|reason| format!("cannot read marketplace {marketplace}: {reason}"))?;
            let plugin = plugins
                .iter()
                .find(|plugin| &plugin.name == name)
                .ok_or_else(|| {
                    format!(
                        "marketplace {marketplace} has no plugin {name}; \
                         tickloop plugin marketplace update {marketplace} reads it again"
                    )
                })?;
            install_from(HTTP, store, marketplace, plugin).await?
        }
        None => {
            let (marketplace, plugin) = choose_offer(store, name).await?;
            install_from(HTTP, store, &marketplace, &plugin).await?
        }
    };
    report(&installation);
    Ok(())
}

/// The marketplace that offers `name`: the only one, or the one it is installed from. A
/// marketplace that cannot be read is reported and passed over.
async fn choose_offer(
    store: &PluginStore,
    name: &str,
) -> Result<(String, MarketplacePlugin), String> {
    let mut offers = Vec::new();
    for offer in read_marketplaces(HTTP, store, false).await? {
        match offer.plugins {
            Ok(plugins) => offers.extend(
                plugins
                    .into_iter()
                    .filter(|plugin| plugin.name == name)
                    .map(|plugin| (offer.registration.name.clone(), plugin)),
            ),
            Err(reason) => eprintln!(
                "Warning: cannot read marketplace {}: {}",
                offer.registration.name,
                safe_line(&reason)
            ),
        }
    }
    let installed_from = store.get(name).ok().map(|plugin| plugin.marketplace);
    if let Some(position) = offers
        .iter()
        .position(|(marketplace, _)| Some(marketplace) == installed_from.as_ref())
    {
        return Ok(offers.swap_remove(position));
    }
    match offers.len() {
        0 => Err(format!(
            "no marketplace offers plugin {name}; tickloop plugin list --available shows what they offer"
        )),
        1 => Ok(offers.remove(0)),
        _ => Err(format!(
            "plugin {name} is offered by several marketplaces: {}; choose one with tickloop plugin install {name}@MARKETPLACE",
            offers
                .iter()
                .map(|(marketplace, _)| format!("{name}@{marketplace}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Updates one installed plugin, or every one, from its marketplace read afresh.
pub(super) async fn update(store: &PluginStore, name: Option<&str>) -> Result<(), String> {
    let installed = store.installed().map_err(|error| error.to_string())?;
    let mut targets = installed
        .plugins
        .iter()
        .map(|plugin| (plugin.name().to_owned(), plugin.marketplace.clone()))
        .chain(
            installed
                .broken
                .iter()
                .map(|plugin| (plugin.name.clone(), plugin.marketplace.clone())),
        )
        .collect::<Vec<_>>();
    targets.sort();
    if let Some(name) = name {
        targets.retain(|(installed, _)| installed == name);
        if targets.is_empty() {
            return Err(format!("plugin {name} is not installed"));
        }
    } else if targets.is_empty() {
        println!("No plugins are installed.");
        return Ok(());
    }
    let mut marketplaces = BTreeMap::new();
    let mut failures = 0;
    for (name, marketplace) in &targets {
        if marketplace == LOCAL_MARKETPLACE {
            let installed_locally = format!(
                "{name}@{marketplace} was installed from a local path; \
                 tickloop plugin install --path updates it"
            );
            if targets.len() == 1 {
                return Err(format!("plugin {installed_locally}"));
            }
            println!("Plugin {installed_locally}.");
            continue;
        }
        if !marketplaces.contains_key(marketplace) {
            let plugins = match registration(store, marketplace) {
                Ok(registration) => read_marketplace(HTTP, store, &registration, true)
                    .await
                    .map_err(|reason| format!("cannot read marketplace {marketplace}: {reason}")),
                Err(reason) => Err(reason),
            };
            marketplaces.insert(marketplace.clone(), plugins);
        }
        let result = match &marketplaces[marketplace] {
            Ok(plugins) => match plugins.iter().find(|plugin| &plugin.name == name) {
                Some(plugin) => install_from(HTTP, store, marketplace, plugin).await,
                None => Err(format!(
                    "cannot update {name}@{marketplace}: marketplace {marketplace} no longer offers it"
                )),
            },
            Err(reason) => Err(format!("cannot update {name}@{marketplace}: {reason}")),
        };
        match result {
            Ok(installation) => report_update(&installation),
            Err(message) if targets.len() == 1 => return Err(message),
            Err(message) => {
                failures += 1;
                eprintln!("Error: {}", safe_line(&message));
            }
        }
    }
    match failures {
        0 => Ok(()),
        _ => Err(format!(
            "{failures} of {} plugins could not be updated",
            targets.len()
        )),
    }
}

fn report_update(installation: &Installation) {
    let plugin = &installation.plugin;
    let id = plugin.id();
    let version = &plugin.version;
    let root = safe_line(&plugin.root.display().to_string());
    match &installation.outcome {
        InstallOutcome::AlreadyInstalled => {
            println!("Plugin {id} {version} is up to date.");
            return;
        }
        InstallOutcome::Installed => println!("Installed plugin {id} {version} in {root}."),
        InstallOutcome::Replaced { previous_version } => {
            println!("Updated plugin {id} from {previous_version} to {version} in {root}.")
        }
        InstallOutcome::Reinstalled => println!("Reinstalled plugin {id} {version} in {root}."),
    }
    report_notes(plugin);
}
