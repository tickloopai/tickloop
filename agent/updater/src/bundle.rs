use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::{Build, Installation, Lock, PROGRAMS, Source, data_root, private_directory, release};

const MAX_UNPACKED_BYTES: u64 = 768 * 1024 * 1024;

pub(crate) fn remove_staging(root: &Path, prefixes: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir()
            && prefixes
                .iter()
                .any(|prefix| entry.file_name().to_string_lossy().starts_with(prefix))
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

pub(crate) fn read_build(directory: &Path) -> Result<Build> {
    let version = probe(&directory.join("tickloop"), "--version")?;
    let version = version
        .strip_prefix("tickloop ")
        .context("Invalid Tickloop version output")?
        .to_string();
    semver::Version::parse(&version)?;
    let identity = probe(&directory.join("tickloop"), "--build-identity")?;
    if !identity.starts_with(&format!("{version}+"))
        || identity.contains('/')
        || identity.contains("..")
    {
        bail!("Invalid Tickloop build identity");
    }
    Ok(Build { version, identity })
}

fn probe(program: &Path, argument: &str) -> Result<String> {
    // Release programs are trusted only after their archive checksum has been verified.
    let mut child = std::process::Command::new(program)
        .arg(argument)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("Cannot check {}", program.display()))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("{} {argument} failed", program.display());
            }
            let mut output = String::new();
            child
                .stdout
                .take()
                .context("missing probe output")?
                .take(4096)
                .read_to_string(&mut output)?;
            return Ok(output.trim().into());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Timed out checking {}", program.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub(crate) fn verify(directory: &Path, build: &Build) -> Result<()> {
    for program in PROGRAMS {
        let path = directory.join(program);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o111 == 0
        {
            bail!("Invalid executable {program}");
        }
        if probe(&path, "--version")? != format!("{program} {}", build.version)
            || probe(&path, "--build-identity")? != build.identity
        {
            bail!(
                "{program} does not belong to the same Tickloop build; reinstall both programs together"
            );
        }
    }
    Ok(())
}

pub(crate) fn copy_programs(from: &Path, to: &Path) -> Result<()> {
    for program in PROGRAMS {
        let path = from.join(program);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("Expected a regular program at {}", path.display());
        }
        std::fs::copy(&path, to.join(program))?;
        std::fs::set_permissions(to.join(program), std::fs::Permissions::from_mode(0o500))?;
    }
    Ok(())
}

pub(crate) fn unpack(bytes: &[u8], checksum: &str, directory: &Path) -> Result<()> {
    if format!("{:x}", Sha256::digest(bytes)) != checksum {
        bail!("Update archive SHA-256 mismatch");
    }
    let mut archive = tar::Archive::new(GzDecoder::new(Cursor::new(bytes)));
    let mut found = HashSet::new();
    let mut total = 0u64;
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut components = path.components();
        let Some(Component::Normal(name)) = components.next() else {
            bail!("Unsafe archive path");
        };
        if components.next().is_some() {
            bail!("Unexpected nested archive path");
        }
        let name = name.to_str().context("Invalid archive filename")?;
        if !PROGRAMS.contains(&name)
            || !entry.header().entry_type().is_file()
            || !found.insert(name.to_owned())
        {
            bail!("Unexpected, linked or duplicate update archive entry: {name}");
        }
        total = total
            .checked_add(entry.size())
            .context("Archive size overflow")?;
        if total > MAX_UNPACKED_BYTES {
            bail!("Unpacked update exceeds size limit");
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(name))?;
        std::io::copy(&mut entry, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        output.set_permissions(std::fs::Permissions::from_mode(0o500))?;
    }
    if found.len() != PROGRAMS.len() {
        bail!("Update archive is missing a program");
    }
    Ok(())
}

pub(crate) fn activate(root: &Path, directory: &Path, build: &Build) -> Result<()> {
    activate_source(
        root,
        directory,
        build,
        Source::Native {
            root: root.to_owned(),
        },
    )
}

