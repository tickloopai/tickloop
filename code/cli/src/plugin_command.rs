//! `tickloop tick` and `tickloop plugin`: each installs the plugins of its [`PluginKind`] by name
//! or from local directories or archives under the Tickloop config root, enables, disables, lists
//! and removes them. Only `tick` logs in, and only `plugin` manages marketplaces and checks a
//! plugin directory.

use std::{collections::BTreeSet, ffi::OsString, path::Path};

use tick_packages::LoginState;
use tick_plugins::{
    Component, HookSupport, InstallOutcome, Installation, InstalledPlugin, InstalledPlugins,
    PluginError, PluginManifest, PluginStore,
};
use tickloop_code_console::terminal::{hooks_to_confirm_summary, safe_line};
use tickloop_tick_installer::{PluginKind, login_label};

const TICK_USAGE: &str = "Usage: tickloop tick install TICK[@MARKETPLACE]
       tickloop tick install --path PATH
       tickloop tick update [TICK]
       tickloop tick uninstall TICK [--keep-data]
       tickloop tick enable TICK
       tickloop tick disable TICK
       tickloop tick login TICK [--issuer LABEL]
       tickloop tick logout TICK
       tickloop tick list [--available]";
const TICK_GUIDE: &str = "Manage Ticks: the plugins that carry a Tick program, which the Agent opens as a Tick. They are kept with the other plugins in ~/.tickloop/plugins, or in TICKLOOP_CONFIG_DIR/plugins when it is set; tickloop plugin manages the plugins without a Tick program.
install TICK installs the Tick of that name from the one marketplace that offers it, or from the one it is installed from; when several offer it, it lists them, and TICK@MARKETPLACE chooses. A Tick cannot take the name of an installed plugin from another marketplace.
install --path installs a Tick directory or its .tar.gz archive from the local marketplace, replacing the installed Tick of that name even at the same version.
update reads the Tick's marketplace again and installs what it offers now, unless that version is installed already. Without TICK it updates every Tick except those installed with --path.
uninstall removes a Tick and its data directory; --keep-data keeps the data. Its saved login stays, and its running Ticks keep running until closed.
enable and disable turn a Tick on and off without uninstalling it; a disabled Tick cannot be opened by name, stays disabled when it is installed again, and its Skills, named TICK:SKILL, stop loading from the Agent's next request.
list shows the installed Ticks: their marketplace, version, whether they are enabled and their login state. list --available shows the Ticks every marketplace offers, with what is installed and why one cannot be installed; the built-in tickloop marketplace, read on every use (TICKLOOP_TICK_MARKETPLACE overrides its URL or file), lists them.
login and logout manage the Tick's data source credentials: an OAuth login shows a link and code to approve in a browser on any device, and a key is read without showing it.
tickloop plugin marketplace adds, lists, updates and removes the marketplaces both commands read.
Installing runs nothing in the Tick; its program runs as your user when opened. Its Skills grant no tools; tools still ask as usual.
  -h, --help  Show this help.";
const PLUGIN_USAGE: &str = "Usage: tickloop plugin install PLUGIN[@MARKETPLACE]
       tickloop plugin install --path PATH
       tickloop plugin update [PLUGIN]
       tickloop plugin uninstall PLUGIN [--keep-data]
       tickloop plugin enable PLUGIN
       tickloop plugin disable PLUGIN
       tickloop plugin configure PLUGIN
       tickloop plugin list [--available]
       tickloop plugin validate DIRECTORY [--name NAME]
       tickloop plugin marketplace add SOURCE
       tickloop plugin marketplace list
       tickloop plugin marketplace update [MARKETPLACE]
       tickloop plugin marketplace remove MARKETPLACE";
