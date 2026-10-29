//! `tickloop plugin marketplace …`, and installing, updating and listing Ticks and plugins by name
//! from every registered marketplace.

use std::collections::BTreeMap;

use tick_plugins::{
    InstallOutcome, Installation, LOCAL_MARKETPLACE, MarketplacePlugin, PluginStore,
    TICKLOOP_MARKETPLACE,
};
use tickloop_code_console::terminal::safe_line;
use tickloop_tick_installer::{
    PluginKind, PluginRequest, install_from, installed_status, marketplace_registration,
    marketplace_state, plugin_count, read_marketplace, read_marketplaces, source_label,
    unavailable_reason, update_marketplace,
};

use super::plugin_command::{other_kind, print_table, report, report_notes};

const NO_MARKETPLACE_READ: &str = "no marketplace could be read";
const HTTP: &tickloop_tick_installer::HttpClient = &tickloop_code_settings::package_http::client;

pub(super) async fn add_marketplace(store: &PluginStore, text: &str) -> Result<(), String> {
    let catalog = tickloop_tick_installer::add_marketplace(HTTP, store, text)
        .await
        .map_err(|reason| format!("cannot add marketplace {}: {reason}", safe_line(text)))?;
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
    let Some(name) = name else {
        let registrations = store.marketplaces().map_err(|error| error.to_string())?;
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
    let plugins = update_marketplace(HTTP, store, name)
        .await
        .map_err(|reason| format!("cannot update marketplace {name}: {reason}"))?;
    report_updated_marketplace(name, plugins.len());
    Ok(())
}

fn report_updated_marketplace(name: &str, count: usize) {
    println!(
        "Updated marketplace {name}; it offers {}.",
        plugin_count(count)
    );
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

pub(super) fn remove_marketplace(store: &PluginStore, name: &str) -> Result<(), String> {
    let remaining = tickloop_tick_installer::remove_marketplace(store, name)?;
    if name == tick_plugins::CLAUDE_OFFICIAL_MARKETPLACE {
        println!(
            "Removed marketplace {name}; add it back with tickloop plugin marketplace add anthropics/claude-plugins-official."
        );
    } else {
        println!("Removed marketplace {name}.");
    }
    if !remaining.is_empty() {
        println!(
            "Its plugins stay installed: {}; tickloop plugin uninstall removes one.",
            remaining.join(", ")
        );
    }
    Ok(())
}

/// Installs `NAME@MARKETPLACE`, or `NAME` from the one marketplace that offers it, when what the
/// marketplace offers is of `kind`; `typed` is the command as the user gave it.
pub(super) async fn install(
    store: &PluginStore,
    kind: PluginKind,
    text: &str,
    typed: &str,
) -> Result<(), String> {
    let request = PluginRequest::parse(text)?;
    let name = &request.name;
    let installation = match request.marketplace.as_deref() {
        Some(LOCAL_MARKETPLACE) => {
            return Err(format!(
                "{} of the {LOCAL_MARKETPLACE} marketplace install with {} install --path PATH",
                kind.plural(),
                kind.command()
            ));
        }
        Some(TICKLOOP_MARKETPLACE) if kind == PluginKind::Tick => {
            return super::plugin_tick::install(store, name).await;
        }
        Some(TICKLOOP_MARKETPLACE) => return Err(other_kind(name, PluginKind::Tick, typed)),
        Some(marketplace) => {
            let registration = marketplace_registration(store, marketplace)?;
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
            let offered = PluginKind::of_offer(plugin);
            if offered != kind {
                return Err(other_kind_offer(name, offered, typed));
            }
            install_from(HTTP, store, marketplace, plugin).await?
        }
        None => {
            let (marketplace, plugin) = choose_offer(store, kind, name, typed).await?;
            install_from(HTTP, store, &marketplace, &plugin).await?
        }
    };
    report(store, &installation, kind);
    super::plugin_config::configure(store, &installation.plugin, false)?;
    Ok(())
}

/// Why the offer `name` belongs to the other command: a Tick marketplace package is known to carry
/// a Tick program, while any other offer is only not known to until it is fetched.
fn other_kind_offer(name: &str, offered: PluginKind, typed: &str) -> String {
    match offered {
        PluginKind::Tick => other_kind(name, offered, typed),
        PluginKind::Plugin => format!(
            "{name} is not offered as a Tick; use {} {typed}",
            offered.command()
        ),
    }
}

/// The marketplace that offers `name` as `kind`: the only one, or the one it is installed from.
/// A marketplace that cannot be read is reported and passed over.
async fn choose_offer(
    store: &PluginStore,
    kind: PluginKind,
    name: &str,
    typed: &str,
) -> Result<(String, MarketplacePlugin), String> {
    let mut offers = Vec::new();
    let mut is_any_readable = false;
    for offer in read_marketplaces(HTTP, store, false).await? {
        match offer.plugins {
            Ok(plugins) => {
                is_any_readable = true;
                offers.extend(
                    plugins
                        .into_iter()
                        .filter(|plugin| plugin.name == name)
                        .map(|plugin| (offer.registration.name.clone(), plugin)),
                )
            }
            Err(reason) => eprintln!(
                "Warning: cannot read marketplace {}: {}",
                offer.registration.name,
                safe_line(&reason)
            ),
        }
    }
    if !is_any_readable {
        return Err(NO_MARKETPLACE_READ.to_owned());
    }
    let (mut offers, others): (Vec<_>, Vec<_>) = offers
        .into_iter()
        .partition(|(_, plugin)| PluginKind::of_offer(plugin) == kind);
    if offers.is_empty()
        && let Some((_, other)) = others.first()
    {
        return Err(other_kind_offer(name, PluginKind::of_offer(other), typed));
    }
    let installed = store.installed().map_err(|error| error.to_string())?;
    let installed_from = installed
        .plugins
        .iter()
        .find(|plugin| plugin.name() == name)
        .map(|plugin| &plugin.marketplace)
        .or_else(|| {
            installed
                .broken
                .iter()
                .find(|plugin| plugin.name == name)
                .map(|plugin| &plugin.marketplace)
        });
    if let Some(position) = offers
        .iter()
        .position(|(marketplace, _)| Some(marketplace) == installed_from)
    {
        return Ok(offers.swap_remove(position));
    }
    match offers.len() {
        0 => Err(format!(
            "no marketplace that could be read offers {} {name}; \
             {} list --available shows what they offer",
            kind.noun(),
            kind.command()
        )),
        1 => Ok(offers.remove(0)),
        _ => Err(format!(
            "{} {name} is offered by several marketplaces: {}; choose one with {} install {name}@MARKETPLACE",
            kind.noun(),
            offers
                .iter()
                .map(|(marketplace, _)| format!("{name}@{marketplace}"))
                .collect::<Vec<_>>()
                .join(", "),
            kind.command()
        )),
    }
}

/// Updates one installed plugin, or every one of `kind`, from its marketplace read afresh; the
/// caller has checked that a named one, if usable, is of `kind`.
pub(super) async fn update(
    store: &PluginStore,
    kind: PluginKind,
    name: Option<&str>,
) -> Result<(), String> {
    let installed = store.installed().map_err(|error| error.to_string())?;
    let is_single = name.is_some();
    let mut targets = installed
        .plugins
        .iter()
        .filter(|plugin| is_single || PluginKind::of_installed(plugin) == kind)
        .map(|plugin| (plugin.name().to_owned(), plugin.marketplace.clone()))
        .chain(
            installed
                .broken
                .iter()
                .filter(|plugin| is_single || kind.lists_broken(plugin))
                .map(|plugin| (plugin.name.clone(), plugin.marketplace.clone())),
        )
        .collect::<Vec<_>>();
    targets.sort();
    if let Some(name) = name {
        targets.retain(|(installed, _)| installed == name);
        if targets.is_empty() {
            return Err(format!("{} {name} is not installed", kind.noun()));
        }
    } else if targets.is_empty() {
        println!("No {} are installed.", kind.plural());
        return Ok(());
    }
    let mut marketplaces = BTreeMap::new();
    let mut failures = 0;
    for (name, marketplace) in &targets {
        if marketplace == LOCAL_MARKETPLACE {
            let installed_locally = format!(
                "{name}@{marketplace} was installed from a local path; \
                 {} install --path updates it",
                kind.command()
            );
            if is_single {
                return Err(format!("{} {installed_locally}", kind.noun()));
            }
            println!("{} {installed_locally}.", kind.label());
            continue;
        }
        if !marketplaces.contains_key(marketplace) {
            let plugins = match marketplace_registration(store, marketplace) {
                Ok(registration) => read_marketplace(HTTP, store, &registration, true)
                    .await
                    .map_err(|reason| format!("cannot read marketplace {marketplace}: {reason}")),
                Err(reason) if is_single => {
                    return Err(format!("cannot update {name}@{marketplace}: {reason}"));
                }
                Err(reason) => {
                    println!(
                        "{} {name}@{marketplace} is not updated: {reason}; \
                         tickloop plugin marketplace add registers it again.",
                        kind.label()
                    );
                    continue;
                }
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
            Ok(installation) => report_update(store, &installation),
            Err(message) if is_single => return Err(message),
            Err(message) => {
                failures += 1;
                eprintln!("Error: {}", safe_line(&message));
            }
        }
    }
    match failures {
        0 => Ok(()),
        _ => Err(format!(
            "{failures} of {} {} could not be updated",
            targets.len(),
            kind.plural()
        )),
    }
}

fn report_update(store: &PluginStore, installation: &Installation) {
    let plugin = &installation.plugin;
    let id = plugin.id();
    let version = &plugin.version;
    let root = safe_line(&plugin.root.display().to_string());
    let kind = PluginKind::of_installed(plugin);
    let noun = kind.noun();
    match &installation.outcome {
        InstallOutcome::AlreadyInstalled => {
            println!("{} {id} {version} is up to date.", kind.label());
            return;
        }
        InstallOutcome::Installed => println!("Installed {noun} {id} {version} in {root}."),
        InstallOutcome::Replaced { previous_version } => {
            println!("Updated {noun} {id} from {previous_version} to {version} in {root}.")
        }
        InstallOutcome::Reinstalled => println!("Reinstalled {noun} {id} {version} in {root}."),
    }
    report_notes(store, plugin);
}

/// Lists what every marketplace offers of `kind`, merged by name, beside what is installed; a
/// marketplace that cannot be read is reported and passed over.
pub(super) async fn list_available(store: &PluginStore, kind: PluginKind) -> Result<(), String> {
    let offers = read_marketplaces(HTTP, store, false).await?;
    let installed = store.installed().map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    let mut is_any_readable = false;
    for offer in offers {
        let marketplace = &offer.registration.name;
        match offer.plugins {
            Ok(plugins) => {
                is_any_readable = true;
                let offered = plugins
                    .iter()
                    .filter(|plugin| PluginKind::of_offer(plugin) == kind);
                rows.extend(offered.map(|plugin| {
                    let mut status = installed_status(&installed, marketplace, plugin);
                    if let Some(reason) = unavailable_reason(plugin) {
                        status.push_str(&format!("; {reason}"));
                    }
                    [
                        format!("{}@{marketplace}", plugin.name),
                        plugin.version.as_deref().map_or("-".to_owned(), safe_line),
                        status,
                        plugin
                            .description
                            .as_deref()
                            .map(safe_line)
                            .unwrap_or_default(),
                    ]
                }))
            }
            Err(reason) => eprintln!(
                "Warning: cannot read marketplace {marketplace}: {}",
                safe_line(&reason)
            ),
        }
    }
    if !is_any_readable {
        return Err(NO_MARKETPLACE_READ.to_owned());
    }
    if rows.is_empty() {
        println!("No marketplace offers {} yet.", kind.plural());
        return Ok(());
    }
    rows.sort_by(|left, right| {
        let key = |row: &[String; 4]| {
            row[0]
                .split_once('@')
                .map(|(name, marketplace)| (name.to_owned(), marketplace.to_owned()))
        };
        key(left).cmp(&key(right))
    });
    print_table(&rows);
    Ok(())
}
