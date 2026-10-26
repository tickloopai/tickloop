//! `installed.json`: which plugins are installed, from which marketplace and source, which
//! version, since when, and whether they are enabled.

use std::{collections::BTreeMap, fs, io, io::Write, path::Path};

use serde::{Deserialize, Serialize};

use crate::{PluginError, manifest::is_valid_version, validate_plugin_name};

pub const REGISTRY_FILE_NAME: &str = "installed.json";
/// The registry is replaced through a temporary file with this prefix beside it.
pub(crate) const TEMPORARY_PREFIX: &str = ".installed.json.";
const REGISTRY_VERSION: u32 = 1;

/// Where an installed plugin came from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginSource {
    /// A local plugin directory or archive.
    Path { path: String },
    /// A Git repository at the verified commit, optionally restricted to a subdirectory.
    Git {
        source: crate::GitSource,
        commit: String,
    },
    /// An archive downloaded from a marketplace.
    Download { url: String },
}

impl std::fmt::Display for PluginSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path { path } => formatter.write_str(path),
            Self::Git { source, .. } => formatter.write_str(&source.url),
            Self::Download { url } => formatter.write_str(url),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Registry {
    version: u32,
    pub plugins: BTreeMap<String, RegistryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RegistryEntry {
    pub marketplace: String,
    /// The version shown for the plugin.
    pub version: String,
    /// What tells installed copies apart: the version of a marketplace archive, `local` for a
    /// local install. It names the plugin's directory in the cache.
    pub revision: String,
    pub source: PluginSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub installed_at: String,
    pub enabled: bool,
}

impl Registry {
    pub fn empty() -> Self {
        Self {
            version: REGISTRY_VERSION,
            plugins: BTreeMap::new(),
        }
    }

    /// Reads the registry at `path`; a missing one lists no plugins.
    pub fn read(path: &Path) -> Result<Self, PluginError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(error) => {
                return Err(PluginError::io(format!("cannot read {}", path.display()))(
                    error,
                ));
            }
        };
        let damaged = |reason: String| PluginError::Registry {
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
        for (name, entry) in &registry.plugins {
            if validate_plugin_name(name).is_err() {
                return Err(damaged(format!("{name:?} is not a plugin name")));
            }
            if validate_plugin_name(&entry.marketplace).is_err() {
                return Err(damaged(format!(
                    "{:?} of {name} is not a marketplace name",
                    entry.marketplace
                )));
            }
            if !is_valid_version(&entry.version) || !is_valid_version(&entry.revision) {
                return Err(damaged(format!("the version of {name} is not one")));
            }
        }
        Ok(registry)
    }

    /// Replaces the registry at `path` at once, owner-only like other Tickloop state.
    pub fn write(&self, path: &Path) -> Result<(), PluginError> {
        let directory = path.parent().expect("the registry lives in a directory");
        let failed = || PluginError::io(format!("cannot write {}", path.display()));
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
