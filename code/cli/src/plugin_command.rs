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

const PLUGIN_USAGE: &str = "Usage: tickloop plugin install PLUGIN[@MARKETPLACE]
       tickloop plugin install --path PATH
       tickloop plugin update [PLUGIN]
       tickloop plugin uninstall PLUGIN [--keep-data]
       tickloop plugin enable PLUGIN
       tickloop plugin disable PLUGIN
       tickloop plugin login PLUGIN [--issuer LABEL]
       tickloop plugin logout PLUGIN
       tickloop plugin list [--available]
       tickloop plugin validate DIRECTORY [--name NAME]
       tickloop plugin marketplace add SOURCE
       tickloop plugin marketplace list
       tickloop plugin marketplace update [MARKETPLACE]
       tickloop plugin marketplace remove MARKETPLACE";
const PLUGIN_GUIDE: &str = "Manage the plugins in ~/.tickloop/plugins, or in TICKLOOP_CONFIG_DIR/plugins when it is set. A plugin is a directory with a .tickloop-plugin/plugin.json, a .claude-plugin/plugin.json or a tick-package.json manifest, or none, when it is named after its directory.
install PLUGIN installs the plugin of that name from the one marketplace that offers it, or from the one it is installed from; when several offer it, it lists them, and PLUGIN@MARKETPLACE chooses. A marketplace is read as last fetched; marketplace update reads it again. A plugin cannot take the name of an installed plugin from another marketplace.
install --path installs a plugin directory or its .tar.gz archive from the local marketplace, replacing the installed plugin of that name even at the same version, and lists the components Tickloop does not use yet.
update reads the plugin's marketplace again and installs what it offers now, unless that is installed already: the same version, or for Git sources the same commit. Without PLUGIN it updates every plugin except those installed with --path.
uninstall removes a plugin and its data directory; --keep-data keeps the data. Its saved login stays.
enable and disable turn a plugin on and off without uninstalling it; a disabled plugin stays disabled when it is installed again. Its Skills, named PLUGIN:SKILL, load or stop loading from the Agent's next request.
list shows the installed plugins: their marketplace, version, whether they are enabled, their login state and the components Tickloop does not use yet. list --available shows what every marketplace offers: its latest version, what is installed and why a plugin cannot be installed. A marketplace that cannot be read is reported and the others are shown.
validate shows what Tickloop reads from a plugin directory: its manifest, Skills, the components it does not use yet, and what it ignores. --name uses a marketplace entry name as the installation identity and keeps the declared name for display.
login and logout manage the plugin's data source credentials.
marketplace add registers a marketplace from a GitHub OWNER/REPO[#REF], a Git URL (https, ssh or file), a local directory, or an https URL of a marketplace.json; it takes the name its marketplace file gives. marketplace list shows them, marketplace update reads one or every one again, and marketplace remove unregisters one and keeps the plugins installed from it. The built-in marketplaces cannot be removed: tickloop, read on every use (TICKLOOP_TICK_MARKETPLACE overrides its URL or file), and claude-plugins-official, fetched with git the first time it is used.
tickloop tick is a deprecated alias for tickloop plugin.
Installing runs nothing in the plugin, and a plugin's Skills grant no tools; tools still ask as usual.
  -h, --help  Show this help.";
const SUBCOMMANDS: [&str; 10] = [
    "install",
    "update",
    "login",
    "logout",
    "uninstall",
    "enable",
    "disable",
    "list",
    "validate",
    "marketplace",
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
    Validate {
        path: &'a Path,
        marketplace_name: Option<String>,
    },
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
        PluginCommand::ListAvailable => {
            super::plugin_marketplace::list_available(&store()?).await?
        }
        PluginCommand::InstallPath(path) => install_path(&store()?, path)?,
        PluginCommand::Uninstall { name, keep_data } => uninstall(&store()?, &name, keep_data)?,
        PluginCommand::SetEnabled { name, enabled } => set_enabled(&store()?, &name, enabled)?,
        PluginCommand::List => list(&store()?)?,
        PluginCommand::Validate {
            path,
            marketplace_name,
        } => validate(path, marketplace_name.as_deref())?,
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
        [command, _, help] if command == "marketplace" && is_help(help) => PluginCommand::Help,
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
        [command, path] if command == "validate" && !is_option(path) => PluginCommand::Validate {
            path: Path::new(path),
            marketplace_name: None,
        },
        [command, path, option, identity]
            if command == "validate" && !is_option(path) && option == "--name" =>
        {
            PluginCommand::Validate {
                path: Path::new(path),
                marketplace_name: Some(name(identity)),
            }
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

/// The name its manifest gives `plugin`, whether it stays disabled, that its program runs as the user, and what its manifest
/// leaves unused.
pub(super) fn report_notes(plugin: &InstalledPlugin) {
    if let Some(display_name) = &plugin.manifest.display_name {
        println!("Its manifest calls it {}.", safe_line(display_name));
    }
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
            let notes = plugin
                .manifest
                .display_name
                .iter()
                .map(|display_name| format!("manifest name: {}", safe_line(display_name)))
                .chain(
                    (!unsupported.is_empty())
                        .then(|| format!("not supported yet: {}", component_labels(&unsupported))),
                )
                .collect::<Vec<_>>();
            [
                plugin.id(),
                plugin.version.clone(),
                enabled_label(plugin.enabled).to_owned(),
                login_label(store.login_state(plugin)).to_owned(),
                notes.join("; "),
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
fn validate(path: &Path, marketplace_name: Option<&str>) -> Result<(), String> {
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
    let manifest = match marketplace_name {
        Some(name) => PluginManifest::load_from_marketplace(path, name, None),
        None => PluginManifest::load(path, &directory_name, None),
    }
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
    if let Some(display_name) = &manifest.display_name {
        println!("Its manifest calls it {}.", safe_line(display_name));
    }
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
