//! Where an Agent open finds the program of a Tick. A path containing `/` is taken relative to the
//! working directory; a bare name is the program of an installed Tick package first, then found on
//! PATH relative to the working directory.

use std::{
    ffi::OsStr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tick_plugins::{InstalledPlugin, PluginError, PluginStore};

use crate::host_error::HostError;

const TICKHOST_EXECUTABLE: &str = "tickhost";
/// Every Tick package names its program `tick-<package>`.
const PACKAGE_PROGRAM_PREFIX: &str = "tick-";
const INVALID_ARGUMENTS: &str = "InvalidArguments";

/// The program an open runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedExecutable {
    /// The canonical path of the program.
    pub path: PathBuf,
    /// The installed Tick package whose program it is.
    pub plugin: Option<InstalledPlugin>,
}

/// Resolves a program the way an Agent open does, finding the programs of the Tick packages
/// installed in `packages`.
pub fn resolve_executable(
    program: &str,
    cwd: &Path,
    packages: Option<&PluginStore>,
) -> Result<ResolvedExecutable, HostError> {
    resolve_executable_from(
        program,
        cwd,
        &std::env::var_os("PATH").unwrap_or_default(),
        packages,
    )
}

/// The `tickhost` installed beside the Agent, else the one on PATH.
pub fn bundled_tickhost_executable() -> Option<PathBuf> {
    let beside_agent = std::env::current_exe()
        .ok()?
        .parent()?
        .join(TICKHOST_EXECUTABLE);
    executable_file(&beside_agent).or_else(|| {
        resolve_executable(TICKHOST_EXECUTABLE, &std::env::current_dir().ok()?, None)
            .ok()
            .map(|resolved| resolved.path)
    })
}

fn resolve_executable_from(
    program: &str,
    cwd: &Path,
    search_path: &OsStr,
    packages: Option<&PluginStore>,
) -> Result<ResolvedExecutable, HostError> {
    if program.contains('/') {
        let path = executable_file(&cwd.join(program)).ok_or_else(|| not_found(program))?;
        let package = packages.and_then(|packages| installed_plugin_of(&path, packages));
        if let Some(plugin) = &package
            && !plugin.enabled
        {
            return Err(plugin_disabled(plugin.name()));
        }
        return Ok(ResolvedExecutable {
            path,
            plugin: package,
        });
    }
    let package_name = program
        .strip_prefix(PACKAGE_PROGRAM_PREFIX)
        .filter(|name| tick_plugins::validate_plugin_name(name).is_ok());
    let mut has_no_tick = false;
    if let (Some(name), Some(packages)) = (package_name, packages) {
        match packages.get(name) {
            Ok(package) if !package.enabled => return Err(plugin_disabled(name)),
            Ok(package) if package.manifest.tick.is_some() => {
                let path =
                    executable_file(&package.executable().expect("Tick component has a program"))
                        .ok_or_else(|| not_found(program))?;
                return Ok(ResolvedExecutable {
                    path,
                    plugin: Some(package),
                });
            }
            Ok(_) => has_no_tick = true,
            Err(PluginError::NotInstalled(_)) => {}
            Err(error) => return Err(HostError::before(INVALID_ARGUMENTS, error.to_string())),
        }
    }
    std::env::split_paths(search_path)
        .find_map(|directory| executable_file(&cwd.join(directory).join(program)))
        .map(|path| ResolvedExecutable { path, plugin: None })
        .ok_or_else(|| match package_name {
            Some(name) if has_no_tick => HostError::before(
                INVALID_ARGUMENTS,
                format!("plugin {name} has no Tick program; executable not found: {program}"),
            ),
            Some(name) => plugin_not_installed(program, name),
            None => not_found(program),
        })
}

pub fn plugin_disabled(name: &str) -> HostError {
    HostError::before(
        INVALID_ARGUMENTS,
        format!(
            "plugin {name} is disabled; the user enables it with `tickloop tick enable {name}`"
        ),
    )
}

/// The failure of a program that comes from the Tick package `name`, which is not installed.
pub fn plugin_not_installed(program: &str, name: &str) -> HostError {
    HostError::before(
        INVALID_ARGUMENTS,
        format!(
            "executable not found: {program} comes from plugin {name}, which is not installed; the user installs it from /ticks in the Agent (or `tickloop tick install {name}` in a terminal)"
        ),
    )
}

