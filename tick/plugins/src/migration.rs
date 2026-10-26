//! Moving the Tick packages an earlier Tickloop installed in `ticks/` into the plugin store,
//! once: each becomes a plugin of the `tickloop` marketplace, or of `local` when it was installed
//! from a local directory or archive. Their credentials stay where they are, since a plugin keeps
//! its package's name.

use std::{
    fs,
    path::{Path, PathBuf},
};

use tick_packages::{BrokenPackage, PACKAGES_DIRECTORY, PackageStore};
use time::OffsetDateTime;

use crate::{
    LOCAL_MARKETPLACE, PluginError, PluginSource, TICKLOOP_MARKETPLACE,
    registry::{Registry, RegistryEntry},
    store::{LOCAL_REVISION, PluginStore, create_private_directories},
};

const MIGRATED_PREFIX: &str = "ticks.migrated-";
const DOWNLOAD_SCHEMES: [&str; 2] = ["https://", "http://"];

/// What moving the earlier Tick packages did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// The plugin ID of each package that moved.
    pub migrated: Vec<String>,
    /// The packages whose files could no longer be used, which stay behind.
    pub skipped: Vec<BrokenPackage>,
    /// Where the earlier packages directory now is.
    pub kept_at: PathBuf,
}

impl PluginStore {
    /// Moves the Tick packages an earlier Tickloop installed into this store, unless the store
    /// has a registry already; `None` when there was nothing to move.
    pub fn migrate(&self) -> Result<Option<Migration>, PluginError> {
        if !self.needs_migration() {
            return Ok(None);
        }
        let _lock = self.lock_directory()?;
        self.migrate_locked()
    }

    /// [`PluginStore::migrate`] for a caller that holds the directory's lock. When a package
    /// cannot be moved, the ones moved already go back and the earlier directory stays as it was.
    pub(crate) fn migrate_locked(&self) -> Result<Option<Migration>, PluginError> {
        if !self.needs_migration() {
            return Ok(None);
        }
        let old_directory = self.config_root.join(PACKAGES_DIRECTORY);
        let packages = PackageStore::in_config_root(&self.config_root).installed()?;
        let mut registry = Registry::empty();
        let mut moved = Vec::<(PathBuf, PathBuf)>::new();
        let restore = |moved: &[(PathBuf, PathBuf)]| {
            for (from, to) in moved.iter().rev() {
                let _ = fs::rename(to, from);
            }
        };
        let mut migrated = Vec::new();
        for package in packages.packages {
            let name = package.manifest.name.clone();
            let version = package.manifest.version.clone();
            let is_download = DOWNLOAD_SCHEMES
                .iter()
                .any(|scheme| package.source.starts_with(scheme));
            let (marketplace, revision, source) = if is_download {
                (
                    TICKLOOP_MARKETPLACE,
                    version.clone(),
                    PluginSource::Download {
                        url: package.source.clone(),
                    },
                )
            } else {
                (
                    LOCAL_MARKETPLACE,
                    LOCAL_REVISION.to_owned(),
                    PluginSource::Path {
                        path: package.source.clone(),
                    },
                )
            };
            let plugin_directory = self.plugin_directory(marketplace, &name);
            let target = plugin_directory.join(&revision);
            let moved_package = create_private_directories(&plugin_directory)
                .and_then(|()| move_package(&package.root, &target));
            if let Err(error) = moved_package {
                restore(&moved);
                return Err(error);
            }
            moved.push((package.root.clone(), target));
            migrated.push(format!("{name}@{marketplace}"));
            registry.plugins.insert(
                name,
                RegistryEntry {
                    marketplace: marketplace.to_owned(),
                    version,
                    revision,
                    source,
                    sha256: package.sha256,
                    installed_at: package.installed_at,
                    enabled: true,
                },
            );
        }
        if let Err(error) = registry.write(&self.registry_path()) {
            restore(&moved);
            return Err(error);
        }
        let kept_at = self.config_root.join(format!(
            "{MIGRATED_PREFIX}{}",
            OffsetDateTime::now_utc().unix_timestamp()
        ));
        let kept_at = match fs::rename(&old_directory, &kept_at) {
            Ok(()) => kept_at,
            // The plugins are registered already, so the earlier registry is never read again.
            Err(_) => old_directory,
        };
        Ok(Some(Migration {
            migrated,
            skipped: packages.broken,
            kept_at,
        }))
    }

    fn needs_migration(&self) -> bool {
        let old_registry = self
            .config_root
            .join(PACKAGES_DIRECTORY)
            .join(tick_packages::REGISTRY_FILE_NAME);
        old_registry.is_file() && !self.registry_path().exists()
    }
}

fn move_package(from: &Path, to: &Path) -> Result<(), PluginError> {
    fs::rename(from, to).map_err(PluginError::io(format!(
        "cannot move Tick package {} to {}",
        from.display(),
        to.display()
    )))
}
