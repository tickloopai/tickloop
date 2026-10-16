//! Where an Agent open finds the program of a Tick. A path containing `/` is taken relative to the
//! working directory; a bare name is a bundled executable beside the Agent first, then the program
//! of an installed Tick package, then found on PATH relative to the working directory.

use std::{
    ffi::OsStr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tick_packages::{InstalledPackage, PackageError, PackageStore};

use crate::host_error::HostError;

/// The executables installed beside the Agent, which an open finds there before anything else.
pub const BUNDLED_EXECUTABLES: [&str; 2] = ["tick-reader", "tickhost"];
/// Every Tick package names its program `tick-<package>`.
const PACKAGE_PROGRAM_PREFIX: &str = "tick-";
const INVALID_ARGUMENTS: &str = "InvalidArguments";

/// The program an open runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedExecutable {
    /// The canonical path of the program.
    pub path: PathBuf,
    /// The installed Tick package whose program it is.
    pub package: Option<InstalledPackage>,
}

/// Resolves a program the way an Agent open does, finding the programs of the Tick packages
/// installed in `packages`.
pub fn resolve_executable(
    program: &str,
    cwd: &Path,
    packages: Option<&PackageStore>,
) -> Result<ResolvedExecutable, HostError> {
    resolve_executable_from(
        program,
        cwd,
        std::env::current_exe().ok().as_deref(),
        &std::env::var_os("PATH").unwrap_or_default(),
        packages,
    )
}

pub fn bundled_reader_executable() -> Option<PathBuf> {
    bundled_executable("tick-reader")
}

pub fn bundled_tickhost_executable() -> Option<PathBuf> {
    bundled_executable("tickhost")
}

fn bundled_executable(program: &str) -> Option<PathBuf> {
    resolve_executable(program, &std::env::current_dir().ok()?, None)
        .ok()
        .map(|resolved| resolved.path)
}

fn resolve_executable_from(
    program: &str,
    cwd: &Path,
    agent_executable: Option<&Path>,
    search_path: &OsStr,
    packages: Option<&PackageStore>,
) -> Result<ResolvedExecutable, HostError> {
    if program.contains('/') {
        let path = executable_file(&cwd.join(program)).ok_or_else(|| not_found(program))?;
        let package = packages.and_then(|packages| installed_package_of(&path, packages));
        return Ok(ResolvedExecutable { path, package });
    }
    let is_bundled = BUNDLED_EXECUTABLES.contains(&program);
    if let Some(path) = agent_executable
        .and_then(Path::parent)
        .filter(|_| is_bundled)
        .and_then(|directory| executable_file(&directory.join(program)))
    {
        return Ok(ResolvedExecutable {
            path,
            package: None,
        });
    }
    let package_name = program.strip_prefix(PACKAGE_PROGRAM_PREFIX);
    if let (Some(name), Some(packages)) = (package_name, packages) {
        match packages.get(name) {
            Ok(package) => {
                let path =
                    executable_file(&package.executable()).ok_or_else(|| not_found(program))?;
                return Ok(ResolvedExecutable {
                    path,
                    package: Some(package),
                });
            }
            Err(PackageError::NotInstalled(_) | PackageError::Invalid(_)) => {}
            Err(error) => return Err(HostError::before(INVALID_ARGUMENTS, error.to_string())),
        }
    }
    std::env::split_paths(search_path)
        .find_map(|directory| executable_file(&cwd.join(directory).join(program)))
        .map(|path| ResolvedExecutable {
            path,
            package: None,
        })
        .ok_or_else(|| {
            match package_name
                .filter(|name| !is_bundled && tick_packages::validate_package_name(name).is_ok())
            {
                Some(name) => package_not_installed(program, name),
                None => not_found(program),
            }
        })
}

/// The failure of a program that comes from the Tick package `name`, which is not installed.
pub fn package_not_installed(program: &str, name: &str) -> HostError {
    HostError::before(
        INVALID_ARGUMENTS,
        format!(
            "executable not found: {program} comes from Tick package {name}, which is not installed; the user installs it from /ticks in the Agent (or `tickloop tick install {name}` in a terminal)"
        ),
    )
}

/// The package a program recorded at `path` came from, once that package is no longer installed
/// in `packages`: the program lay in the package's directory there and was named after it.
pub fn uninstalled_package_of(path: &Path, packages: &PackageStore) -> Option<String> {
    let name = path
        .file_name()?
        .to_str()?
        .strip_prefix(PACKAGE_PROGRAM_PREFIX)?;
    let directory = packages
        .directory()
        .canonicalize()
        .unwrap_or_else(|_| packages.directory().to_path_buf());
    path.strip_prefix(directory.join(name)).ok()?;
    matches!(packages.get(name), Err(PackageError::NotInstalled(_))).then(|| name.to_owned())
}

