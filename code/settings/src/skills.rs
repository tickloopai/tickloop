use crate::loop_skill::LoopFiles;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tick_plugins::{InstalledPlugin, PluginStore};
pub use tickloop_harness::policy::skills::*;
use tickloop_harness::protocol::SkillSource;
const SKILL_FILE_NAME: &str = "SKILL.md";
const SKILLS_DIRECTORY: &str = "skills";
const PROJECT_SKILLS_DIRECTORY: &str = ".tickloop/skills";
const INCOMPLETE_DISCOVERY_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

pub trait SkillDiscovery {
    fn discover(folder: &Path, config_root: Option<&Path>) -> Self;
    fn discover_with_builtins<'a>(
        folder: &Path,
        config_root: Option<&Path>,
        builtins: impl IntoIterator<Item = &'a str>,
    ) -> Self;
}
impl SkillDiscovery for SkillRegistry {
    fn discover(folder: &Path, config_root: Option<&Path>) -> Self {
        Self::discover_with_builtins(folder, config_root, BUILTIN_SKILLS)
    }
    fn discover_with_builtins<'a>(
        folder: &Path,
        config_root: Option<&Path>,
        builtins: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        discover_tracked(folder, config_root, builtins).0
    }
}

/// Cached disk discovery. Call from a background worker: unchanged sources require only metadata.
#[derive(Default)]
pub struct SkillDiscoveryCache {
    context: Option<(PathBuf, Option<PathBuf>)>,
    retry_at: Option<std::time::Instant>,
    watched: Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>,
}

impl SkillDiscoveryCache {
    pub fn refresh(&mut self, folder: &Path, config_root: Option<&Path>) -> Option<SkillRegistry> {
        let context = (folder.to_path_buf(), config_root.map(Path::to_path_buf));
        if self.context.as_ref() == Some(&context)
            && self
                .retry_at
                .is_none_or(|retry| std::time::Instant::now() < retry)
            && self
                .watched
                .iter()
                .all(|(path, previous)| &file_stamp(path) == previous)
        {
            return None;
        }
        let (registry, paths) = discover_tracked(folder, config_root, BUILTIN_SKILLS);
        self.watched = paths
            .into_iter()
            .map(|path| {
                let stamp = file_stamp(&path);
                (path, stamp)
            })
            .collect();
        // Invalid manifests may hide their own input paths; retry incomplete discovery so repairs
        // become visible even when the installation registry did not change.
        self.retry_at = registry
            .needs_discovery_retry()
            .then(|| std::time::Instant::now() + INCOMPLETE_DISCOVERY_RETRY);
        self.context = Some(context);
        Some(registry)
    }
}

fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn discover_tracked<'a>(
    folder: &Path,
    config_root: Option<&Path>,
    builtins: impl IntoIterator<Item = &'a str>,
) -> (SkillRegistry, Vec<PathBuf>) {
    let mut registry = SkillRegistry::with_builtins(builtins);
    registry.set_loop_instructions(Arc::new(LoopFiles::new(Some(folder), config_root)));
    let mut seen_files = HashSet::new();
    let mut watched = Vec::new();
    if let Some(root) = config_root {
        load_directory(
            &mut registry,
            &root.join(SKILLS_DIRECTORY),
            SkillOrigin::User,
            &mut seen_files,
            &mut watched,
        );
        load_plugins(&mut registry, root, &mut seen_files, &mut watched);
    }
    for directory in project_skill_directories(folder) {
        load_directory(
            &mut registry,
            &directory,
            SkillOrigin::Project,
            &mut seen_files,
            &mut watched,
        );
    }
    (registry, watched)
}

enum SkillOrigin<'a> {
    User,
    Project,
    Plugin(&'a PluginStore, &'a InstalledPlugin),
}

