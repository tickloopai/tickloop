//! The Claude Code `command` hooks a Folder has, from three sources: the user's own
//! `settings.json` (its top level and the Folder's partition), the enabled plugins, and the
//! Folder's `.claude/settings.json` and `.claude/settings.local.json`, of which only `hooks` is
//! read. A plugin or repository hook runs only once the user trusts its hash; the trust records
//! live in the user's `settings.json` under `hookTrust`: the user's and the plugins' (by plugin ID)
//! at the top level, the repository's in the Folder's partition.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tick_plugins::{
    CLAUDE_MANIFEST, COMMAND_HOOK_TYPE, InstalledPlugin, ManifestFormat, PluginHooks, PluginStore,
    SHIPPED_FILES_RECORD, TICKLOOP_MANIFEST, UNSUPPORTED_COMMAND_FIELDS,
};
use tickloop_harness::policy::settings::SettingsStoreError;
use tickloop_harness::policy::skills::{PLUGIN_DATA_VARIABLES, PLUGIN_ROOT_VARIABLES};
use tickloop_harness::protocol::hooks::{
    ConfiguredHook, HookBackground, HookEvent, HookList, HookPluginDirectories, HookSource,
    HookTrust, HookTrustAction, HookTrustChange,
};
use tickloop_harness::tools::permissions::is_supported_rule_text;

use crate::{
    InvalidEntry, PROJECTS_KEY, SettingsError, UserSettings, object_entry,
    plugin_config::PluginUserConfig, project_key,
};

const HOOKS_KEY: &str = "hooks";
const HOOK_TRUST_KEY: &str = "hookTrust";
const USER_RECORDS_KEY: &str = "user";
const PLUGIN_RECORDS_KEY: &str = "plugins";
const TRUSTED_HASH_KEY: &str = "trustedHash";
const DISABLED_KEY: &str = "disabled";
const REPOSITORY_SETTINGS_FILES: [&str; 2] =
    [".claude/settings.json", ".claude/settings.local.json"];
/// The scripts a repository's trust covers besides the hook declarations.
const REPOSITORY_HOOKS_DIRECTORY: &str = ".claude/hooks";
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const FINGERPRINT_BUFFER_BYTES: usize = 64 * 1024;
/// Changes with the fingerprint's encoding, so trust confirmed under an earlier one lapses.
const FINGERPRINT_VERSION: &[u8] = b"tickloop-hook-content-v3";
/// Bounds a fingerprint, such as of a symbolic link to a large directory elsewhere.
const MAX_FINGERPRINT_ENTRIES: usize = 100_000;
/// What a symbolic link counts as while its target cannot be reached or opened.
const UNREACHABLE_TARGET: &[u8] = b"unreachable";
const MAX_FOLLOWED_LINKS: usize = 40;

/// A hook as declared, before its source decides its hash and trust.
struct DeclaredHook {
    file: PathBuf,
    event: HookEvent,
    matcher: Option<String>,
    command: String,
    timeout_secs: u64,
    condition: Option<String>,
    background: Option<HookBackground>,
}

impl DeclaredHook {
    /// The hook's identity within its source, from its normalized declaration. A hook without
    /// `if`, `async` or `asyncRewake` keeps the identity it had before they were read.
    fn key(&self) -> String {
        let common = (
            self.event.name(),
            &self.matcher,
            COMMAND_HOOK_TYPE,
            &self.command,
            self.timeout_secs,
        );
        let declaration = match (&self.condition, &self.background) {
            (None, None) => serde_json::to_vec(&common),
            (Some(condition), None) => serde_json::to_vec(&(common, condition)),
            (condition, Some(background)) => serde_json::to_vec(&(common, condition, background)),
        }
        .expect("hook declarations serialize");
        hex_digest(&declaration)
    }
}

/// A supported `command` hook's own fields.
struct CommandHandler {
    command: String,
    timeout_secs: u64,
    condition: Option<String>,
    background: Option<HookBackground>,
}

/// What makes two hooks of one source the same: the event, matcher, command, `if` and way of
/// running.
type SameHook = (
    HookEvent,
    Option<String>,
    String,
    Option<String>,
    Option<HookBackground>,
);