pub(crate) fn activate_source(
    root: &Path,
    directory: &Path,
    build: &Build,
    source: Source,
) -> Result<()> {
    let versions = root.join("versions");
    private_directory(&versions)?;
    let destination = versions.join(&build.version);
    if destination.exists() {
        let previous = Installation::read(&destination)?;
        if previous.build.identity != build.identity || previous.source != source {
            bail!("This release version already exists with another build");
        }
        for program in PROGRAMS {
            if Sha256::digest(std::fs::read(destination.join(program))?)
                != Sha256::digest(std::fs::read(directory.join(program))?)
            {
                bail!("An installed release cannot be replaced with different content");
            }
        }
    } else {
        Installation {
            source,
            build: build.clone(),
            pinned: false,
        }
        .write(directory)?;
        std::fs::rename(directory, &destination)?;
    }
    std::fs::File::open(&versions)?.sync_all()?;
    let temporary = tempfile::Builder::new()
        .prefix(".activate-")
        .tempdir_in(root)?;
    let pointer = temporary.path().join("current");
    symlink(Path::new("versions").join(&build.version), &pointer)?;
    std::fs::rename(pointer, root.join("current"))?;
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

pub(crate) async fn download_install(
    root: &Path,
    candidate: &release::Candidate,
    source: Source,
    progress: &mut crate::ProgressReporter<'_>,
) -> Result<()> {
    progress.stage(crate::UpdateStage::Downloading)?;
    let name = candidate
        .archive
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .context("Archive URL has no filename")?;
    let checksum = release::checksum(&release::get(&candidate.checksum, 4096).await?, name)?;
    let bytes = release::get_with_progress(
        &candidate.archive,
        release::MAX_ARCHIVE_BYTES,
        |downloaded, total| progress.report(crate::UpdateProgress::download(downloaded, total)),
    )
    .await?;
    progress.stage(crate::UpdateStage::Verifying)?;
    let stage = tempfile::Builder::new()
        .prefix(".download-")
        .tempdir_in(root)?;
    let root = root.to_owned();
    let version = candidate.version.to_string();
    let (stage, build) = tokio::task::spawn_blocking(move || -> Result<_> {
        unpack(&bytes, &checksum, stage.path())?;
        let build = read_build(stage.path())?;
        if build.version != version {
            bail!("Archive reports another release version");
        }
        verify(stage.path(), &build)?;
        Ok((stage, build))
    })
    .await??;
    progress.stage(crate::UpdateStage::Installing)?;
    tokio::task::spawn_blocking(move || activate_source(&root, stage.path(), &build, source))
        .await??;
    Ok(())
}

pub(crate) async fn install_native(build: Build) -> Result<()> {
    let root = data_root()?;
    private_directory(&root)?;
    let _lock = Lock::acquire(&root.join("install.lock"))?;
    remove_staging(&root, &[".install-", ".download-", ".activate-"])?;
    let bin = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
        .join(".local/bin");
    std::fs::create_dir_all(&bin)?;
    let links = [
        ("tickloop", root.join("current/tickloop")),
        ("tl", std::path::PathBuf::from("tickloop")),
        ("tickhost", root.join("current/tickhost")),
    ];
    for (name, target) in &links {
        let path = bin.join(name);
        if std::fs::symlink_metadata(&path).is_ok()
            && std::fs::read_link(&path).ok().as_ref() != Some(target)
        {
            bail!(
                "Installation would replace {}. Remove the conflicting installation explicitly first.",
                path.display()
            );
        }
    }
    if let Ok(current) = Installation::read(&root.join("current"))
        && semver::Version::parse(&current.build.version)? > semver::Version::parse(&build.version)?
    {
        bail!("The installer will not downgrade the current installation");
    }
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(&root)?;
    let executable = std::env::current_exe()?.canonicalize()?;
    copy_programs(
        executable.parent().context("executable has no parent")?,
        stage.path(),
    )?;
    verify(stage.path(), &build)?;
    activate(&root, stage.path(), &build)?;
    for (name, target) in links {
        let path = bin.join(name);
        if std::fs::symlink_metadata(&path).is_err() {
            symlink(target, path)?;
        }
    }
    println!(
        "Installed Tickloop {} in {}.\nEnsure {} is on PATH.",
        build.version,
        root.display(),
        bin.display()
    );
    Ok(())
}

pub(crate) fn install_registry(build: Build, launcher: &Path) -> Result<()> {
    let launcher = launcher
        .canonicalize()
        .context("Cannot find the Cargo launcher")?;
    let (root, registered) = crate::source::registry_runtime(&launcher)?;
    if registered.to_string() != build.version {
        bail!("The downloaded application does not match the Cargo launcher version");
    }
    let source = Source::CargoRegistry {
        root: root.clone(),
        launcher,
    };
    private_directory(&root)?;
    let _lock = Lock::acquire(&root.join("install.lock"))?;
    remove_staging(&root, &[".install-", ".download-", ".activate-"])?;
    if let Ok(current) = Installation::read(&root.join("current")) {
        if current.source != source {
            bail!("The Cargo runtime belongs to another installation");
        }
        if semver::Version::parse(&current.build.version)? > semver::Version::parse(&build.version)?
        {
            return Ok(());
        }
    }
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(&root)?;
    let executable = std::env::current_exe()?.canonicalize()?;
    copy_programs(
        executable.parent().context("executable has no parent")?,
        stage.path(),
    )?;
    verify(stage.path(), &build)?;
    activate_source(&root, stage.path(), &build, source)?;
    println!(
        "Installed Tickloop {} for the Cargo launcher.",
        build.version
    );
    Ok(())
}
