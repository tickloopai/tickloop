use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{Build, Installation, Source, bundle, prepare_runtime};

#[derive(Clone, Debug)]
pub struct SelectedRuntime {
    pub executable: PathBuf,
    pub build: Build,
}

/// Resolves the selected release through this installation's original source.
/// This may probe executables and preserve a package-manager runtime; call it off the UI thread.
pub fn selected_runtime() -> Result<Option<SelectedRuntime>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let Some(installation) = Installation::detect(&executable)? else {
        return Ok(None);
    };
    selected_runtime_for(&installation, &executable)
}

/// Development Views follow the runtime explicitly activated by another development launch.
/// Managed installations continue selecting through their original installer.
pub fn follow_daemon_runtime(
    executable: &Path,
    expected_build: &str,
) -> Result<Option<SelectedRuntime>> {
    let current = std::env::current_exe()?.canonicalize()?;
    if Source::detect(&current)?.is_some_and(|source| !matches!(source, Source::Checkout { .. })) {
        return Ok(None);
    }
    let executable = executable.canonicalize()?;
    if executable == current {
        return Ok(None);
    }
    crate::ensure_same_runtime_source(&current, &executable)?;
    let directory = executable
        .parent()
        .context("Running daemon has no program directory")?;
    let build = bundle::read_build(directory)?;
    if build.identity != expected_build {
        anyhow::bail!("The running daemon executable changed after it was started");
    }
    if semver::Version::parse(&build.version)? < semver::Version::parse(env!("CARGO_PKG_VERSION"))?
    {
        return Ok(None);
    }
    bundle::verify(directory, &build)?;
    Ok(Some(SelectedRuntime { executable, build }))
}