/// The hooks of one source in declaration order, each [`SameHook`] once.
#[derive(Default)]
struct SourceHooks {
    hooks: Vec<DeclaredHook>,
    seen: HashSet<SameHook>,
}

impl SourceHooks {
    fn read(&mut self, declaration: &Value, file: &Path, warnings: &mut Vec<String>) {
        for hook in declared_hooks(declaration, file, warnings) {
            if self.seen.insert((
                hook.event,
                hook.matcher.clone(),
                hook.command.clone(),
                hook.condition.clone(),
                hook.background.clone(),
            )) {
                self.hooks.push(hook);
            }
        }
    }
}

pub(crate) fn configured_hooks(settings: &UserSettings, folder: Option<&Path>) -> HookList {
    let mut warnings = Vec::new();
    let (saved, revision) = match settings.read_object() {
        Ok(saved) => {
            let revision = trust_revision(&saved, folder);
            (saved, revision)
        }
        Err(error) => {
            warnings.push(format!("{error}; user hooks and hook trust are ignored"));
            (Map::new(), String::new())
        }
    };
    let records = TrustRecords::read(&saved, folder, settings.file_path(), &mut warnings);
    let mut hooks = Vec::new();

    let mut user = SourceHooks::default();
    let folder_partition = folder.and_then(|folder| project_partition(&saved, folder));
    for declaration in [
        saved.get(HOOKS_KEY),
        folder_partition.and_then(|partition| partition.get(HOOKS_KEY)),
    ]
    .into_iter()
    .flatten()
    {
        user.read(declaration, settings.file_path(), &mut warnings);
    }
    for hook in user.hooks {
        let key = hook.key();
        let trust = trust_state(TrustRecords::record(records.user, &key), None);
        hooks.push(configured(hook, key, HookSource::User, None, None, trust));
    }

    if let Some(folder) = folder {
        hooks.extend(repository_hooks(folder, &records, &mut warnings));
    }
    if let Some(root) = settings.root() {
        hooks.extend(plugin_hooks(root, &records, &mut warnings));
    }
    HookList {
        hooks,
        warnings,
        revision,
    }
}

fn repository_hooks(
    folder: &Path,
    records: &TrustRecords<'_>,
    warnings: &mut Vec<String>,
) -> Vec<ConfiguredHook> {
    let mut repository = SourceHooks::default();
    for file in REPOSITORY_SETTINGS_FILES.map(|file| folder.join(file)) {
        let document = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!(
                    "{}: cannot read its hooks: {error}",
                    file.display()
                ));
                continue;
            }
        };
        match document {
            Ok(document) => {
                if let Some(declaration) = document.get(HOOKS_KEY) {
                    repository.read(declaration, &file, warnings);
                }
            }
            Err(_) => warnings.push(format!(
                "{}: is not valid JSON; its hooks are skipped",
                file.display()
            )),
        }
    }
    if repository.hooks.is_empty() {
        return Vec::new();
    }
    let scripts = folder.join(REPOSITORY_HOOKS_DIRECTORY);
    let fingerprint = match directory_fingerprint(&scripts) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            warnings.push(format!(
                "the Folder's hooks are skipped: cannot read {}: {error}",
                scripts.display()
            ));
            return Vec::new();
        }
    };
    repository
        .hooks
        .into_iter()
        .map(|hook| {
            let key = hook.key();
            let hash = format!("{key}/{fingerprint}");
            let record = TrustRecords::record(records.repository, &key);
            let trust = trust_state(record, Some(&hash));
            configured(hook, key, HookSource::Repository, None, Some(hash), trust)
        })
        .collect()
}

