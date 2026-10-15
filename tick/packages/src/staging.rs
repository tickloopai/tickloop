//! Filling a staging directory from a package directory or a `.tar.gz` archive, and checking the
//! staged files before they are installed. Nothing is written through a symbolic link: links
//! are created after every file, and only below real directories.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::{PackageError, printable};

/// Package files keep their execute bits but are writable by their owner only, without setuid,
/// setgid or sticky bits.
const PACKAGE_FILE_MODE_MASK: u32 = 0o755;
const MEBIBYTE: u64 = 1024 * 1024;
/// Far more than a Tick program needs; stops an archive that would unpack without bound.
const ARCHIVE_UNPACKED_LIMIT: u64 = 512 * MEBIBYTE;

/// Copies the package directory `source` into the empty directory `destination`, keeping
/// symbolic links as links.
pub(crate) fn copy_directory(source: &Path, destination: &Path) -> Result<(), PackageError> {
    let mut pending = vec![PathBuf::new()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(source.join(&directory)).map_err(PackageError::io(format!(
            "cannot read {}",
            printable(source.join(&directory))
        )))?;
        for entry in entries {
            let entry = entry.map_err(PackageError::io(format!(
                "cannot read {}",
                printable(source.join(&directory))
            )))?;
            let relative = directory.join(entry.file_name());
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(PackageError::io(format!(
                "cannot read {}",
                printable(&path)
            )))?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                create_directories(destination, &relative)?;
                pending.push(relative);
            } else if file_type.is_file() {
                let mut file = File::open(&path).map_err(PackageError::io(format!(
                    "cannot read {}",
                    printable(&path)
                )))?;
                write_file(
                    destination,
                    &relative,
                    metadata.permissions().mode(),
                    &mut file,
                )?;
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path).map_err(PackageError::io(format!(
                    "cannot read {}",
                    printable(&path)
                )))?;
                create_link(destination, &relative, &target)?;
            } else {
                return Err(unsupported_file(&relative));
            }
        }
    }
    Ok(())
}

/// Unpacks the `.tar.gz` archive `archive` into the empty directory `destination`. Entries must
/// stay inside it and be directories, regular files or symbolic links.
pub(crate) fn unpack_archive(archive: File, destination: &Path) -> Result<(), PackageError> {
    let mut archive = tar::Archive::new(GzDecoder::new(BufReader::new(archive)));
    let mut links = Vec::new();
    let mut unpacked_bytes = 0_u64;
    for entry in archive.entries().map_err(unreadable_archive)? {
        let mut entry = entry.map_err(unreadable_archive)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() {
            continue;
        }
        let path = entry.path().map_err(unreadable_archive)?.into_owned();
        let Some(relative) = entry_path_in_package(&path)? else {
            continue;
        };
        match entry_type {
            EntryType::Directory => create_directories(destination, &relative)?,
            EntryType::Regular | EntryType::Continuous => {
                unpacked_bytes = unpacked_bytes.saturating_add(entry.size());
                if unpacked_bytes > ARCHIVE_UNPACKED_LIMIT {
                    return Err(PackageError::Invalid(format!(
                        "the archive unpacks to more than {} MiB",
                        ARCHIVE_UNPACKED_LIMIT / MEBIBYTE
                    )));
                }
                let mode = entry.header().mode().map_err(unreadable_archive)?;
                write_file(destination, &relative, mode, &mut entry)?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(unreadable_archive)?
                    .ok_or_else(|| {
                        PackageError::Invalid(format!(
                            "symbolic link {} in the archive has no target",
                            printable(&relative)
                        ))
                    })?
                    .into_owned();
                links.push((relative, target));
            }
            _ => return Err(unsupported_file(&relative)),
        }
    }
    for (relative, target) in links {
        create_link(destination, &relative, &target)?;
    }
    Ok(())
}

/// Checks that every entry under `root` is a directory, a regular file, or a symbolic link that
/// resolves to something inside `root`.
pub(crate) fn check_tree(root: &Path) -> Result<(), PackageError> {
    let root = root
        .canonicalize()
        .map_err(PackageError::io(format!("cannot open {}", printable(root))))?;
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(PackageError::io(format!(
            "cannot read {}",
            printable(&directory)
        )))?;
        for entry in entries {
            let entry = entry.map_err(PackageError::io(format!(
                "cannot read {}",
                printable(&directory)
            )))?;
            let path = entry.path();
            let relative = path.strip_prefix(&root).unwrap_or(&path);
            let file_type = entry.file_type().map_err(PackageError::io(format!(
                "cannot read {}",
                printable(&path)
            )))?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_symlink() {
                if !path
                    .canonicalize()
                    .is_ok_and(|target| target.starts_with(&root))
                {
                    return Err(PackageError::Invalid(format!(
                        "symbolic link {} must lead to a file or directory inside the package",
                        printable(relative)
                    )));
                }
            } else if !file_type.is_file() {
                return Err(unsupported_file(relative));
            }
        }
    }
    Ok(())
}

pub(crate) fn sha256_of(file: &mut File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    io::copy(file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// The path of an archive entry relative to the package root; `None` for the root itself.
fn entry_path_in_package(path: &Path) -> Result<Option<PathBuf>, PackageError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => relative.push(name),
            _ => {
                return Err(PackageError::Invalid(format!(
                    "archive entry {} lies outside the package",
                    printable(path)
                )));
            }
        }
    }
    Ok((!relative.as_os_str().is_empty()).then_some(relative))
}

/// Creates the directories of `relative` below `root` that do not exist yet, refusing a
/// component that is anything but a real directory.
fn create_directories(root: &Path, relative: &Path) -> Result<(), PackageError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(PackageError::Invalid(format!(
                    "{} in the package is not a directory",
                    printable(current.strip_prefix(root).unwrap_or(&current))
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(PackageError::io(format!(
                    "cannot create {}",
                    printable(&current)
                )))?;
            }
            Err(error) => {
                return Err(PackageError::io(format!(
                    "cannot read {}",
                    printable(&current)
                ))(error));
            }
        }
    }
    Ok(())
}

fn create_parent_directories(root: &Path, relative: &Path) -> Result<(), PackageError> {
    match relative.parent() {
        Some(parent) => create_directories(root, parent),
        None => Ok(()),
    }
}

fn write_file(
    root: &Path,
    relative: &Path,
    mode: u32,
    contents: &mut impl Read,
) -> Result<(), PackageError> {
    create_parent_directories(root, relative)?;
    let failed = || PackageError::io(format!("cannot unpack {}", printable(relative)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode & PACKAGE_FILE_MODE_MASK)
        .open(root.join(relative))
        .map_err(failed())?;
    io::copy(contents, &mut file).map_err(failed())?;
    Ok(())
}

fn create_link(root: &Path, relative: &Path, target: &Path) -> Result<(), PackageError> {
    create_parent_directories(root, relative)?;
    symlink(target, root.join(relative)).map_err(PackageError::io(format!(
        "cannot unpack {}",
        printable(relative)
    )))
}

fn unsupported_file(relative: &Path) -> PackageError {
    PackageError::Invalid(format!(
        "{} is neither a regular file, a directory nor a symbolic link",
        printable(relative)
    ))
}

fn unreadable_archive(error: io::Error) -> PackageError {
    PackageError::Invalid(format!("the archive is not a readable .tar.gz: {error}"))
}
