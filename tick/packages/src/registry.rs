//! `installed.json`: which packages are installed, which version, from where and when.

use std::{collections::BTreeMap, fs, io, io::Write, path::Path};

use serde::{Deserialize, Serialize};

use crate::{PackageError, manifest::is_valid_version, validate_package_name};

pub const REGISTRY_FILE_NAME: &str = "installed.json";
/// The registry is replaced through a temporary file with this prefix beside it.
pub(crate) const TEMPORARY_PREFIX: &str = ".installed.json.";
const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Registry {
    version: u32,
    pub packages: BTreeMap<String, RegistryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RegistryEntry {
    pub version: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub installed_at: String,
}

impl Registry {
    /// Reads the registry at `path`; a missing one lists no packages.
    pub fn read(path: &Path) -> Result<Self, PackageError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: REGISTRY_VERSION,
                    packages: BTreeMap::new(),
                });
            }
            Err(error) => {
                return Err(PackageError::io(format!("cannot read {}", path.display()))(
                    error,
                ));
            }
        };
        let damaged = |reason: String| PackageError::Registry {
            path: path.to_path_buf(),
            reason,
        };
        let registry: Self =
            serde_json::from_slice(&bytes).map_err(|error| damaged(error.to_string()))?;
        if registry.version != REGISTRY_VERSION {
            return Err(damaged(format!(
                "version {} is not one this tickloop reads",
                registry.version
            )));
        }
        for (name, entry) in &registry.packages {
            if validate_package_name(name).is_err() {
                return Err(damaged(format!("{name:?} is not a Tick package name")));
            }
            if !is_valid_version(&entry.version) {
                return Err(damaged(format!(
                    "{:?} of {name} is not a version",
                    entry.version
                )));
            }
        }
        Ok(registry)
    }

    /// Replaces the registry at `path` at once, owner-only like other Tickloop state.
    pub fn write(&self, path: &Path) -> Result<(), PackageError> {
        let directory = path.parent().expect("the registry lives in a directory");
        let failed = || PackageError::io(format!("cannot write {}", path.display()));
        let mut contents = serde_json::to_vec_pretty(self).expect("the registry serializes");
        contents.push(b'\n');
        let mut temporary = tempfile::Builder::new()
            .prefix(TEMPORARY_PREFIX)
            .tempfile_in(directory)
            .map_err(failed())?;
        temporary.write_all(&contents).map_err(failed())?;
        temporary.as_file().sync_all().map_err(failed())?;
        temporary
            .persist(path)
            .map_err(|error| failed()(error.error))?;
        Ok(())
    }
}