fn plugin_hooks(
    config_root: &Path,
    records: &TrustRecords<'_>,
    warnings: &mut Vec<String>,
) -> Vec<ConfiguredHook> {
    let store = PluginStore::in_config_root(config_root);
    let installed = match store.installed() {
        Ok(installed) => installed,
        Err(error) => {
            warnings.push(format!(
                "the hooks of installed plugins are not loaded: {error}"
            ));
            return Vec::new();
        }
    };
    for broken in installed.broken.iter().filter(|broken| broken.enabled) {
        warnings.push(format!(
            "the hooks of plugin {} are not loaded: it cannot be used ({}); install it again",
            broken.name, broken.reason
        ));
    }
    let mut hooks = Vec::new();
    for plugin in installed.plugins.iter().filter(|plugin| plugin.enabled) {
        let id = plugin.id();
        let plugin_records = TrustRecords::record(records.plugins, &id);
        let declared = plugin_hooks_with_trust(plugin, plugin_records, warnings);
        if declared.is_empty() {
            continue;
        }
        let user_config =
            PluginUserConfig::load(config_root, &plugin.manifest).unwrap_or_else(|error| {
                warnings.push(format!("plugin {id}: {error}"));
                PluginUserConfig::defaults(&plugin.manifest)
            });
        let data = store
            .create_data_directory(plugin.name())
            .unwrap_or_else(|error| {
                warnings.push(format!(
                    "the data directory of plugin {id} cannot be created: {error}"
                ));
                store.data_directory(plugin.name())
            });
        let mut environment = user_config.variables();
        for (names, directory) in [
            (PLUGIN_ROOT_VARIABLES, &plugin.root),
            (PLUGIN_DATA_VARIABLES, &data),
        ] {
            for name in names {
                let name = name.trim_start_matches("${").trim_end_matches('}');
                environment.insert(name.into(), directory.to_string_lossy().into_owned());
            }
        }
        let directories = HookPluginDirectories {
            root: plugin.root.clone(),
            data: data.clone(),
        };
        for PluginHook {
            hook,
            key,
            hash,
            trust,
        } in declared
        {
            let resolved = resolve_plugin_command(&hook.command, &environment);
            let sensitive_values =
                user_config.secret_texts_for(&Value::String(hook.command.clone()));
            let mut hook = configured(
                hook,
                key,
                HookSource::Plugin { id: id.clone() },
                Some(directories.clone()),
                Some(hash),
                trust,
            );
            hook.resolved_command = resolved;
            hook.sensitive_values = sensitive_values;
            hooks.push(hook);
        }
    }
    hooks
}

/// A plugin's declared hook with what its trust is judged by.
struct PluginHook {
    hook: DeclaredHook,
    key: String,
    hash: String,
    trust: HookTrust,
}

/// The hooks `plugin` declares with their trust by its `records`; none when its files cannot be
/// fingerprinted.
fn plugin_hooks_with_trust(
    plugin: &InstalledPlugin,
    records: Option<&Map<String, Value>>,
    warnings: &mut Vec<String>,
) -> Vec<PluginHook> {
    let mut declared = SourceHooks::default();
    for declaration in &plugin.manifest.hooks {
        read_plugin_hooks(plugin, declaration, &mut declared, warnings);
    }
    if declared.hooks.is_empty() {
        return Vec::new();
    }
    let fingerprint = match plugin_fingerprint(plugin) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            warnings.push(format!(
                "the hooks of plugin {} are skipped: cannot read its files: {error}",
                plugin.id()
            ));
            return Vec::new();
        }
    };
    declared
        .hooks
        .into_iter()
        .map(|hook| {
            let key = hook.key();
            let hash = format!("{key}/{fingerprint}");
            let trust = trust_state(TrustRecords::record(records, &key), Some(&hash));
            PluginHook {
                hook,
                key,
                hash,
                trust,
            }
        })
        .collect()
}

fn read_plugin_hooks(
    plugin: &InstalledPlugin,
    declaration: &PluginHooks,
    declared: &mut SourceHooks,
    warnings: &mut Vec<String>,
) {
    match declaration {
        PluginHooks::Inline(hooks) => {
            let manifest = match plugin.manifest.format {
                ManifestFormat::Tickloop => plugin.root.join(TICKLOOP_MANIFEST),
                ManifestFormat::Claude => plugin.root.join(CLAUDE_MANIFEST),
                _ => plugin.root.clone(),
            };
            declared.read(hooks, &manifest, warnings);
        }
        PluginHooks::File(relative) => {
            let file = plugin.root.join(relative);
            let document = fs::read(&file)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<Value>(&bytes)
                        .map_err(|_| "it is not valid JSON".to_owned())
                });
            match document {
                Ok(document) => {
                    if let Some(hooks) = document.get(HOOKS_KEY) {
                        declared.read(hooks, &file, warnings);
                    }
                }
                Err(reason) => warnings.push(format!(
                    "{}: its hooks are skipped: {reason}",
                    file.display()
                )),
            }
        }
    }
}

