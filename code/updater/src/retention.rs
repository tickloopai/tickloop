use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tickhost_connect::executable_lease::{ExecutableLease, lease_directory};

use crate::{Installation, Lock, Source};

const RETAIN_UNUSED_RELEASES: usize = 2;

/// Collects only registered, unreferenced bundles from this installation source.
pub fn cleanup_unused_runtimes() -> Result<Vec<PathBuf>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let Some(installation) = Installation::detect(&executable)? else {
        return Ok(Vec::new());
    };
    let (root, versions, current) = match &installation.source {
        Source::Native { root } | Source::CargoRegistry { root, .. } => (
            root.clone(),
            root.join("versions"),
            root.join("current").canonicalize().ok(),
        ),
        Source::Homebrew { .. } | Source::Cargo { .. } => {
            let root = installation.state_directory()?.join("runtimes");
            (root.clone(), root, None)
        }
        Source::Checkout { root } => {
            let development = root.join("target/dev-runs");
            let development = development.canonicalize().unwrap_or(development);
            if executable.starts_with(&development) {
                (
                    development.clone(),
                    development,
                    executable.parent().map(Path::to_owned),
                )
            } else {
                let root = installation.state_directory()?.join("runtimes");
                (root.clone(), root, None)
            }
        }
    };
    if !versions.is_dir() {
        return Ok(Vec::new());
    }
    let _installation_lock = match Lock::acquire(&root.join("install.lock")) {
        Ok(lock) => lock,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    collect(
        &versions,
        &installation.source,
        current.as_deref(),
        &lease_directory()?,
    )
}

