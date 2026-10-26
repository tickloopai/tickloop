//! `tickloop plugin`: installs plugins from local directories or archives under the Tickloop config
//! root, enables, disables, lists and removes them, and checks a plugin directory.

use std::{ffi::OsString, path::Path};

use tick_packages::LoginState;
use tick_plugins::{
    Component, InstallOutcome, Installation, InstalledPlugin, InstalledPlugins, PluginError,
    PluginManifest, PluginStore,
};
use tickloop_code_console::terminal::safe_line;
use tickloop_tick_installer::login_label;

const PLUGIN_USAGE: &str = "Usage: tickloop plugin install PLUGIN
       tickloop plugin install --path PATH
       tickloop plugin uninstall PLUGIN [--keep-data]
       tickloop plugin enable PLUGIN
       tickloop plugin disable PLUGIN
       tickloop plugin login PLUGIN [--issuer LABEL]
       tickloop plugin logout PLUGIN
       tickloop plugin list [--available]
       tickloop plugin validate DIRECTORY";
const PLUGIN_GUIDE: &str = "Manage the plugins in ~/.tickloop/plugins, or in TICKLOOP_CONFIG_DIR/plugins when it is set. A plugin is a directory with a .tickloop-plugin/plugin.json, a .claude-plugin/plugin.json or a tick-package.json manifest, or none, when it is named after its directory.
install --path installs a plugin directory or its .tar.gz archive from the local marketplace, replacing the installed plugin of that name even at the same version, and lists the components Tickloop does not use yet. A plugin cannot take the name of an installed plugin from another marketplace.
uninstall removes a plugin and its data directory; --keep-data keeps the data. Its saved login stays.
enable and disable turn a plugin on and off without uninstalling it; a disabled plugin stays disabled when it is installed again. Its Skills, named PLUGIN:SKILL, load or stop loading from the Agent's next request.
list shows the installed plugins: their marketplace, version, whether they are enabled, their login state and the components Tickloop does not use yet.
validate shows what Tickloop reads from a plugin directory: its manifest, Skills, the components it does not use yet, and what it ignores.
install PLUGIN downloads from the tickloop marketplace (TICKLOOP_TICK_MARKETPLACE overrides its URL or file). login and logout manage the plugin's data source credentials. list --available shows the tickloop marketplace.
tickloop tick is a deprecated alias for tickloop plugin.
Installing runs nothing in the plugin, and a plugin's Skills grant no tools; tools still ask as usual.
  -h, --help  Show this help.";
const SUBCOMMANDS: [&str; 8] = [
    "install",
    "login",
    "logout",
    "uninstall",
    "enable",
    "disable",
    "list",
    "validate",
];
const KEEP_DATA_OPTION: &str = "--keep-data";

enum PluginCommand<'a> {
    Help,
    Install(String),
    Login {
        name: String,
        issuer: Option<String>,
    },
    Logout(String),
    ListAvailable,
    InstallPath(&'a Path),
    Uninstall {
        name: String,
        keep_data: bool,
    },
    SetEnabled {
        name: String,
        enabled: bool,
    },
    List,
    Validate(&'a Path),
    Update(Option<String>),
    MarketplaceAdd(String),
    MarketplaceList,
    MarketplaceUpdate(Option<String>),
    MarketplaceRemove(String),
}

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    match parse(arguments)? {
        PluginCommand::Help => println!("{PLUGIN_USAGE}\n\n{PLUGIN_GUIDE}"),
        PluginCommand::Install(name) => {
            super::plugin_marketplace::install(&store()?, &name).await?
        }
        PluginCommand::Update(name) => {
            super::plugin_marketplace::update(&store()?, name.as_deref()).await?
        }
        PluginCommand::Login { name, issuer } => {
            super::plugin_tick::login(&store()?, &name, issuer.as_deref()).await?
        }
        PluginCommand::Logout(name) => super::plugin_tick::logout(&store()?, &name).await?,
        PluginCommand::ListAvailable => super::plugin_tick::list_available(&store()?).await?,
        PluginCommand::InstallPath(path) => install_path(&store()?, path)?,
        PluginCommand::Uninstall { name, keep_data } => uninstall(&store()?, &name, keep_data)?,
        PluginCommand::SetEnabled { name, enabled } => set_enabled(&store()?, &name, enabled)?,
        PluginCommand::List => list(&store()?)?,
        PluginCommand::Validate(path) => validate(path)?,
        PluginCommand::MarketplaceAdd(source) => {
            super::plugin_marketplace::add_marketplace(&store()?, &source).await?
        }
        PluginCommand::MarketplaceUpdate(name) => {
            super::plugin_marketplace::update_marketplaces(&store()?, name.as_deref()).await?
        }
        PluginCommand::MarketplaceList => super::plugin_marketplace::list_marketplaces(&store()?)?,
        PluginCommand::MarketplaceRemove(name) => {
            super::plugin_marketplace::remove_marketplace(&store()?, &name)?
        }
    }
    Ok(())
}

