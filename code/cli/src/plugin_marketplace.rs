//! `tickloop plugin marketplace …`, and installing, updating and listing plugins by name from
//! every registered marketplace.

use tick_plugins::{MarketplaceRegistration, MarketplaceSource, PluginStore, TICKLOOP_MARKETPLACE};
use tickloop_code_console::terminal::safe_line;
use tickloop_tick_installer::{plugin_http, read_marketplace, source_label};

use super::plugin_command::print_table;

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
                        "Cannot update marketplace {}: {}",
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