/// The plugin's placeholders substituted, or `None` while a user configuration value it needs
/// is not set. Other `${...}` stay for the shell.
fn resolve_plugin_command(command: &str, environment: &BTreeMap<String, String>) -> Option<String> {
    if crate::mcp::missing_user_config(&Value::String(command.into()), environment) {
        return None;
    }
    Some(crate::mcp::expand_known_variables(command, environment))
}

fn configured(
    hook: DeclaredHook,
    key: String,
    source: HookSource,
    plugin: Option<HookPluginDirectories>,
    hash: Option<String>,
    trust: HookTrust,
) -> ConfiguredHook {
    ConfiguredHook {
        key,
        source,
        file: hook.file,
        event: hook.event,
        matcher: hook.matcher,
        resolved_command: Some(hook.command.clone()),
        sensitive_values: Vec::new(),
        command: hook.command,
        condition: hook.condition,
        timeout_secs: hook.timeout_secs,
        background: hook.background,
        plugin,
        hash,
        trust,
    }
}

/// The `command` hooks of a Claude Code `hooks` object, each other entry skipped with a warning.
fn declared_hooks(
    declaration: &Value,
    file: &Path,
    warnings: &mut Vec<String>,
) -> Vec<DeclaredHook> {
    let location = file.display();
    let Some(events) = declaration.as_object() else {
        warnings.push(format!(
            "{location}: hooks must be an object of events; they are skipped"
        ));
        return Vec::new();
    };
    let mut hooks = Vec::new();
    for (name, groups) in events {
        let Some(event) = HookEvent::from_name(name) else {
            warnings.push(format!(
                "{location}: hook event {name} is not supported; its hooks are skipped"
            ));
            continue;
        };
        let Some(groups) = groups.as_array() else {
            warnings.push(format!(
                "{location}: {name} hooks must be a list of matcher groups; they are skipped"
            ));
            continue;
        };
        for group in groups {
            let matcher = match group.get("matcher") {
                None | Some(Value::Null) => None,
                Some(Value::String(matcher)) => Some(matcher.clone()),
                Some(_) => {
                    warnings.push(format!(
                        "{location}: {name} matcher must be text; its hooks are skipped"
                    ));
                    continue;
                }
            };
            let matcher = matcher.filter(|matcher| event.uses_matcher() && !matcher.is_empty());
            let Some(handlers) = group.get(HOOKS_KEY).and_then(Value::as_array) else {
                warnings.push(format!(
                    "{location}: {name} matcher group needs a hooks list; it is skipped"
                ));
                continue;
            };
            for handler in handlers {
                match command_hook(handler) {
                    Ok(CommandHandler {
                        command,
                        timeout_secs,
                        condition,
                        background,
                    }) => {
                        if let Some(condition) = &condition {
                            if !event.is_tool_event() {
                                warnings.push(format!(
                                    "{location}: {name} hook if applies only to tool events; \
                                     the hook never runs"
                                ));
                            } else if !is_supported_rule_text(condition) {
                                warnings.push(format!(
                                    "{location}: {name} hook if {condition} cannot be matched; \
                                     the hook runs for every matching call"
                                ));
                            }
                        }
                        hooks.push(DeclaredHook {
                            file: file.to_path_buf(),
                            event,
                            matcher: matcher.clone(),
                            command,
                            timeout_secs,
                            condition,
                            background,
                        })
                    }
                    Err(problem) => warnings.push(format!("{location}: {name} hook {problem}")),
                }
            }
        }
    }
    hooks
}