fn collect(
    versions: &Path,
    source: &Source,
    selected: Option<&Path>,
    leases: &Path,
) -> Result<Vec<PathBuf>> {
    let selected = selected.map(Path::canonicalize).transpose()?;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(versions)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            continue;
        }
        let container = path.canonicalize()?;
        let development = matches!(source, Source::Checkout { .. })
            && entry.file_name().to_string_lossy().starts_with("run.");
        let directory = if development {
            if fs::read_dir(&container)?.any(|entry| match entry {
                Ok(entry) => entry.file_name() != "bin",
                Err(_) => true,
            }) {
                continue;
            }
            let bin = container.join("bin");
            if !fs::symlink_metadata(&bin).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            if fs::read_dir(&bin)?.any(|entry| match entry {
                Ok(entry) => {
                    let name = entry.file_name();
                    !entry.file_type().is_ok_and(|kind| kind.is_file())
                        || name != "installation.json"
                            && !crate::PROGRAMS.iter().any(|program| name == *program)
                }
                Err(_) => true,
            }) {
                continue;
            }
            bin
        } else {
            container.clone()
        };
        if selected
            .as_ref()
            .is_some_and(|selected| selected == &directory)
        {
            continue;
        }
        let Ok(record) = Installation::read(&directory) else {
            continue;
        };
        if &record.source != source {
            continue;
        }
        let Some(lease) = ExecutableLease::try_retire(&directory, leases)? else {
            continue;
        };
        let version = semver::Version::parse(&record.build.version)?;
        candidates.push((version, metadata.modified()?, container, development, lease));
    }
    candidates.sort_by(|left, right| {
        let recent = right.1.cmp(&left.1);
        if matches!(source, Source::Checkout { .. }) {
            recent.then_with(|| left.2.cmp(&right.2))
        } else {
            right
                .0
                .cmp(&left.0)
                .then(recent)
                .then_with(|| left.2.cmp(&right.2))
        }
    });
    let mut removed = Vec::new();
    for (_, _, directory, development, _lease) in
        candidates.into_iter().skip(RETAIN_UNUSED_RELEASES)
    {
        let tombstone = tempfile::Builder::new()
            .prefix(".retired-")
            .tempdir_in(versions)?;
        let retired = tombstone.path().join("bundle");
        let permissions = fs::metadata(&directory)?.permissions();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        if let Err(error) = fs::rename(&directory, &retired) {
            fs::set_permissions(&directory, permissions)?;
            return Err(error).context("Cannot retire an unused runtime");
        }
        if development {
            fs::set_permissions(retired.join("bin"), fs::Permissions::from_mode(0o700))?;
        }
        fs::remove_dir_all(&retired).context("Cannot remove a retired runtime")?;
        removed.push(directory);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Build;

    #[test]
    fn cleanup_protects_selected_live_and_legacy_bundles() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        fs::create_dir(&versions).unwrap();
        let leases = root.path().join("leases");
        let source = Source::Native {
            root: root.path().into(),
        };
        let mut paths = Vec::new();
        for index in 0..6 {
            let version = format!("1.0.{index}");
            let directory = versions.join(&version);
            fs::create_dir(&directory).unwrap();
            Installation {
                source: source.clone(),
                build: Build {
                    version,
                    identity: format!("identity{index}"),
                },
                pinned: false,
            }
            .write(&directory)
            .unwrap();
            if index > 0 {
                drop(ExecutableLease::acquire(&directory, &leases).unwrap());
            }
            paths.push(directory);
        }
        let active = ExecutableLease::acquire(&paths[1], &leases).unwrap();
        let removed = collect(&versions, &source, Some(&paths[5]), &leases).unwrap();
        let expected = root.path().canonicalize().unwrap().join("versions/1.0.2");
        assert_eq!(removed, vec![expected]);
        for index in [0, 1, 3, 4, 5] {
            assert!(paths[index].is_dir());
        }
        drop(active);
    }

    #[test]
    fn cleanup_does_not_follow_a_version_directory_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "owned elsewhere").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("1.0.0")).unwrap();
        let source = Source::Native {
            root: root.path().into(),
        };
        assert!(
            collect(root.path(), &source, None, &root.path().join("leases"))
                .unwrap()
                .is_empty()
        );
        assert!(outside.path().join("keep").is_file());
    }

    #[test]
    fn development_cleanup_preserves_active_selected_and_unrecognized_runs() {
        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("target/dev-runs");
        fs::create_dir_all(&runs).unwrap();
        let leases = root.path().join("leases");
        let source = Source::Checkout {
            root: root.path().canonicalize().unwrap(),
        };
        let mut directories = Vec::new();
        for index in 0..7 {
            let directory = runs.join(format!("run.{index}"));
            let bin = directory.join("bin");
            fs::create_dir_all(&bin).unwrap();
            for program in crate::PROGRAMS {
                fs::write(bin.join(program), "test executable").unwrap();
            }
            Installation {
                source: source.clone(),
                build: Build {
                    version: format!("1.0.{}", 7 - index),
                    identity: format!("development-{index}"),
                },
                pinned: true,
            }
            .write(&bin)
            .unwrap();
            drop(ExecutableLease::acquire(&bin, &leases).unwrap());
            fs::File::open(&directory)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(index + 1),
                )
                .unwrap();
            directories.push(directory);
        }
        fs::write(directories[0].join("bin/user-file"), "keep").unwrap();
        fs::write(directories[1].join("outside-bin"), "keep").unwrap();
        let live = ExecutableLease::acquire(&directories[2].join("bin"), &leases).unwrap();
        for directory in &directories {
            fs::set_permissions(directory.join("bin"), fs::Permissions::from_mode(0o500)).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o500)).unwrap();
        }
        let expected = directories[3].canonicalize().unwrap();
        let removed = collect(&runs, &source, Some(&directories[6].join("bin")), &leases).unwrap();
        assert_eq!(removed, vec![expected]);
        for index in [0, 1, 2, 4, 5, 6] {
            assert!(directories[index].join("bin/tickloop").is_file());
            fs::set_permissions(&directories[index], fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(
                directories[index].join("bin"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        drop(live);
    }
}