fn parse(arguments: &[OsString]) -> Result<PluginCommand<'_>, String> {
    let is_help = |argument: &OsString| argument == "--help" || argument == "-h";
    let is_option = |argument: &OsString| argument.to_string_lossy().starts_with('-');
    let name = |argument: &OsString| argument.to_string_lossy().into_owned();
    Ok(match arguments {
        [help] if is_help(help) => PluginCommand::Help,
        [command, help] if is_help(help) && SUBCOMMANDS.iter().any(|name| command == name) => {
            PluginCommand::Help
        }
        [command, option, path] if command == "install" && option == "--path" => {
            PluginCommand::InstallPath(Path::new(path))
        }
        [command, plugin] if command == "install" && !is_option(plugin) => {
            PluginCommand::Install(name(plugin))
        }
        [command, plugin] if command == "login" && !is_option(plugin) => PluginCommand::Login {
            name: name(plugin),
            issuer: None,
        },
        [command, plugin, option, issuer]
            if command == "login" && !is_option(plugin) && option == "--issuer" =>
        {
            PluginCommand::Login {
                name: name(plugin),
                issuer: Some(name(issuer)),
            }
        }
        [command, plugin] if command == "logout" && !is_option(plugin) => {
            PluginCommand::Logout(name(plugin))
        }
        [command, option] if command == "list" && option == "--available" => {
            PluginCommand::ListAvailable
        }
        [command, plugin] if command == "uninstall" && !is_option(plugin) => {
            PluginCommand::Uninstall {
                name: name(plugin),
                keep_data: false,
            }
        }
        [command, plugin, option] | [command, option, plugin]
            if command == "uninstall" && option == KEEP_DATA_OPTION && !is_option(plugin) =>
        {
            PluginCommand::Uninstall {
                name: name(plugin),
                keep_data: true,
            }
        }
        [command, plugin] if command == "enable" && !is_option(plugin) => {
            PluginCommand::SetEnabled {
                name: name(plugin),
                enabled: true,
            }
        }
        [command, plugin] if command == "disable" && !is_option(plugin) => {
            PluginCommand::SetEnabled {
                name: name(plugin),
                enabled: false,
            }
        }
        [command] if command == "list" => PluginCommand::List,
        [command] if command == "update" => PluginCommand::Update(None),
        [command, plugin] if command == "update" && !is_option(plugin) => {
            PluginCommand::Update(Some(name(plugin)))
        }
        [command, action, source]
            if command == "marketplace" && action == "add" && !is_option(source) =>
        {
            PluginCommand::MarketplaceAdd(name(source))
        }
        [command, action] if command == "marketplace" && action == "list" => {
            PluginCommand::MarketplaceList
        }
        [command, action] if command == "marketplace" && action == "update" => {
            PluginCommand::MarketplaceUpdate(None)
        }
        [command, action, marketplace]
            if command == "marketplace" && action == "update" && !is_option(marketplace) =>
        {
            PluginCommand::MarketplaceUpdate(Some(name(marketplace)))
        }
        [command, action, marketplace]
            if command == "marketplace" && action == "remove" && !is_option(marketplace) =>
        {
            PluginCommand::MarketplaceRemove(name(marketplace))
        }
        [command, path] if command == "validate" && !is_option(path) => {
            PluginCommand::Validate(Path::new(path))
        }
        _ => return Err(format!("Unknown plugin command. {PLUGIN_USAGE}")),
    })
}