/// The fields of a supported `command` hook, or why it is skipped.
fn command_hook(handler: &Value) -> Result<CommandHandler, String> {
    let kind = handler.get("type").and_then(Value::as_str);
    match kind {
        Some(COMMAND_HOOK_TYPE) => {}
        Some(kind) => return Err(format!("type {kind} is not supported; it is skipped")),
        None => return Err("needs a type; it is skipped".into()),
    }
    if let Some(field) = UNSUPPORTED_COMMAND_FIELDS
        .into_iter()
        .find(|field| handler.get(field).is_some())
    {
        return Err(format!(
            "field {field} is not supported; the hook is skipped"
        ));
    }
    let command = handler
        .get(COMMAND_HOOK_TYPE)
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .ok_or("needs a command; it is skipped")?;
    let timeout_secs = match handler.get("timeout") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(timeout) => timeout
            .as_f64()
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| seconds.ceil() as u64)
            .ok_or("timeout must be a positive number of seconds; it is skipped")?,
    };
    let condition = match handler.get("if") {
        None | Some(Value::Null) => None,
        Some(Value::String(condition)) => {
            Some(condition.trim().to_owned()).filter(|condition| !condition.is_empty())
        }
        Some(_) => return Err("if must be text such as Bash(git push*); it is skipped".into()),
    };
    Ok(CommandHandler {
        command: command.to_owned(),
        timeout_secs,
        condition,
        background: background(handler)?,
    })
}

/// How Claude Code's `async` and `asyncRewake` run a hook; `asyncRewake` implies `async`.
fn background(handler: &Value) -> Result<Option<HookBackground>, String> {
    let flag = |field: &str| match handler.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(set)) => Ok(*set),
        Some(_) => Err(format!("{field} must be true or false; it is skipped")),
    };
    let rewake_text = |field: &str| match handler.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(_) => Err(format!("{field} must be non-empty text; it is skipped")),
    };
    if flag("asyncRewake")? {
        return Ok(Some(HookBackground::AsyncRewake {
            message: rewake_text("rewakeMessage")?,
            summary: rewake_text("rewakeSummary")?,
        }));
    }
    Ok(flag("async")?.then_some(HookBackground::Async))
}

fn trust_state(record: Option<&Map<String, Value>>, hash: Option<&str>) -> HookTrust {
    let field = |name| record.and_then(|record| record.get(name));
    if field(DISABLED_KEY).and_then(Value::as_bool) == Some(true) {
        return HookTrust::Disabled;
    }
    let Some(hash) = hash else {
        return HookTrust::Trusted;
    };
    match field(TRUSTED_HASH_KEY).and_then(Value::as_str) {
        Some(trusted) if trusted == hash => HookTrust::Trusted,
        Some(_) => HookTrust::Modified,
        None => HookTrust::Pending,
    }
}

/// The trust records that apply to one Folder; a partition of the wrong shape counts as empty.
struct TrustRecords<'a> {
    user: Option<&'a Map<String, Value>>,
    plugins: Option<&'a Map<String, Value>>,
    repository: Option<&'a Map<String, Value>>,
}

impl<'a> TrustRecords<'a> {
    fn read(
        saved: &'a Map<String, Value>,
        folder: Option<&Path>,
        settings_file: &Path,
        warnings: &mut Vec<String>,
    ) -> Self {
        let mut partition = |value: Option<&'a Value>, location: String| match value {
            None => None,
            Some(Value::Object(records)) => Some(records),
            Some(_) => {
                warnings.push(format!(
                    "settings file {}: {location} must be an object; its hook trust records are ignored",
                    settings_file.display()
                ));
                None
            }
        };
        let global = partition(saved.get(HOOK_TRUST_KEY), HOOK_TRUST_KEY.into());
        let user = partition(
            global.and_then(|global| global.get(USER_RECORDS_KEY)),
            format!("{HOOK_TRUST_KEY}.{USER_RECORDS_KEY}"),
        );
        let plugins = partition(
            global.and_then(|global| global.get(PLUGIN_RECORDS_KEY)),
            format!("{HOOK_TRUST_KEY}.{PLUGIN_RECORDS_KEY}"),
        );
        let repository = folder.and_then(|folder| {
            partition(
                project_partition(saved, folder).and_then(|project| project.get(HOOK_TRUST_KEY)),
                format!("{PROJECTS_KEY}[{:?}].{HOOK_TRUST_KEY}", project_key(folder)),
            )
        });
        Self {
            user,
            plugins,
            repository,
        }
    }

    fn record(
        partition: Option<&'a Map<String, Value>>,
        key: &str,
    ) -> Option<&'a Map<String, Value>> {
        partition?.get(key)?.as_object()
    }
}

fn project_partition<'a>(
    saved: &'a Map<String, Value>,
    folder: &Path,
) -> Option<&'a Map<String, Value>> {
    saved
        .get(PROJECTS_KEY)?
        .get(project_key(folder))?
        .as_object()
}

