use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use tick_packages::PackageStore;

/// A config root and a place for package sources, both in one temporary directory.
pub struct Fixture {
    directory: tempfile::TempDir,
    pub store: PackageStore,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = PackageStore::in_config_root(&directory.path().join("config"));
        Self { directory, store }
    }

    pub fn root(&self) -> PathBuf {
        self.directory.path().canonicalize().unwrap()
    }

    /// A valid package directory with a program, a Skill and the given manifest.
    pub fn package_with(&self, label: &str, manifest: &Value) -> PathBuf {
        let directory = self.root().join("sources").join(label);
        let name = manifest["name"].as_str().unwrap_or("sample");
        fs::create_dir_all(directory.join("bin")).unwrap();
        fs::create_dir_all(directory.join("skills").join(name)).unwrap();
        fs::write(
            directory.join(MANIFEST),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
        let program = directory.join("bin").join(format!("tick-{name}"));
        fs::write(&program, format!("#!/bin/sh\necho {label}\n")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            directory.join("skills").join(name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Opens {name}\n---\n"),
        )
        .unwrap();
        directory
    }

    pub fn package(&self, label: &str, version: &str) -> PathBuf {
        self.package_with(label, &manifest("sample", version))
    }

    pub fn packages_directory(&self) -> &Path {
        self.store.directory()
    }

    /// Every entry under the packages directory with its kind, mode and contents.
    pub fn snapshot(&self) -> BTreeMap<PathBuf, String> {
        let mut entries = BTreeMap::new();
        let root = self.packages_directory();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let Ok(children) = fs::read_dir(&directory) else {
                continue;
            };
            for child in children {
                let path = child.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                let mode = metadata.permissions().mode();
                let description = if metadata.is_dir() {
                    pending.push(path);
                    format!("directory {mode:o}")
                } else if metadata.file_type().is_symlink() {
                    format!("link to {}", fs::read_link(&path).unwrap().display())
                } else {
                    format!(
                        "file {mode:o} {}",
                        String::from_utf8_lossy(&fs::read(&path).unwrap())
                    )
                };
                entries.insert(relative, description);
            }
        }
        entries
    }

    /// The packages directory holds only its lock, its registry and package directories.
    pub fn assert_no_leftovers(&self) {
        let Ok(entries) = fs::read_dir(self.packages_directory()) else {
            return;
        };
        for entry in entries {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                name == ".lock" || name == "installed.json" || !name.starts_with('.'),
                "left behind {name}"
            );
        }
    }

    pub fn registry(&self) -> Value {
        serde_json::from_slice(&fs::read(self.packages_directory().join("installed.json")).unwrap())
            .unwrap()
    }
}

pub const MANIFEST: &str = "tick-package.json";

pub fn manifest(name: &str, version: &str) -> Value {
    json!({
        "name": name,
        "version": version,
        "description": format!("The {name} sample"),
        "executable": format!("bin/tick-{name}"),
        "protocol": true,
    })
}

/// Packs the contents of `directory` as the root of a `.tar.gz`, keeping links as links.
pub fn archive_of(directory: &Path, archive: &Path) {
    let mut builder = tar::Builder::new(GzEncoder::new(
        fs::File::create(archive).unwrap(),
        Compression::default(),
    ));
    builder.follow_symlinks(false);
    builder.append_dir_all(".", directory).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
}

/// One raw archive entry, written without the path checks of [`tar::Builder`].
#[derive(Clone)]
pub struct RawEntry<'a> {
    pub path: &'a str,
    pub entry_type: tar::EntryType,
    pub link: Option<&'a str>,
    pub data: &'a [u8],
    pub mode: u32,
}

impl<'a> RawEntry<'a> {
    pub fn file(path: &'a str, data: &'a [u8], mode: u32) -> Self {
        Self {
            path,
            entry_type: tar::EntryType::Regular,
            link: None,
            data,
            mode,
        }
    }

    pub fn link(path: &'a str, entry_type: tar::EntryType, target: &'a str) -> Self {
        Self {
            path,
            entry_type,
            link: Some(target),
            data: b"",
            mode: 0o777,
        }
    }
}

pub fn raw_archive(archive: &Path, entries: &[RawEntry]) {
    let mut builder = tar::Builder::new(GzEncoder::new(
        fs::File::create(archive).unwrap(),
        Compression::default(),
    ));
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name[..entry.path.len()].copy_from_slice(entry.path.as_bytes());
        if let Some(link) = entry.link {
            gnu.linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        header.set_entry_type(entry.entry_type);
        header.set_size(entry.data.len() as u64);
        header.set_mode(entry.mode);
        header.set_cksum();
        builder.append(&header, entry.data).unwrap();
    }
    builder
        .into_inner()
        .unwrap()
        .finish()
        .unwrap()
        .flush()
        .unwrap();
}

/// The entries of a valid `sample` package at `version`, as raw archive entries.
pub fn sample_entries(manifest_json: &[u8]) -> Vec<RawEntry<'_>> {
    vec![
        RawEntry::file(MANIFEST, manifest_json, 0o644),
        RawEntry::file("bin/tick-sample", b"#!/bin/sh\n", 0o755),
    ]
}

pub fn link(target: impl AsRef<Path>, path: impl AsRef<Path>) {
    symlink(target, path).unwrap();
}