/// The plugin a recorded program came from, once uninstalled. The cache path carries the
/// installation identity; a marketplace alias can differ from the program's file name.
pub fn uninstalled_plugin_of(path: &Path, plugins: &PluginStore) -> Option<String> {
    path.file_name()?
        .to_str()?
        .strip_prefix(PACKAGE_PROGRAM_PREFIX)?;
    let directory = plugins
        .directory()
        .canonicalize()
        .unwrap_or_else(|_| plugins.directory().to_path_buf());
    let relative = path.strip_prefix(directory.join("cache")).ok()?;
    let mut components = relative.components();
    let marketplace = components.next()?.as_os_str().to_str()?;
    tick_plugins::validate_plugin_name(marketplace).ok()?;
    let name = components.next()?.as_os_str().to_str()?;
    tick_plugins::validate_plugin_name(name).ok()?;
    matches!(plugins.get(name), Err(PluginError::NotInstalled(_))).then(|| name.to_owned())
}

/// The installed plugin whose canonical program path is exactly `path`.
pub fn installed_plugin_of(path: &Path, plugins: &PluginStore) -> Option<InstalledPlugin> {
    plugins
        .installed()
        .ok()?
        .plugins
        .into_iter()
        .find(|plugin| {
            plugin
                .executable()
                .and_then(|program| executable_file(&program))
                .as_deref()
                == Some(path)
        })
}

/// The canonical path of `path` when it is an executable file.
fn executable_file(path: &Path) -> Option<PathBuf> {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then(|| path.canonicalize().ok())
        .flatten()
}