/// Changes only with the trust records that apply to `folder`.
fn trust_revision(saved: &Map<String, Value>, folder: Option<&Path>) -> String {
    let repository = folder
        .and_then(|folder| project_partition(saved, folder))
        .and_then(|project| project.get(HOOK_TRUST_KEY));
    hex_digest(
        &serde_json::to_vec(&(saved.get(HOOK_TRUST_KEY), repository))
            .expect("settings values serialize"),
    )
}

pub(crate) fn write_trust(
    settings: &UserSettings,
    folder: Option<&Path>,
    changes: &[HookTrustChange],
    expected_revision: Option<&str>,
) -> Result<(), SettingsStoreError> {
    for change in changes {
        match (&change.source, &change.action) {
            (HookSource::User, HookTrustAction::Trust { .. }) => {
                return Err(SettingsStoreError::Storage(
                    "user hooks run without trust; only plugin and repository hooks are trusted"
                        .into(),
                ));
            }
            (HookSource::Repository, _) if folder.is_none() => {
                return Err(SettingsStoreError::Storage(
                    "repository hooks belong to a Folder".into(),
                ));
            }
            _ => {}
        }
    }
    settings
        .update_checked(|saved| {
            if expected_revision.is_some_and(|expected| trust_revision(saved, folder) != expected) {
                return Err(SettingsError::Conflict);
            }
            apply_trust_changes(saved, folder, changes).map_err(|entry| {
                SettingsError::InvalidEntry {
                    path: settings.file_path().to_path_buf(),
                    key: entry.key,
                    expected: entry.expected,
                }
            })
        })
        .map_err(Into::into)
}

fn apply_trust_changes(
    saved: &mut Map<String, Value>,
    folder: Option<&Path>,
    changes: &[HookTrustChange],
) -> Result<(), InvalidEntry> {
    for change in changes {
        let records = trust_partition(saved, folder, &change.source)?;
        let record = object_entry(records, &change.key, format!("hook record {}", change.key))?;
        match &change.action {
            HookTrustAction::Trust { hash } => {
                record.insert(TRUSTED_HASH_KEY.into(), hash.clone().into());
            }
            HookTrustAction::Disable => {
                record.insert(DISABLED_KEY.into(), true.into());
            }
            HookTrustAction::Enable => {
                record.remove(DISABLED_KEY);
            }
        }
        if record.is_empty() {
            records.remove(&change.key);
        }
    }
    Ok(())
}

fn trust_partition<'a>(
    saved: &'a mut Map<String, Value>,
    folder: Option<&Path>,
    source: &HookSource,
) -> Result<&'a mut Map<String, Value>, InvalidEntry> {
    match (source, folder) {
        (HookSource::Repository, Some(folder)) => {
            let key = project_key(folder);
            let projects = object_entry(saved, PROJECTS_KEY, PROJECTS_KEY.into())?;
            let location = format!("{PROJECTS_KEY}[{key:?}]");
            let project = object_entry(projects, &key, location.clone())?;
            object_entry(
                project,
                HOOK_TRUST_KEY,
                format!("{location}.{HOOK_TRUST_KEY}"),
            )
        }
        (HookSource::Repository, None) => unreachable!("checked before writing"),
        (HookSource::User, _) => {
            let global = object_entry(saved, HOOK_TRUST_KEY, HOOK_TRUST_KEY.into())?;
            object_entry(
                global,
                USER_RECORDS_KEY,
                format!("{HOOK_TRUST_KEY}.{USER_RECORDS_KEY}"),
            )
        }
        (HookSource::Plugin { id }, _) => {
            let global = object_entry(saved, HOOK_TRUST_KEY, HOOK_TRUST_KEY.into())?;
            let location = format!("{HOOK_TRUST_KEY}.{PLUGIN_RECORDS_KEY}");
            let plugins = object_entry(global, PLUGIN_RECORDS_KEY, location.clone())?;
            object_entry(plugins, id, format!("{location}[{id:?}]"))
        }
    }
}