/// Loads the Skills of every enabled plugin; a disabled one stays installed but unloaded.
fn load_plugins(
    registry: &mut SkillRegistry,
    config_root: &Path,
    seen_files: &mut HashSet<PathBuf>,
    watched: &mut Vec<PathBuf>,
) {
    watched.push(config_root.join("plugins/installed.json"));
    watched.push(config_root.join("settings.json"));
    watched.push(config_root.join("ticks/installed.json"));
    let store = PluginStore::in_config_root(config_root);
    let installed = match store.installed() {
        Ok(installed) => installed,
        Err(error) => {
            registry.warn(format!(
                "the Skills of installed plugins are not loaded: {error}"
            ));
            return;
        }
    };
    for broken in installed.broken.iter().filter(|broken| broken.enabled) {
        registry.warn(format!(
            "the Skills of plugin {} are not loaded: it cannot be used ({}); install it again",
            broken.name, broken.reason
        ));
    }
    for plugin in installed.plugins.iter().filter(|plugin| plugin.enabled) {
        for manifest in [
            ".tickloop-plugin/plugin.json",
            ".claude-plugin/plugin.json",
            "tick-package.json",
        ] {
            watched.push(plugin.root.join(manifest));
        }
        load_plugin_commands(registry, &store, plugin, watched);
        for path in &plugin.manifest.skill_paths {
            let directory = plugin.root.join(path);
            watched.push(directory.join(SKILL_FILE_NAME));
            if let Ok(entries) = fs::read_dir(&directory) {
                watched.extend(
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path().join(SKILL_FILE_NAME)),
                );
            }
            watched.push(directory);
        }
        watched.push(plugin.root.join("skills"));
        let skill_files = plugin
            .skill_directories()
            .into_iter()
            .map(|directory| directory.join(SKILL_FILE_NAME))
            .collect();
        load_files(
            registry,
            skill_files,
            &SkillOrigin::Plugin(&store, plugin),
            seen_files,
            watched,
        );
    }
}

fn load_plugin_commands(
    registry: &mut SkillRegistry,
    store: &PluginStore,
    plugin: &InstalledPlugin,
    watched: &mut Vec<PathBuf>,
) {
    let discovery = tick_plugins::discover_commands(&plugin.root, &plugin.manifest);
    watched.extend(discovery.watched);
    for warning in discovery.warnings {
        registry.warn(format!("Plugin {}: {warning}", plugin.name()));
    }
    if discovery.commands.is_empty() {
        return;
    }
    watched.push(
        store
            .config_root()
            .join("credentials")
            .join(plugin.name())
            .join("user-config.json"),
    );
    let config =
        match crate::plugin_config::PluginUserConfig::load(store.config_root(), &plugin.manifest) {
            Ok(config) => config,
            Err(_) => {
                registry.warn(format!("Plugin {}: cannot read user config", plugin.name()));
                return;
            }
        };
    for command in discovery.commands {
        let markdown = command.markdown;
        let data = plugin_data_directory(registry, store, plugin, &markdown);
        let mut variables = vec![(PACKAGE_ROOT_VARIABLE, plugin.root.as_path())];
        variables.extend(PLUGIN_ROOT_VARIABLES.map(|name| (name, plugin.root.as_path())));
        variables.extend(PLUGIN_DATA_VARIABLES.map(|name| (name, data.as_path())));
        variables
            .extend(SKILL_DIRECTORY_VARIABLES.map(|name| (name, command.base_directory.as_path())));
        registry.register_command(
            CommandDocument {
                name: command.name,
                markdown,
                source: SkillSource::Plugin {
                    plugin: plugin.name().into(),
                    path: command.path,
                },
                base_directory: command.base_directory.clone(),
                metadata: command.metadata,
                public_config: plugin
                    .manifest
                    .user_config
                    .keys()
                    .map(|key| {
                        (
                            key.clone(),
                            config.skill_text(&format!("${{user_config.{key}}}")),
                        )
                    })
                    .collect(),
            },
            &variables,
        );
    }
}