fn not_found(program: &str) -> HostError {
    HostError::before(
        INVALID_ARGUMENTS,
        format!("executable not found: {program}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Installs a package `name` whose program speaks the Tick protocol into `packages`.
    fn install_package(packages: &PluginStore, sources: &Path, name: &str) -> InstalledPlugin {
        let source = sources.join(name);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join(tick_packages::MANIFEST_FILE_NAME),
            serde_json::json!({
                "name": name,
                "version": "1.0.0",
                "description": "A test package",
                "executable": format!("bin/tick-{name}"),
                "protocol": true,
            })
            .to_string(),
        )
        .unwrap();
        write_executable(&source.join("bin").join(format!("tick-{name}")));
        packages.install_path(&source).unwrap().plugin
    }

    #[test]
    fn an_installed_package_program_comes_before_path() {
        let directory = tempfile::tempdir().unwrap();
        let path_bin = directory.path().join("path-bin");
        let packages = PluginStore::in_config_root(&directory.path().join("config"));
        let sources = directory.path().join("sources");
        let sample = install_package(&packages, &sources, "sample");
        for executable in [path_bin.join("tick-sample"), path_bin.join("tick-other")] {
            write_executable(&executable);
        }
        let resolve = |program: &str| {
            resolve_executable_from(
                program,
                directory.path(),
                path_bin.as_os_str(),
                Some(&packages),
            )
        };

        let resolved = resolve("tick-sample").unwrap();
        assert_eq!(resolved.path, sample.executable().unwrap());
        assert_eq!(resolved.plugin.unwrap().manifest.name, "sample");
        let by_path = resolve(sample.executable().unwrap().to_str().unwrap()).unwrap();
        assert_eq!(
            by_path.plugin.map(|package| package.manifest.name),
            Some("sample".to_owned()),
            "a path naming an installed program is that package's"
        );
        assert_eq!(
            resolve("path-bin/tick-sample").unwrap().plugin,
            None,
            "another program of the same name belongs to no package"
        );
        assert_eq!(
            resolve("tick-other").unwrap(),
            ResolvedExecutable {
                path: path_bin.join("tick-other").canonicalize().unwrap(),
                plugin: None,
            }
        );

        packages.uninstall("sample", false).unwrap();
        assert_eq!(
            resolve("tick-sample").unwrap(),
            ResolvedExecutable {
                path: path_bin.join("tick-sample").canonicalize().unwrap(),
                plugin: None,
            }
        );
        std::fs::remove_file(path_bin.join("tick-sample")).unwrap();
        let error = resolve("tick-sample").unwrap_err();
        assert_eq!(
            error.to_string(),
            "InvalidArguments: executable not found: tick-sample comes from plugin sample, which is not installed; the user installs it from /ticks in the Agent (or `tickloop tick install sample` in a terminal)"
        );
        assert!(!error.dispatched);
        assert_eq!(
            resolve("tick-").unwrap_err().message,
            "executable not found: tick-",
            "only a Tick package name gets the install hint"
        );
    }

    #[test]
    fn a_recorded_program_names_its_package_only_once_that_is_uninstalled() {
        let directory = tempfile::tempdir().unwrap();
        let packages = PluginStore::in_config_root(&directory.path().join("config"));
        let sources = directory.path().join("sources");
        let sample = install_package(&packages, &sources, "sample");
        let other = install_package(&packages, &sources, "other");
        let recorded = sample.executable().unwrap();

        assert_eq!(uninstalled_plugin_of(&recorded, &packages), None);
        packages.uninstall("sample", false).unwrap();
        assert_eq!(
            uninstalled_plugin_of(&recorded, &packages).as_deref(),
            Some("sample")
        );
        for elsewhere in [
            directory.path().join("bin/tick-sample"),
            other.root.join("bin/tick-sample"),
        ] {
            assert_eq!(uninstalled_plugin_of(&elsewhere, &packages), None);
        }
    }

    #[test]
    fn a_damaged_registry_refuses_package_programs_instead_of_running_another() {
        let directory = tempfile::tempdir().unwrap();
        let path_bin = directory.path().join("path-bin");
        write_executable(&path_bin.join("tick-sample"));
        let packages = PluginStore::in_config_root(&directory.path().join("config"));
        std::fs::create_dir_all(packages.directory()).unwrap();
        std::fs::write(
            packages.directory().join(tick_plugins::REGISTRY_FILE_NAME),
            "not json",
        )
        .unwrap();

        let error = resolve_executable_from(
            "tick-sample",
            directory.path(),
            path_bin.as_os_str(),
            Some(&packages),
        )
        .unwrap_err();
        assert!(error.message.contains("is damaged"), "{error}");
    }
    #[test]
    fn a_disabled_plugin_cannot_open_by_name_even_when_path_has_the_same_program() {
        let directory = tempfile::tempdir().unwrap();
        let packages = PluginStore::in_config_root(&directory.path().join("config"));
        install_package(&packages, &directory.path().join("sources"), "sample");
        packages.set_enabled("sample", false).unwrap();
        let path_bin = directory.path().join("path-bin");
        write_executable(&path_bin.join("tick-sample"));
        let error = resolve_executable_from(
            "tick-sample",
            directory.path(),
            path_bin.as_os_str(),
            Some(&packages),
        )
        .unwrap_err();
        assert!(
            error.message.contains("tickloop tick enable sample"),
            "{error}"
        );
        assert!(!error.dispatched);
    }
    #[test]
    fn a_damaged_earlier_registry_refuses_path_fallback_during_migration() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        std::fs::create_dir_all(config.join("ticks")).unwrap();
        std::fs::write(config.join("ticks/installed.json"), "not json").unwrap();
        let path_bin = directory.path().join("path-bin");
        write_executable(&path_bin.join("tick-sample"));
        let plugins = PluginStore::in_config_root(&config);
        let error = resolve_executable_from(
            "tick-sample",
            directory.path(),
            path_bin.as_os_str(),
            Some(&plugins),
        )
        .unwrap_err();
        assert!(error.message.contains("is damaged"), "{error}");
    }

    #[test]
    fn a_skill_only_plugin_is_not_reported_as_uninstalled() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("notes");
        std::fs::create_dir(&source).unwrap();
        let plugins = PluginStore::in_config_root(&directory.path().join("config"));
        plugins.install_path(&source).unwrap();
        let error = resolve_executable_from(
            "tick-notes",
            directory.path(),
            OsStr::new(""),
            Some(&plugins),
        )
        .unwrap_err();
        assert!(
            error.message.contains("plugin notes has no Tick program"),
            "{error}"
        );
    }
}