impl UserSettings {
    /// How many hooks of `plugin` wait for the user to confirm them: never confirmed, or changed
    /// since. Hooks the user turned off do not count, and neither does whether the plugin is
    /// enabled.
    pub fn plugin_hooks_to_confirm(&self, plugin: &InstalledPlugin) -> usize {
        let saved = self.read_object().unwrap_or_default();
        let records = TrustRecords::read(&saved, None, self.file_path(), &mut Vec::new());
        let plugin_records = TrustRecords::record(records.plugins, &plugin.id());
        plugin_hooks_with_trust(plugin, plugin_records, &mut Vec::new())
            .iter()
            .filter(|hook| matches!(hook.trust, HookTrust::Pending | HookTrust::Modified))
            .count()
    }

    /// Removes the hook trust records of the plugin `id` (`<plugin>@<marketplace>`), as when it is
    /// uninstalled, and tells whether it had any; the file is left alone when it has none.
    pub fn forget_plugin_hooks(&self, id: &str) -> Result<bool, SettingsError> {
        let has_records = |saved: &Map<String, Value>| {
            saved
                .get(HOOK_TRUST_KEY)
                .and_then(|global| global.get(PLUGIN_RECORDS_KEY))
                .and_then(|plugins| plugins.get(id))
                .is_some()
        };
        if !has_records(&self.read_object()?) {
            return Ok(false);
        }
        self.update(|saved| {
            if let Some(plugins) = saved
                .get_mut(HOOK_TRUST_KEY)
                .and_then(|global| global.get_mut(PLUGIN_RECORDS_KEY))
                .and_then(Value::as_object_mut)
            {
                plugins.remove(id);
            }
            Ok(())
        })?;
        Ok(true)
    }
}

/// A plugin's fingerprint: what it shipped with, by the record its install kept, so files its
/// programs create later (such as Python bytecode) leave it unchanged; a plugin without a
/// readable record is covered whole.
fn plugin_fingerprint(plugin: &InstalledPlugin) -> io::Result<String> {
    let mut fingerprint = Fingerprint::new();
    match plugin.shipped_files().ok().flatten() {
        Some(shipped) => {
            let record = PathBuf::from(SHIPPED_FILES_RECORD);
            for relative in shipped.iter().chain([&record]) {
                fingerprint.entry(&plugin.root.join(relative), relative, &mut Vec::new(), 0)?;
            }
        }
        None => fingerprint.entry(&plugin.root, Path::new(""), &mut Vec::new(), 0)?,
    }
    Ok(fingerprint.finish())
}

/// The fingerprint of everything under `directory`; a missing one counts as empty.
fn directory_fingerprint(directory: &Path) -> io::Result<String> {
    let mut fingerprint = Fingerprint::new();
    fingerprint.entry(directory, Path::new(""), &mut Vec::new(), 0)?;
    Ok(fingerprint.finish())
}

/// A digest of files by their full relative paths and bytes, each directory with its number of
/// entries, so no two trees encode alike. Symbolic links count with their target and the content
/// behind it. A link whose target cannot be reached or opened, that leads too far or back into a
/// directory being read counts as a fixed marker, so it changes once it leads somewhere; any
/// other failure behind a link fails the fingerprint, as it does in a directory of its own.
struct Fingerprint {
    hash: Sha256,
    buffer: Vec<u8>,
    entries: usize,
    entry_limit: usize,
}

/// A directory opened for reading, by its canonical path.
struct OpenDirectory {
    canonical: PathBuf,
    listing: fs::ReadDir,
}

fn open_directory(path: &Path) -> io::Result<OpenDirectory> {
    Ok(OpenDirectory {
        canonical: fs::canonicalize(path)?,
        listing: fs::read_dir(path)?,
    })
}

impl Fingerprint {
    fn new() -> Self {
        Self::with_entry_limit(MAX_FINGERPRINT_ENTRIES)
    }

    fn with_entry_limit(entry_limit: usize) -> Self {
        let mut hash = Sha256::new();
        hash.update(FINGERPRINT_VERSION);
        Self {
            hash,
            buffer: vec![0; FINGERPRINT_BUFFER_BYTES],
            entries: 0,
            entry_limit,
        }
    }

    fn finish(self) -> String {
        format!("sha256:{:x}", self.hash.finalize())
    }

    fn named(&mut self, name: &Path) {
        self.bytes(name.as_os_str().as_encoded_bytes());
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.hash.update((bytes.len() as u64).to_le_bytes());
        self.hash.update(bytes);
    }

