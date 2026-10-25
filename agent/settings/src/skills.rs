use crate::loop_skill::LoopFiles;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tick_packages::{InstalledPackage, PackageStore};
pub use tickloop_agent_policy::skills::*;
use tickloop_agent_protocol::SkillSource;
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
        let (source, root) = match origin {
            SkillOrigin::User => (SkillSource::User { path }, None),
            SkillOrigin::Project => (SkillSource::Project { path }, None),
            SkillOrigin::TickPackage(package) => (
                SkillSource::TickPackage {
                    package: package.manifest.name.clone(),
                    path,
                },
                Some(package.root.as_path()),
            ),
        };
        registry.register_markdown(&markdown, source, root);
    }
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