const PLUGIN_GUIDE: &str = "Manage the plugins without a Tick program in ~/.tickloop/plugins, or in TICKLOOP_CONFIG_DIR/plugins when it is set; tickloop tick manages Ticks, the plugins that carry one. A plugin is a directory with a .tickloop-plugin/plugin.json, a .claude-plugin/plugin.json or a tick-package.json manifest, or none, when it is named after its directory.
install PLUGIN installs the plugin of that name from the one marketplace that offers it, or from the one it is installed from; when several offer it, it lists them, and PLUGIN@MARKETPLACE chooses. A marketplace is read as last fetched; marketplace update reads it again. A plugin cannot take the name of an installed plugin from another marketplace.
install --path installs a plugin directory or its .tar.gz archive from the local marketplace, replacing the installed plugin of that name even at the same version, and lists the components Tickloop does not use yet and how many of its hooks need confirming.
update reads the plugin's marketplace again and installs what it offers now, unless that is installed already: the same version, or for Git sources the same commit. Without PLUGIN it updates every plugin except those installed with --path.
uninstall removes a plugin, its data directory and the trust records of its hooks; --keep-data keeps the data.
enable and disable turn a plugin on and off without uninstalling it; a disabled plugin stays disabled when it is installed again. Its Skills, named PLUGIN:SKILL, load or stop loading from the Agent's next request.
list shows the installed plugins: their marketplace, version, whether they are enabled, their login state, the components Tickloop does not use yet and how many of their hooks need confirming. list --available shows what every marketplace offers: its latest version, what is installed and why a plugin cannot be installed. A marketplace that cannot be read is reported and the others are shown.
validate shows what Tickloop reads from a plugin directory: its manifest, Skills, the hook events, types and fields it runs and those it does not, the components it does not use yet, and what it ignores. --name uses a marketplace entry name as the installation identity and keeps the declared name for display.
marketplace add registers a marketplace from a GitHub OWNER/REPO[#REF], a Git URL (https, ssh or file), a local directory, or an https URL of a marketplace.json; it takes the name its marketplace file gives. marketplace list shows them, marketplace update reads one or every one again, and marketplace remove unregisters one and keeps the plugins installed from it. The built-in tickloop marketplace cannot be removed; it is read on every use (TICKLOOP_TICK_MARKETPLACE overrides its URL or file). claude-plugins-official is registered by default and fetched with git on first use. Its removal is remembered across restarts; add it back with tickloop plugin marketplace add anthropics/claude-plugins-official. Both names remain reserved for their official sources.
Installing runs nothing in the plugin, and a plugin's Skills grant no tools; tools still ask as usual.
  -h, --help  Show this help.";
const SUBCOMMANDS: [&str; 11] = [
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
    "configure",
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
    Configure(String),
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

impl PluginCommand<'_> {
    /// Whether `tickloop tick` or `tickloop plugin`, as `kind` names it, takes this command.
    fn is_offered_to(&self, kind: PluginKind) -> bool {
        match self {
            Self::Login { .. } | Self::Logout(_) => kind == PluginKind::Tick,
            Self::Configure(_)
            | Self::Validate { .. }
            | Self::MarketplaceAdd(_)
            | Self::MarketplaceList
            | Self::MarketplaceUpdate(_)
            | Self::MarketplaceRemove(_) => kind == PluginKind::Plugin,
            _ => true,
        }
    }

    /// The installed plugin the command acts on by name.
    fn installed_name(&self) -> Option<&str> {
        match self {
            Self::Login { name, .. }
            | Self::Configure(name)
            | Self::Logout(name)
            | Self::Uninstall { name, .. }
            | Self::SetEnabled { name, .. }
            | Self::Update(Some(name)) => Some(name),
            _ => None,
        }
    }
}

fn usage(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Plugin => PLUGIN_USAGE,
        PluginKind::Tick => TICK_USAGE,
    }
}