/// The installed package whose program `path` is.
pub fn installed_package_of(path: &Path, packages: &PackageStore) -> Option<InstalledPackage> {
    let name = path
        .file_name()?
        .to_str()?
        .strip_prefix(PACKAGE_PROGRAM_PREFIX)?;
    let package = packages.get(name).ok()?;
    (executable_file(&package.executable()).as_deref() == Some(path)).then_some(package)
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
    fn install_package(packages: &PackageStore, sources: &Path, name: &str) -> InstalledPackage {
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
        packages.install_path(&source).unwrap().package
    }

    #[test]
    fn bundled_reader_resolution_uses_its_installed_companion_before_path() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("installed");
        let path_bin = directory.path().join("path-bin");
        for executable in [installed.join("tick-reader"), path_bin.join("tick-reader")] {
            write_executable(&executable);
        }
        let agent = installed.join("tickloop");
        let resolve = |program| {
            resolve_executable_from(
                program,
                directory.path(),
                Some(&agent),
                path_bin.as_os_str(),
                None,
            )
            .map(|resolved| resolved.path)
        };

        assert_eq!(
            resolve("tick-reader").unwrap(),
            installed.join("tick-reader").canonicalize().unwrap()
        );
        assert_eq!(
            resolve("path-bin/tick-reader").unwrap(),
            path_bin.join("tick-reader").canonicalize().unwrap()
        );
        std::fs::remove_file(installed.join("tick-reader")).unwrap();
        assert_eq!(
            resolve("tick-reader").unwrap(),
            path_bin.join("tick-reader").canonicalize().unwrap()
        );
        std::fs::remove_file(path_bin.join("tick-reader")).unwrap();
        assert_eq!(
            resolve("tick-reader").unwrap_err().message,
            "executable not found: tick-reader",
            "the bundled Reader is no Tick package to install"
        );
        write_executable(&installed.join("tickhost"));
        assert_eq!(
            resolve("tickhost").unwrap(),
            installed.join("tickhost").canonicalize().unwrap()
        );
        write_executable(&installed.join("tick"));
        assert!(
            resolve("tick").is_err(),
            "a program beside the Agent is found there only when it is bundled"
        );
    }

    #[test]
    fn an_installed_package_program_comes_after_the_bundled_one_and_before_path() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("installed");
        let path_bin = directory.path().join("path-bin");
        let packages = PackageStore::in_config_root(&directory.path().join("config"));
        let sources = directory.path().join("sources");
        let sample = install_package(&packages, &sources, "sample");
        let reader = install_package(&packages, &sources, "reader");
        for executable in [
            installed.join("tick-reader"),
            path_bin.join("tick-sample"),
            path_bin.join("tick-other"),
        ] {
            write_executable(&executable);
        }
        let agent = installed.join("tickloop");
        let resolve = |program: &str| {
            resolve_executable_from(
                program,
                directory.path(),
                Some(&agent),
                path_bin.as_os_str(),
                Some(&packages),
            )
        };

        let resolved = resolve("tick-sample").unwrap();
        assert_eq!(resolved.path, sample.executable());
        assert_eq!(resolved.package.unwrap().manifest.name, "sample");
        let by_path = resolve(sample.executable().to_str().unwrap()).unwrap();
        assert_eq!(
            by_path.package.map(|package| package.manifest.name),
            Some("sample".to_owned()),
            "a path naming an installed program is that package's"
        );
        assert_eq!(
            resolve("path-bin/tick-sample").unwrap().package,
            None,
            "another program of the same name belongs to no package"
        );
        assert_eq!(
            resolve("tick-other").unwrap(),
            ResolvedExecutable {
                path: path_bin.join("tick-other").canonicalize().unwrap(),
                package: None,
            }
        );

        assert_eq!(
            resolve("tick-reader").unwrap(),
            ResolvedExecutable {
                path: installed.join("tick-reader").canonicalize().unwrap(),
                package: None,
            },
            "the bundled program comes before a package's program of its name"
        );
        std::fs::remove_file(installed.join("tick-reader")).unwrap();
        assert_eq!(resolve("tick-reader").unwrap().path, reader.executable());

        packages.uninstall("sample").unwrap();
        assert_eq!(
            resolve("tick-sample").unwrap(),
            ResolvedExecutable {
                path: path_bin.join("tick-sample").canonicalize().unwrap(),
                package: None,
            }
        );
        std::fs::remove_file(path_bin.join("tick-sample")).unwrap();
        let error = resolve("tick-sample").unwrap_err();
        assert_eq!(
            error.to_string(),
            "InvalidArguments: executable not found: tick-sample comes from Tick package sample, which is not installed; the user installs it from /ticks in the Agent (or `tickloop tick install sample` in a terminal)"
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
        let packages = PackageStore::in_config_root(&directory.path().join("config"));
        let sources = directory.path().join("sources");
        let sample = install_package(&packages, &sources, "sample");
        let other = install_package(&packages, &sources, "other");
        let recorded = sample.executable();

        assert_eq!(uninstalled_package_of(&recorded, &packages), None);
        packages.uninstall("sample").unwrap();
        assert_eq!(
            uninstalled_package_of(&recorded, &packages).as_deref(),
            Some("sample")
        );
        for elsewhere in [
            directory.path().join("bin/tick-sample"),
            other.root.join("bin/tick-sample"),
        ] {
            assert_eq!(uninstalled_package_of(&elsewhere, &packages), None);
        }
    }

    #[test]
    fn a_damaged_registry_refuses_package_programs_instead_of_running_another() {
        let directory = tempfile::tempdir().unwrap();
        let path_bin = directory.path().join("path-bin");
        write_executable(&path_bin.join("tick-sample"));
        let packages = PackageStore::in_config_root(&directory.path().join("config"));
        std::fs::create_dir_all(packages.directory()).unwrap();
        std::fs::write(
            packages.directory().join(tick_packages::REGISTRY_FILE_NAME),
            "not json",
        )
        .unwrap();

        let error = resolve_executable_from(
            "tick-sample",
            directory.path(),
            None,
            path_bin.as_os_str(),
            Some(&packages),
        )
        .unwrap_err();
        assert!(error.message.contains("is damaged"), "{error}");
    }
}