fn load_directory(
    registry: &mut SkillRegistry,
    directory: &Path,
    origin: SkillOrigin<'_>,
    seen_files: &mut HashSet<PathBuf>,
    watched: &mut Vec<PathBuf>,
) {
    watched.push(directory.to_path_buf());
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            registry.warn(format!(
                "cannot read Skill directory {}: {error}",
                directory.display()
            ));
            return;
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(SKILL_FILE_NAME))
        .inspect(|path| watched.push(path.clone()))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    load_files(registry, paths, &origin, seen_files, watched);
}

fn load_files(
    registry: &mut SkillRegistry,
    paths: Vec<PathBuf>,
    origin: &SkillOrigin<'_>,
    seen_files: &mut HashSet<PathBuf>,
    watched: &mut Vec<PathBuf>,
) {
    for path in paths {
        watched.push(path.clone());
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                registry.warn(format!("cannot resolve Skill {}: {error}", path.display()));
                continue;
            }
        };
        watched.push(canonical.clone());
        if !seen_files.insert(canonical.clone()) {
            continue;
        }
        let markdown = match fs::read_to_string(&canonical) {
            Ok(markdown) => markdown,
            Err(error) => {
                registry.warn(format!(
                    "cannot read Skill {}: {error}",
                    canonical.display()
                ));
                continue;
            }
        };
        let path = canonical.clone();
        match origin {
            SkillOrigin::User => {
                registry.register_markdown(&markdown, SkillSource::User { path }, &[])
            }
            SkillOrigin::Project => {
                registry.register_markdown(&markdown, SkillSource::Project { path }, &[])
            }
            SkillOrigin::Plugin(store, plugin) => {
                watched.push(
                    store
                        .config_root()
                        .join("credentials")
                        .join(plugin.name())
                        .join("user-config.json"),
                );
                let config = match crate::plugin_config::PluginUserConfig::load(
                    store.config_root(),
                    &plugin.manifest,
                ) {
                    Ok(config) => config,
                    Err(_) => {
                        registry.warn(format!("Plugin {}: cannot read user config", plugin.name()));
                        continue;
                    }
                };
                let markdown = config.skill_text(&markdown);
                let data = plugin_data_directory(registry, store, plugin, &markdown);
                let skill_directory = path.parent().unwrap_or(&path).to_path_buf();
                let mut variables = vec![(PACKAGE_ROOT_VARIABLE, plugin.root.as_path())];
                variables.extend(PLUGIN_ROOT_VARIABLES.map(|name| (name, plugin.root.as_path())));
                variables.extend(PLUGIN_DATA_VARIABLES.map(|name| (name, data.as_path())));
                variables.extend(
                    SKILL_DIRECTORY_VARIABLES.map(|name| (name, skill_directory.as_path())),
                );
                registry.register_markdown(
                    &markdown,
                    SkillSource::Plugin {
                        plugin: plugin.name().to_owned(),
                        path,
                    },
                    &variables,
                );
            }
        }
    }
}

/// The plugin's data directory, created when the Skill in `markdown` names it.
fn plugin_data_directory(
    registry: &mut SkillRegistry,
    store: &PluginStore,
    plugin: &InstalledPlugin,
    markdown: &str,
) -> PathBuf {
    if !PLUGIN_DATA_VARIABLES
        .iter()
        .any(|variable| markdown.contains(variable))
    {
        return store.data_directory(plugin.name());
    }
    store
        .create_data_directory(plugin.name())
        .unwrap_or_else(|error| {
            registry.warn(format!(
                "the data directory of plugin {} cannot be created: {error}",
                plugin.name()
            ));
            store.data_directory(plugin.name())
        })
}

fn project_skill_directories(folder: &Path) -> Vec<PathBuf> {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    let mut directories = Vec::new();
    for ancestor in folder.ancestors() {
        directories.push(ancestor.join(PROJECT_SKILLS_DIRECTORY));
        if ancestor.join(".git").exists() {
            break;
        }
        if std::env::var_os("HOME").is_some_and(|home| ancestor == Path::new(&home)) {
            break;
        }
    }
    directories
}