fn store() -> Result<PluginStore, String> {
    let config_root = tickloop_code_settings::config_root_directory()
        .ok_or("cannot find the Tickloop config directory; set HOME or TICKLOOP_CONFIG_DIR")?;
    let store = PluginStore::in_config_root(&config_root);
    for notice in store
        .take_migration_notices()
        .map_err(|error| error.to_string())?
    {
        for line in notice.lines() {
            eprintln!("{}", safe_line(line));
        }
    }
    Ok(store)
}

fn install_path(store: &PluginStore, path: &Path) -> Result<(), String> {
    let installation = store
        .install_path(path)
        .map_err(|error| format!("cannot install {}: {error}", path.display()))?;
    report(&installation);
    Ok(())
}

pub(super) fn report(installation: &Installation) {
    let plugin = &installation.plugin;
    let id = plugin.id();
    let version = &plugin.version;
    let root = safe_line(&plugin.root.display().to_string());
    match &installation.outcome {
        InstallOutcome::Installed => println!("Installed plugin {id} {version} in {root}."),
        InstallOutcome::Replaced { previous_version } => {
            println!("Replaced plugin {id} {previous_version} with {version} in {root}.")
        }
        InstallOutcome::Reinstalled => println!("Reinstalled plugin {id} {version} in {root}."),
        InstallOutcome::AlreadyInstalled => {
            println!("Plugin {id} {version} is already installed in {root}; nothing changed.")
        }
    }
    report_notes(plugin);
}

/// Whether `plugin` stays disabled, that its program runs as the user, and what its manifest
/// leaves unused.
pub(super) fn report_notes(plugin: &InstalledPlugin) {
    if !plugin.enabled {
        println!(
            "It stays disabled; tickloop plugin enable {} turns it on.",
            plugin.name()
        );
    }
    if plugin.manifest.tick.is_some() {
        println!("When opened, this plugin's program runs as your user. Installing runs nothing.");
    }
    report_manifest_notes(&plugin.manifest);
}

/// Prints the components Tickloop does not use yet and what it ignored in the manifest.
fn report_manifest_notes(manifest: &PluginManifest) {
    let unsupported = manifest.unsupported();
    if !unsupported.is_empty() {
        println!(
            "Not supported yet, so not used: {}.",
            component_labels(&unsupported)
        );
    }
    for warning in &manifest.warnings {
        println!("Warning: {}", safe_line(warning));
    }
}