/// Runs `tickloop tick` or `tickloop plugin`, as `kind` names it, with `arguments`.
pub async fn run(kind: PluginKind, arguments: &[OsString]) -> Result<(), String> {
    let command = parse(arguments);
    if kind == PluginKind::Plugin
        && let Some(PluginCommand::Login { name, .. }) = &command
        && let Ok(plugin) = store()?.get(name)
        && PluginKind::of_installed(&plugin) == PluginKind::Plugin
        && plugin.manifest.mcp_servers.values().any(|entry| {
            entry["url"].is_string()
                && !entry["headers"].as_object().is_some_and(|headers| {
                    headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("authorization"))
                })
        })
    {
        return Err(format!(
            "This plugin uses MCP server login; log in from /mcp or with tickloop mcp login plugin:{name}:<server>."
        ));
    }
    let command = command
        .filter(|command| command.is_offered_to(kind))
        .ok_or_else(|| {
            format!(
                "Unknown {} command. {}",
                kind.noun().to_lowercase(),
                usage(kind)
            )
        })?;
    match command {
        PluginCommand::Help => {
            let guide = match kind {
                PluginKind::Plugin => PLUGIN_GUIDE,
                PluginKind::Tick => TICK_GUIDE,
            };
            println!("{}\n\n{guide}", usage(kind));
            return Ok(());
        }
        PluginCommand::Validate {
            path,
            marketplace_name,
        } => return validate(path, marketplace_name.as_deref()),
        _ => {}
    }
    let typed = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let store = store()?;
    if let Some(name) = command.installed_name() {
        ensure_installed_kind(&store, kind, name, &typed)?;
    }
    match command {
        PluginCommand::Help | PluginCommand::Validate { .. } => {}
        PluginCommand::Install(name) => {
            super::plugin_marketplace::install(&store, kind, &name, &typed).await?
        }
        PluginCommand::Update(name) => {
            super::plugin_marketplace::update(&store, kind, name.as_deref()).await?
        }
        PluginCommand::Login { name, issuer } => {
            super::plugin_tick::login(&store, &name, issuer.as_deref()).await?
        }
        PluginCommand::Logout(name) => super::plugin_tick::logout(&store, &name).await?,
        PluginCommand::ListAvailable => {
            super::plugin_marketplace::list_available(&store, kind).await?
        }
        PluginCommand::InstallPath(path) => install_path(&store, kind, path, &typed)?,
        PluginCommand::Uninstall { name, keep_data } => uninstall(&store, kind, &name, keep_data)?,
        PluginCommand::SetEnabled { name, enabled } => set_enabled(&store, &name, enabled)?,
        PluginCommand::List => list(&store, kind)?,
        PluginCommand::Configure(name) => {
            let plugin = store.get(&name).map_err(|error| error.to_string())?;
            super::plugin_config::configure(&store, &plugin, true)?;
        }
        PluginCommand::MarketplaceAdd(source) => {
            super::plugin_marketplace::add_marketplace(&store, &source).await?
        }
        PluginCommand::MarketplaceUpdate(name) => {
            super::plugin_marketplace::update_marketplaces(&store, name.as_deref()).await?
        }
        PluginCommand::MarketplaceList => super::plugin_marketplace::list_marketplaces(&store)?,
        PluginCommand::MarketplaceRemove(name) => {
            super::plugin_marketplace::remove_marketplace(&store, &name)?
        }
    }
    Ok(())
}

/// Refuses `typed` on an installed plugin of the other kind; an unusable plugin's kind cannot be
/// read, so either command takes it.
fn ensure_installed_kind(
    store: &PluginStore,
    kind: PluginKind,
    name: &str,
    typed: &str,
) -> Result<(), String> {
    match store.get(name) {
        Ok(plugin) if PluginKind::of_installed(&plugin) != kind => {
            Err(other_kind(name, PluginKind::of_installed(&plugin), typed))
        }
        _ => Ok(()),
    }
}

/// Why the command for one kind does not take `name`, which is of `actual` kind, and the command
/// that runs `typed` for it.
pub(super) fn other_kind(name: &str, actual: PluginKind, typed: &str) -> String {
    let what = match actual {
        PluginKind::Tick => "carries a Tick program",
        PluginKind::Plugin => "has no Tick program",
    };
    format!("{name} {what}; use {} {typed}", actual.command())
}

