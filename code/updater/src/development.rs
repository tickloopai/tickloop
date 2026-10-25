use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result, bail};
use tickhost_connect::executable_lease::{ExecutableLease, lease_directory};

use crate::{Build, Installation, Source, bundle};

pub fn register_development(root: &Path, build: &Build) -> Result<ExecutableLease> {
    register(root, &std::env::current_exe()?, build, &lease_directory()?)
}

fn register(
    root: &Path,
    executable: &Path,
    build: &Build,
    leases: &Path,
) -> Result<ExecutableLease> {
    let root = root.canonicalize()?;
    let executable = executable.canonicalize()?;
    let bin = executable
        .parent()
        .context("Development executable has no directory")?;
    let run = bin
        .parent()
        .context("Development bundle has no directory")?;
    let runs = root.join("target/dev-runs").canonicalize()?;
    if bin.file_name().is_none_or(|name| name != "bin")
        || run.parent() != Some(runs.as_path())
        || run
            .file_name()
            .is_none_or(|name| !name.to_string_lossy().starts_with("run."))
        || executable.file_name().is_none_or(|name| name != "tickloop")
    {
        bail!("Development registration requires this checkout's immutable make dev bundle");
    }
    for directory in [run, bin] {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            bail!("Development runtime directory has unsafe ownership or permissions");
        }
    }
    let source = Source::Checkout { root };
    let lease = ExecutableLease::acquire(bin, leases)?;
    bundle::verify(bin, build)?;
    if bin.join("installation.json").exists() {
        let record = Installation::read(bin)?;
        if record.source != source
            || record.build.identity != build.identity
            || record.build.version != build.version
            || !record.pinned
        {
            bail!("Development runtime belongs to another build or checkout");
        }
    } else {
        Installation {
            source,
            build: build.clone(),
            pinned: true,
        }
        .write(bin)?;
    }
    fs::set_permissions(bin, fs::Permissions::from_mode(0o500))?;
    fs::set_permissions(run, fs::Permissions::from_mode(0o500))?;
    Ok(lease)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_registration_pins_complete_bundle_and_holds_its_reference() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("target/dev-runs/run.fixture");
        let bin = run.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let build = Build {
            version: "1.0.0".into(),
            identity: "1.0.0+1234567890abcdef".into(),
        };
        for name in crate::PROGRAMS {
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\ncase \"$1\" in\n--version) echo '{name} 1.0.0';;\n--build-identity) echo '{}';;\n*) exit 2;;\nesac\n", build.identity)).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
        }
        let leases = root.path().join("leases");
        let lease = register(root.path(), &bin.join("tickloop"), &build, &leases).unwrap();
        let record = Installation::read(&bin).unwrap();
        assert!(record.pinned);
        assert!(matches!(record.source, Source::Checkout { .. }));
        assert_eq!(fs::metadata(&bin).unwrap().mode() & 0o777, 0o500);
        assert!(
            ExecutableLease::try_retire(&bin, &leases)
                .unwrap()
                .is_none()
        );
        drop(lease);
        assert!(
            ExecutableLease::try_retire(&bin, &leases)
                .unwrap()
                .is_some()
        );
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn registration_refuses_a_program_outside_the_checkouts_dev_runs() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("target/dev-runs")).unwrap();
        let bin = root.path().join("unrelated/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("tickloop"), "unrelated").unwrap();
        let build = Build {
            version: "1.0.0".into(),
            identity: "fixture".into(),
        };
        assert!(
            register(
                root.path(),
                &bin.join("tickloop"),
                &build,
                &root.path().join("leases")
            )
            .is_err()
        );
        assert!(!bin.join("installation.json").exists());
    }
}