fn selected_runtime_for(
    installation: &Installation,
    running_executable: &Path,
) -> Result<Option<SelectedRuntime>> {
    let (selected, selected_version) = match &installation.source {
        Source::Native { root } | Source::CargoRegistry { root, .. } => {
            let directory = root.join("current").canonicalize()?;
            let record = Installation::read(&directory)?;
            if record.source != installation.source {
                anyhow::bail!("The selected release belongs to another installation");
            }
            (
                directory.join("tickloop"),
                semver::Version::parse(&record.build.version)?,
            )
        }
        Source::Homebrew { executable } => {
            let selected = executable
                .parent()
                .context("Homebrew has no executable directory")?
                .join("tickloop")
                .canonicalize()?;
            let name = selected
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .context("Homebrew release has no version")?;
            let version = semver::Version::parse(name.split('_').next().unwrap_or(name))?;
            (selected, version)
        }
        Source::Cargo { root, .. } => {
            let (_, version) =
                crate::source::cargo_source(root)?.context("Cargo installation was removed")?;
            (root.join("bin/tickloop").canonicalize()?, version)
        }
        Source::Checkout { .. } => return Ok(None),
    };
    if selected == running_executable {
        return Ok(None);
    }
    if selected_version <= semver::Version::parse(&installation.build.version)? {
        return Ok(None);
    }
    let source = Source::detect(&selected)?
        .context("The selected release has no verified installation source")?;
    if source != installation.source {
        anyhow::bail!("The selected executable belongs to another installation");
    }
    let directory = selected
        .parent()
        .context("Selected runtime has no directory")?;
    let build = bundle::read_build(directory)?;
    if semver::Version::parse(&build.version)?
        <= semver::Version::parse(&installation.build.version)?
    {
        return Ok(None);
    }
    bundle::verify(directory, &build)?;
    let executable = prepare_runtime(&selected, &selected, &build)?.unwrap_or(selected);
    Ok(Some(SelectedRuntime { executable, build }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn release(root: &Path, version: &str, source: Source) -> PathBuf {
        let directory = root.join("versions").join(version);
        std::fs::create_dir_all(&directory).unwrap();
        let build = Build {
            version: version.into(),
            identity: format!("{version}+1234567890abcdef"),
        };
        for program in crate::PROGRAMS {
            let path = directory.join(program);
            std::fs::write(&path, format!("#!/bin/sh\ncase \"$1\" in\n--version) echo '{program} {version}';;\n--build-identity) echo '{}';;\n*) exit 2;;\nesac\n", build.identity)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Installation {
            source,
            build,
            pinned: false,
        }
        .write(&directory)
        .unwrap();
        directory.join("tickloop")
    }

    #[test]
    fn activation_uses_the_selected_complete_release_and_refuses_downgrades() {
        let root = tempfile::tempdir().unwrap();
        let source = Source::Native {
            root: root.path().into(),
        };
        let old = release(root.path(), "1.0.0", source.clone());
        let new = release(root.path(), "1.1.0", source);
        symlink("versions/1.1.0", root.path().join("current")).unwrap();
        let installation = Installation::read(old.parent().unwrap()).unwrap();
        assert_eq!(
            selected_runtime_for(&installation, &old)
                .unwrap()
                .unwrap()
                .executable,
            new.canonicalize().unwrap()
        );
        let newer = Installation::read(new.parent().unwrap()).unwrap();
        std::fs::remove_file(root.path().join("current")).unwrap();
        symlink("versions/1.0.0", root.path().join("current")).unwrap();
        assert!(selected_runtime_for(&newer, &new).unwrap().is_none());
    }

    #[test]
    fn activation_rejects_a_selected_release_with_another_installation_source() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let old = release(
            root.path(),
            "1.0.0",
            Source::Native {
                root: root.path().into(),
            },
        );
        release(
            root.path(),
            "1.1.0",
            Source::Native {
                root: other.path().into(),
            },
        );
        symlink("versions/1.1.0", root.path().join("current")).unwrap();
        let installation = Installation::read(old.parent().unwrap()).unwrap();
        assert!(
            selected_runtime_for(&installation, &old)
                .unwrap_err()
                .to_string()
                .contains("another installation")
        );
    }

    fn selected_candidate(directory: &Path, marker: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let candidate = directory.join("tickloop");
        std::fs::write(&candidate, format!(
            "#!/bin/sh\n: > '{}'\ncase \"$1\" in\n--version) echo 'tickloop 2.0.0';;\n--build-identity) echo '2.0.0+fixture';;\n*) exit 2;;\nesac\n",
            marker.display(),
        )).unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
        candidate
    }

    #[test]
    fn activation_rejects_foreign_homebrew_before_probing_selected_executable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let original = root.join("original-brew");
        let foreign = root.join("foreign-brew");
        for prefix in [&original, &foreign] {
            std::fs::create_dir_all(prefix.join("bin")).unwrap();
            std::fs::write(prefix.join("bin/brew"), "metadata-only Homebrew marker").unwrap();
        }
        let old = release(
            &root.join("running"),
            "1.0.0",
            Source::Homebrew {
                executable: original.join("bin/brew"),
            },
        );
        let installation = Installation::read(old.parent().unwrap()).unwrap();
        let prefix = foreign.join("Cellar/tickloop/2.0.0");
        let marker = root.join("candidate-executed");
        let candidate = selected_candidate(&prefix.join("bin"), &marker);
        std::fs::write(
            prefix.join("INSTALL_RECEIPT.json"),
            serde_json::to_vec(&serde_json::json!({"source":{"tap":"tickloopai/tap"}})).unwrap(),
        )
        .unwrap();
        symlink(candidate, original.join("bin/tickloop")).unwrap();
        let error = selected_runtime_for(&installation, &old).unwrap_err();
        assert!(
            error.to_string().contains("another installation"),
            "{error:#}"
        );
        assert!(
            !marker.exists(),
            "foreign Homebrew must be rejected before probing its program"
        );
    }

    #[test]
    fn activation_rejects_changed_cargo_repository_before_probing_selected_executable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let cargo = root.join("cargo");
        let old = release(
            &root.join("running"),
            "1.0.0",
            Source::Cargo {
                root: cargo.clone(),
                repository: "https://example.test/original".into(),
            },
        );
        let installation = Installation::read(old.parent().unwrap()).unwrap();
        let marker = root.join("candidate-executed");
        selected_candidate(&cargo.join("bin"), &marker);
        let installs = [
            "tickloop-agent",
            "tickloop-tickhost",
            "tickloop-tick-reader",
        ]
        .into_iter()
        .map(|name| {
            (
                format!(
                    "{name} 2.0.0 (git+https://example.test/another#{})",
                    "a".repeat(40)
                ),
                serde_json::json!({}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
        std::fs::write(
            cargo.join(".crates2.json"),
            serde_json::to_vec(&serde_json::json!({"installs":installs})).unwrap(),
        )
        .unwrap();
        let error = selected_runtime_for(&installation, &old).unwrap_err();
        assert!(
            error.to_string().contains("another installation"),
            "{error:#}"
        );
        assert!(
            !marker.exists(),
            "changed Cargo repository must be rejected before probing its program"
        );
    }
}