fn parse(arguments: &[OsString]) -> Option<PluginCommand<'_>> {
    let is_help = |argument: &OsString| argument == "--help" || argument == "-h";
    let is_option = |argument: &OsString| argument.to_string_lossy().starts_with('-');
    let name = |argument: &OsString| argument.to_string_lossy().into_owned();
    Some(match arguments {
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
        [command, plugin] if command == "configure" && !is_option(plugin) => {
            PluginCommand::Configure(name(plugin))
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
        _ => return None,
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

/// Installs the directory or archive at `path`; a directory whose manifest shows the other kind is
/// refused before anything is copied, while an archive's kind shows only once it is installed.
fn install_path(
    store: &PluginStore,
    kind: PluginKind,
    path: &Path,
    typed: &str,
) -> Result<(), String> {
    if path.is_dir() {
        let directory_name = path
            .canonicalize()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if let Ok(manifest) = PluginManifest::load(path, &directory_name, None)
            && PluginKind::of_manifest(&manifest) != kind
        {
            return Err(other_kind(
                &manifest.name,
                PluginKind::of_manifest(&manifest),
                typed,
            ));
        }
    }
    let installation = store
        .install_path(path)
        .map_err(|error| format!("cannot install {}: {error}", path.display()))?;
    report(store, &installation, kind);
    super::plugin_config::configure(store, &installation.plugin, false)?;
    Ok(())
}

/// Reports what installing did; `kind` is the command that installed it, so a plugin that turns
/// out to be of the other kind says which command manages it.
pub(super) fn report(store: &PluginStore, installation: &Installation, kind: PluginKind) {
    let plugin = &installation.plugin;
    let id = plugin.id();
    let version = &plugin.version;
    let root = safe_line(&plugin.root.display().to_string());
    let actual = PluginKind::of_installed(plugin);
    let noun = actual.noun();
    match &installation.outcome {
        InstallOutcome::Installed => println!("Installed {noun} {id} {version} in {root}."),
        InstallOutcome::Replaced { previous_version } => {
            println!("Replaced {noun} {id} {previous_version} with {version} in {root}.")
        }
        InstallOutcome::Reinstalled => println!("Reinstalled {noun} {id} {version} in {root}."),
        InstallOutcome::AlreadyInstalled => println!(
            "{} {id} {version} is already installed in {root}; nothing changed.",
            actual.label()
        ),
    }
    report_notes(store, plugin);
    if actual != kind {
        let what = match actual {
            PluginKind::Tick => "It carries a Tick program, so tickloop tick and /ticks manage it.",
            PluginKind::Plugin => {
                "It has no Tick program, so tickloop plugin and /plugins manage it."
            }
        };
        println!("{what}");
    }
}

/// The name its manifest gives `plugin`, whether it stays disabled, that its program runs as the
/// user, what its manifest leaves unused and how many of its hooks need confirming.
pub(super) fn report_notes(store: &PluginStore, plugin: &InstalledPlugin) {
    if let Some(display_name) = &plugin.manifest.display_name {
        println!("Its manifest calls it {}.", safe_line(display_name));
    }
    if !plugin.enabled {
        println!(
            "It stays disabled; {} enable {} turns it on.",
            PluginKind::of_installed(plugin).command(),
            plugin.name()
        );
    }
    if plugin.manifest.tick.is_some() {
        println!("When opened, its Tick program runs as your user. Installing runs nothing.");
    }
    report_unsupported(&plugin.manifest);
    if let Some(hooks) = hooks_to_confirm(store, plugin) {
        println!("{hooks}.");
    }
    report_warnings(&plugin.manifest);
    super::plugin_mcp::installed(plugin);
}

fn report_unsupported(manifest: &PluginManifest) {
    let unsupported = manifest.unsupported();
    if !unsupported.is_empty() {
        println!(
            "Not supported yet, so not used: {}.",
            component_labels(&unsupported)
        );
    }
}

fn report_warnings(manifest: &PluginManifest) {
    for warning in &manifest.warnings {
        println!("Warning: {}", safe_line(warning));
    }
}

/// How many of the plugin's hooks wait for the user to confirm them, when any do.
fn hooks_to_confirm(store: &PluginStore, plugin: &InstalledPlugin) -> Option<String> {
    hooks_to_confirm_summary(
        tickloop_code_settings::UserSettings::in_root(store.config_root())
            .plugin_hooks_to_confirm(plugin),
    )
}

/// The hook events, types and fields the manifest uses, split into those Tickloop runs and those
/// it does not.
fn report_hook_support(support: &HookSupport) {
    if support.runnable == 0 && !support.has_unsupported() {
        return;
    }
    let names = |names: &BTreeSet<String>| {
        if names.is_empty() {
            "none".to_owned()
        } else {
            safe_line(&names.iter().cloned().collect::<Vec<_>>().join(", "))
        }
    };
    println!(
        "Hooks: {} supported | events: {} | types: {} | fields: {}",
        support.runnable,
        names(&support.events),
        names(&support.types),
        names(&support.fields)
    );
    if support.has_unsupported() {
        println!(
            "Hooks not supported yet: events: {} | types: {} | fields: {}",
            names(&support.unsupported_events),
            names(&support.unsupported_types),
            names(&support.unsupported_fields)
        );
    }
}

fn component_labels(components: &[Component]) -> String {
    components
        .iter()
        .map(|component| component.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn uninstall(
    store: &PluginStore,
    kind: PluginKind,
    name: &str,
    keep_data: bool,
) -> Result<(), String> {
    let uninstalled = store
        .uninstall(name, keep_data)
        .map_err(|error| failure(name, "uninstall", error))?;
    println!(
        "Uninstalled {} {}@{} {}.",
        kind.noun(),
        uninstalled.name,
        uninstalled.marketplace,
        uninstalled.version
    );
    let id = format!("{}@{}", uninstalled.name, uninstalled.marketplace);
    match tickloop_code_settings::UserSettings::in_root(store.config_root())
        .forget_plugin_hooks(&id)
    {
        Ok(true) => println!("Its hook trust records were removed."),
        Ok(false) => {}
        Err(error) => eprintln!(
            "Warning: its hook trust records were not removed: {}",
            safe_line(&error.to_string())
        ),
    }
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
    let kind = PluginKind::of_installed(&plugin);
    let (label, noun) = (kind.label(), kind.noun());
    match (was_enabled == enabled, enabled) {
        (true, true) => println!("{label} {id} is already enabled."),
        (true, false) => println!("{label} {id} is already disabled."),
        (false, true) => println!("Enabled {noun} {id}; its Skills load from the next request."),
        (false, false) => {
            println!("Disabled {noun} {id}; its Skills stop loading from the next request.")
        }
    }
    if enabled {
        super::plugin_config::configure(store, &plugin, false)?;
        super::plugin_mcp::installed(&plugin);
    } else if !plugin.manifest.mcp_servers.is_empty() {
        println!("Its MCP servers stop from the next Agent request.");
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

fn list(store: &PluginStore, kind: PluginKind) -> Result<(), String> {
    let InstalledPlugins { plugins, broken } =
        store.installed().map_err(|error| error.to_string())?;
    let enabled_label = |enabled: bool| if enabled { "enabled" } else { "disabled" };
    let mut rows = plugins
        .iter()
        .filter(|plugin| PluginKind::of_installed(plugin) == kind)
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
                .chain(hooks_to_confirm(store, plugin))
                .collect::<Vec<_>>();
            [
                plugin.id(),
                plugin.version.clone(),
                enabled_label(plugin.enabled).to_owned(),
                login_label(store.login_state(plugin)).to_owned(),
                notes.join("; "),
            ]
        })
        .chain(
            broken
                .iter()
                .filter(|plugin| kind.lists_broken(plugin))
                .map(|plugin| {
                    [
                        format!("{}@{}", plugin.name, plugin.marketplace),
                        plugin.version.clone(),
                        enabled_label(plugin.enabled).to_owned(),
                        login_label(LoginState::Unknown).to_owned(),
                        format!("unusable: {}; install it again", safe_line(&plugin.reason)),
                    ]
                }),
        )
        .collect::<Vec<_>>();
    if rows.is_empty() {
        println!("No {} are installed.", kind.plural());
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
    let commands = tick_plugins::discover_commands(&root, &manifest);
    let mut registry = tickloop_code_settings::skills::SkillRegistry::default();
    for command in commands.commands {
        registry.register_command(
            tickloop_code_settings::skills::CommandDocument {
                name: command.name,
                markdown: command.markdown,
                source: tickloop_harness::protocol::SkillSource::Plugin {
                    plugin: manifest.name.clone(),
                    path: command.path,
                },
                base_directory: command.base_directory,
                metadata: command.metadata,
                public_config: Default::default(),
            },
            &[],
        );
    }
    let names = registry
        .definitions()
        .map(|command| safe_line(&command.name))
        .collect::<Vec<_>>();
    println!(
        "Commands: {}",
        if names.is_empty() {
            "none".into()
        } else {
            names.join(", ")
        }
    );
    for warning in commands.warnings.iter().chain(registry.warnings()) {
        println!("Warning: {}", safe_line(warning));
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
    report_unsupported(&manifest);
    report_hook_support(&manifest.hook_support);
    report_warnings(&manifest);
    super::plugin_mcp::validate(&manifest, &root);
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