    /// Hashes `path`, named `relative` from where the fingerprint starts. `ancestors` holds the
    /// directories being read, canonical; `links` counts the symbolic links followed to reach
    /// `path`.
    fn entry(
        &mut self,
        path: &Path,
        relative: &Path,
        ancestors: &mut Vec<PathBuf>,
        links: usize,
    ) -> io::Result<()> {
        self.entries += 1;
        if self.entries > self.entry_limit {
            return Err(io::Error::other(format!(
                "more than {} entries to read",
                self.entry_limit
            )));
        }
        self.named(relative);
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.bytes(b"missing");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if metadata.is_symlink() {
            self.link(path, relative, ancestors, links)
        } else if metadata.is_dir() {
            self.directory(open_directory(path)?, relative, ancestors, links)
        } else if metadata.is_file() {
            self.file(fs::File::open(path)?, path, metadata.len())
        } else {
            Err(io::Error::other(format!(
                "{} is not a file, directory or symlink",
                path.display()
            )))
        }
    }

    fn link(
        &mut self,
        path: &Path,
        relative: &Path,
        ancestors: &mut Vec<PathBuf>,
        links: usize,
    ) -> io::Result<()> {
        self.bytes(b"symlink");
        self.named(&fs::read_link(path)?);
        if links >= MAX_FOLLOWED_LINKS {
            self.bytes(b"too many links");
            return Ok(());
        }
        match fs::metadata(path) {
            Ok(target) if target.is_dir() => match open_directory(path) {
                Ok(directory) => self.directory(directory, relative, ancestors, links + 1),
                Err(_) => {
                    self.bytes(UNREACHABLE_TARGET);
                    Ok(())
                }
            },
            Ok(target) if target.is_file() => match fs::File::open(path) {
                Ok(file) => self.file(file, path, target.len()),
                Err(_) => {
                    self.bytes(UNREACHABLE_TARGET);
                    Ok(())
                }
            },
            Ok(_) => {
                self.bytes(b"special");
                Ok(())
            }
            Err(_) => {
                self.bytes(UNREACHABLE_TARGET);
                Ok(())
            }
        }
    }

    fn directory(
        &mut self,
        directory: OpenDirectory,
        relative: &Path,
        ancestors: &mut Vec<PathBuf>,
        links: usize,
    ) -> io::Result<()> {
        if ancestors.contains(&directory.canonical) {
            self.bytes(b"cycle");
            return Ok(());
        }
        let mut entries = directory.listing.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        self.bytes(b"directory");
        self.hash.update((entries.len() as u64).to_le_bytes());
        ancestors.push(directory.canonical);
        let result = entries.iter().try_for_each(|entry| {
            self.entry(
                &entry.path(),
                &relative.join(entry.file_name()),
                ancestors,
                links,
            )
        });
        ancestors.pop();
        result
    }

    fn file(&mut self, file: fs::File, path: &Path, length: u64) -> io::Result<()> {
        // Reads at most one byte past the observed size, to tell a file that changes while it is
        // read from a fingerprint of part of it.
        let mut file = file.take(length.saturating_add(1));
        self.bytes(b"file");
        self.hash.update(length.to_le_bytes());
        let mut read = 0_u64;
        loop {
            let count = file.read(&mut self.buffer)?;
            if count == 0 {
                break;
            }
            read += count as u64;
            self.hash.update(&self.buffer[..count]);
        }
        if read != length {
            return Err(io::Error::other(format!(
                "{} changed while it was read",
                path.display()
            )));
        }
        Ok(())
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_limit_reached_behind_a_link_fails_the_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let scripts = directory.path().join("hooks");
        let elsewhere = directory.path().join("elsewhere");
        fs::create_dir_all(&scripts).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::write(elsewhere.join(name), name).unwrap();
        }
        std::os::unix::fs::symlink(&elsewhere, scripts.join("z")).unwrap();

        let mut fingerprint = Fingerprint::with_entry_limit(4);
        let error = fingerprint
            .entry(&scripts, Path::new(""), &mut Vec::new(), 0)
            .unwrap_err();
        assert!(error.to_string().contains("more than 4 entries"), "{error}");
    }
}