fn component_labels(components: &[Component]) -> String {
    components
        .iter()
        .map(|component| component.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn uninstall(store: &PluginStore, name: &str, keep_data: bool) -> Result<(), String> {
    let uninstalled = store
        .uninstall(name, keep_data)
        .map_err(|error| failure(name, "uninstall", error))?;
    println!(
        "Uninstalled plugin {}@{} {}.",
        uninstalled.name, uninstalled.marketplace, uninstalled.version
    );
    if let Some(data) = uninstalled.data {
        println!(
            "Its data stays in {}.",
            safe_line(&data.display().to_string())
        );
    }
    if let Some(credentials) = uninstalled.credentials {
        println!(
            "Its data source login stays in {}.",
            safe_line(&credentials.display().to_string())
        );
    }
    Ok(())
}

fn set_enabled(store: &PluginStore, name: &str, enabled: bool) -> Result<(), String> {
    let action = if enabled { "enable" } else { "disable" };
    let was_enabled = store
        .get(name)
        .map_err(|error| failure(name, action, error))?
        .enabled;
    let plugin = store
        .set_enabled(name, enabled)
        .map_err(|error| failure(name, action, error))?;
    let id = plugin.id();
    match (was_enabled == enabled, enabled) {
        (true, true) => println!("Plugin {id} is already enabled."),
        (true, false) => println!("Plugin {id} is already disabled."),
        (false, true) => println!("Enabled plugin {id}; its Skills load from the next request."),
        (false, false) => {
            println!("Disabled plugin {id}; its Skills stop loading from the next request.")
        }
    }
    Ok(())
}

/// What a failed `action` on the plugin `name` reports; that it is not installed needs no more.
fn failure(name: &str, action: &str, error: PluginError) -> String {
    match error {
        PluginError::NotInstalled(_) => error.to_string(),
        _ => format!("cannot {action} {name}: {error}"),
    }
}

fn list(store: &PluginStore) -> Result<(), String> {
    let InstalledPlugins { plugins, broken } =
        store.installed().map_err(|error| error.to_string())?;
    let enabled_label = |enabled: bool| if enabled { "enabled" } else { "disabled" };
    let mut rows = plugins
        .iter()
        .map(|plugin| {
            let unsupported = plugin.manifest.unsupported();
            [
                plugin.id(),
                plugin.version.clone(),
                enabled_label(plugin.enabled).to_owned(),
                login_label(store.login_state(plugin)).to_owned(),
                if unsupported.is_empty() {
                    String::new()
                } else {
                    format!("not supported yet: {}", component_labels(&unsupported))
                },
            ]
        })
        .chain(broken.iter().map(|plugin| {
            [
                format!("{}@{}", plugin.name, plugin.marketplace),
                plugin.version.clone(),
                enabled_label(plugin.enabled).to_owned(),
                login_label(LoginState::Unknown).to_owned(),
                format!("unusable: {}; install it again", safe_line(&plugin.reason)),
            ]
        }))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        println!("No plugins are installed.");
        return Ok(());
    }
    rows.sort();
    print_table(&rows);
    Ok(())
}

/// Shows what Tickloop reads from the plugin directory at `path`, or why it cannot use it.
fn validate(path: &Path) -> Result<(), String> {
    let shown = path.display();
    if !path.is_dir() {
        return Err(format!("{shown} is not a plugin directory"));
    }
    let directory_name = path
        .canonicalize()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let manifest = PluginManifest::load(path, &directory_name, None)
        .map_err(|error| format!("{shown} is not a valid plugin: {error}"))?;
    let version = manifest
        .version
        .as_deref()
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    println!(
        "Plugin {}{version}, read from {}.",
        manifest.name,
        manifest.format.label()
    );
    let root = path.canonicalize().map_err(|error| error.to_string())?;
    let skills = tick_plugins::skill_directories(&root, &manifest.skill_paths)
        .iter()
        .map(|directory| {
            safe_line(
                &directory
                    .strip_prefix(&root)
                    .unwrap_or(directory)
                    .display()
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    if skills.is_empty() {
        println!("Skills: none");
    } else {
        println!("Skills: {}", skills.join(", "));
    }
    if let Some(tick) = &manifest.tick {
        let protocol = if tick.protocol {
            "speaks the Tick protocol"
        } else {
            "runs in a terminal"
        };
        println!(
            "Tick program: {} ({protocol})",
            safe_line(&tick.executable.display().to_string())
        );
    }
    report_manifest_notes(&manifest);
    Ok(())
}

/// Prints `rows` with every column but the last padded to its widest cell.
pub(super) fn print_table<const COLUMNS: usize>(rows: &[[String; COLUMNS]]) {
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
        println!("{}", line.trim_end());
    }
}
