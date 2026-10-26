//! Copying the Tick packages an earlier Tickloop installed in `ticks/` into the plugin store,
//! once: each becomes a plugin of the `tickloop` marketplace, or of `local` when it was installed
//! from a local directory or archive. Their credentials stay where they are, since a plugin keeps
//! its package's name.

use std::{fs, io::Write, path::PathBuf};

use tick_packages::{BrokenPackage, PACKAGES_DIRECTORY, PackageStore};
use time::OffsetDateTime;

use crate::{
    LOCAL_MARKETPLACE, PluginError, PluginSource, TICKLOOP_MARKETPLACE,
    registry::{Registry, RegistryEntry},
    store::{LOCAL_REVISION, PluginStore, create_private_directories},
};

const MIGRATION_NOTICE: &str = ".plugin-migration-notice";
const MIGRATED_PREFIX: &str = "ticks.migrated-";
const DOWNLOAD_SCHEMES: [&str; 2] = ["https://", "http://"];

/// What moving the earlier Tick packages did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// The plugin ID of each package that moved.
    pub migrated: Vec<String>,
    /// The damaged packages or names already held by plugins, which stay behind.
    pub skipped: Vec<BrokenPackage>,
    /// Where the earlier packages directory now is.
    pub kept_at: PathBuf,
}

impl PluginStore {
    /// Copies earlier Tick packages, publishes their registration, then preserves `ticks/`.
    /// Before publication, failures leave the earlier installation and registry unchanged.
    /// After publication, a retry finishes preserving the old directory without recopying.
    /// Returns `None` when there is no earlier registry.
    pub fn migrate(&self) -> Result<Option<Migration>, PluginError> {
        if !self.needs_migration() {
            return Ok(None);
        }
        let _lock = self.lock()?;
        if !self.needs_migration() {
            return Ok(None);
        }
        let old_directory = self.config_root.join(PACKAGES_DIRECTORY);
        let packages = PackageStore::in_config_root(&self.config_root).installed()?;
        let mut registry = Registry::read(&self.registry_path())?;
        let mut skipped = packages.broken;
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
            let entry = RegistryEntry {
                marketplace: marketplace.to_owned(),
                version: version.clone(),
                revision: revision.clone(),
                source,
                sha256: package.sha256,
                installed_at: package.installed_at,
                enabled: true,
            };
            if let Some(existing) = registry.plugins.get(&name) {
                if existing == &entry {
                    migrated.push(format!("{name}@{marketplace}"));
                } else {
                    skipped.push(BrokenPackage {
                        name,
                        version,
                        reason: format!(
                            "name already held by plugin from {}; left unchanged",
                            existing.marketplace
                        ),
                    });
                }
                continue;
            }
            let plugin_directory = self.plugin_directory(marketplace, &name);
            let target = plugin_directory.join(&revision);
            create_private_directories(&plugin_directory)?;
            let staging = self.staging()?;
            let copied = staging.path().join("package");
            create_private_directories(&copied)?;
            crate::staging::copy_directory(&package.root, &copied)?;
            crate::staging::check_tree(&copied)?;
            // Unregistered cache copies may be left by an interrupted migration.
            if fs::symlink_metadata(&target).is_ok() {
                fs::rename(&target, staging.path().join("leftover"))
                    .map_err(PluginError::io("cannot replace an earlier migration copy"))?;
            }
            fs::rename(&copied, &target)
                .map_err(PluginError::io("cannot publish a migrated Tick package"))?;
            migrated.push(format!("{name}@{marketplace}"));
            registry.plugins.insert(name, entry);
        }
        let kept_at = self.config_root.join(format!(
            "{MIGRATED_PREFIX}{}",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let migration = Migration {
            migrated,
            skipped,
            kept_at,
        };
        let notice_path = old_directory.join(MIGRATION_NOTICE);
        let save_notice = || PluginError::io("cannot save the Tick package migration notice");
        let mut notice = tempfile::Builder::new()
            .prefix(MIGRATION_NOTICE)
            .tempfile_in(&old_directory)
            .map_err(save_notice())?;
        notice
            .write_all(migration.notice().as_bytes())
            .map_err(save_notice())?;
        notice.as_file().sync_all().map_err(save_notice())?;
        notice
            .persist(&notice_path)
            .map_err(|error| save_notice()(error.error))?;
        registry.write(&self.registry_path())?;
        // The registry is the commit point. Until then every earlier package stays intact;
        // afterwards a restart recognizes its exact entry and only finishes retirement.
        fs::rename(&old_directory, &migration.kept_at)
            .map_err(PluginError::io("cannot preserve earlier Tick packages"))?;
        Ok(Some(migration))
    }

    pub(crate) fn migrate_on_use(&self) -> Result<(), PluginError> {
        self.migrate().map(|_| ())
    }

    /// Takes the unread migration notices for a foreground CLI, including migrations performed
    /// earlier by a background worker. The earlier package directories and files stay in place.
    pub fn take_migration_notices(&self) -> Result<Vec<String>, PluginError> {
        self.migrate()?;
        let entries = match fs::read_dir(&self.config_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PluginError::io("cannot read migration notices")(error)),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(PluginError::io("cannot read migration notices"))?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(MIGRATED_PREFIX)
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                paths.push(entry.path().join(MIGRATION_NOTICE));
            }
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        paths.sort();
        let _lock = self.lock()?;
        let mut notices = Vec::new();
        for path in paths {
            let notice = match fs::read_to_string(&path) {
                Ok(notice) => notice,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(PluginError::io("cannot read migration notice")(error)),
            };
            fs::remove_file(&path)
                .map_err(PluginError::io("cannot acknowledge migration notice"))?;
            notices.push(notice);
        }
        Ok(notices)
    }

    fn needs_migration(&self) -> bool {
        let old_registry = self
            .config_root
            .join(PACKAGES_DIRECTORY)
            .join(tick_packages::REGISTRY_FILE_NAME);
        old_registry.is_file()
    }
}

impl Migration {
    fn notice(&self) -> String {
        let mut notice = format!(
            "Migrated Tick packages to plugins: {}. Earlier files kept in {}.\n",
            self.migrated.join(", "),
            crate::printable(&self.kept_at)
        );
        for skipped in &self.skipped {
            notice.push_str(&format!(
                "Skipped Tick package {}: {}\n",
                crate::printable(&skipped.name),
                crate::printable(&skipped.reason)
            ));
        }
        notice
    }
}
