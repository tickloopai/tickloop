use crate::loop_skill::LoopFiles;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tick_packages::{InstalledPackage, PackageStore};
use tick_plugins::{InstalledPlugin, PluginStore};
pub use tickloop_harness::policy::skills::*;
use tickloop_harness::protocol::SkillSource;
const SKILL_FILE_NAME: &str = "SKILL.md";
const SKILLS_DIRECTORY: &str = "skills";
const PROJECT_SKILLS_DIRECTORY: &str = ".tickloop/skills";

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
        let mut registry = Self::with_builtins(builtins);
        registry.set_loop_instructions(Arc::new(LoopFiles::new(Some(folder), config_root)));
        let mut seen_files = HashSet::new();
        if let Some(root) = config_root {
            load_directory(
                &mut registry,
                &root.join(SKILLS_DIRECTORY),
                SkillOrigin::User,
                &mut seen_files,
            );
            load_tick_packages(&mut registry, root, &mut seen_files);
            load_plugins(&mut registry, root, &mut seen_files);
        }
        for directory in project_skill_directories(folder) {
            load_directory(
                &mut registry,
                &directory,
                SkillOrigin::Project,
                &mut seen_files,
            );
        }
        registry
    }
}

enum SkillOrigin<'a> {
    User,
    Project,
    TickPackage(&'a InstalledPackage),
    Plugin(&'a PluginStore, &'a InstalledPlugin),
}

fn load_tick_packages(
    registry: &mut SkillRegistry,
    config_root: &Path,
    seen_files: &mut HashSet<PathBuf>,
) {
    let installed = match PackageStore::in_config_root(config_root).installed() {
        Ok(installed) => installed,
        Err(error) => {
            registry.warn(format!(
                "the Skills of installed Tick packages are not loaded: {error}"
            ));
            return;
        }
    };
    for broken in installed.broken {
        registry.warn(format!(
                "the Skills of Tick package {} are not loaded: it cannot be used ({}); install it again",
                broken.name, broken.reason
            ));
    }
    for package in &installed.packages {
        load_directory(
            registry,
            &package.root.join(SKILLS_DIRECTORY),
            SkillOrigin::TickPackage(package),
            seen_files,
        );
    }
}

/// Loads the Skills of every enabled plugin; a disabled one stays installed but unloaded.
fn load_plugins(
    registry: &mut SkillRegistry,
    config_root: &Path,
    seen_files: &mut HashSet<PathBuf>,
) {
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
        );
    }
}

fn load_directory(
    registry: &mut SkillRegistry,
    directory: &Path,
    origin: SkillOrigin<'_>,
    seen_files: &mut HashSet<PathBuf>,
) {
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
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    load_files(registry, paths, &origin, seen_files);
}

fn load_files(
    registry: &mut SkillRegistry,
    paths: Vec<PathBuf>,
    origin: &SkillOrigin<'_>,
    seen_files: &mut HashSet<PathBuf>,
) {
    for path in paths {
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                registry.warn(format!("cannot resolve Skill {}: {error}", path.display()));
                continue;
            }
        };
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
            SkillOrigin::TickPackage(package) => registry.register_markdown(
                &markdown,
                SkillSource::TickPackage {
                    package: package.manifest.name.clone(),
                    path,
                },
                &[(PACKAGE_ROOT_VARIABLE, &package.root)],
            ),
            SkillOrigin::Plugin(store, plugin) => {
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
