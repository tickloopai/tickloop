//! The plugins installed under one Tickloop config root: each lives in
//! `plugins/cache/<marketplace>/<plugin>/<revision>/`, keeps its data in `plugins/data/<plugin>/`
//! and is registered in `plugins/installed.json`. Installs, uninstalls and changes hold a lock on
//! the directory; a failed one leaves the previous plugin and the registry as they were.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Seek, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};
use tick_packages::{LoginState, PackageCredentials};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    LOCAL_MARKETPLACE, PLUGINS_DIRECTORY, PluginError, PluginManifest, PluginSource, printable,
    registry::{self, REGISTRY_FILE_NAME, Registry, RegistryEntry},
    skill_directories, staging, validate_plugin_name,
};

pub(crate) const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const LOCK_FILE_NAME: &str = ".lock";
/// Every staging directory starts with it; a name never does, so it cannot shadow a plugin.
const STAGING_PREFIX: &str = ".staging-";
/// Every download file starts with it, so it cannot shadow a plugin either.
const DOWNLOAD_PREFIX: &str = ".download-";
const STAGED_PLUGIN: &str = "plugin";
const DISPLACED_PLUGIN: &str = "displaced";
const PREVIOUS_REVISION: &str = "previous";
pub(crate) const CACHE_DIRECTORY: &str = "cache";
const DATA_DIRECTORY: &str = "data";
/// The revision of a plugin installed from a local directory or archive, which is replaced on
/// every install since a local build need not change its version.
pub(crate) const LOCAL_REVISION: &str = "local";
const MANIFEST_DIRECTORY_MODE: u32 = 0o755;

/// A plugin as installed: its manifest, where it is, and how it was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    /// The absolute plugin directory.
    pub root: PathBuf,
    pub marketplace: String,
    /// The version shown for the plugin.
    pub version: String,
    /// What tells installed copies apart, such as a marketplace version or `local`.
    pub revision: String,
    pub source: PluginSource,
    /// The SHA-256 of the archive it was installed from.
    pub sha256: Option<String>,
    /// When it was installed, in RFC 3339.
    pub installed_at: String,
    pub enabled: bool,
}

impl InstalledPlugin {
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// `<plugin>@<marketplace>`.
    pub fn id(&self) -> String {
        format!("{}@{}", self.manifest.name, self.marketplace)
    }

    /// The absolute path of the plugin's Tick program, if it has one.
    pub fn executable(&self) -> Option<PathBuf> {
        self.manifest
            .tick
            .as_ref()
            .map(|tick| self.root.join(&tick.executable))
    }

    /// The absolute directory of every Skill the plugin carries.
    pub fn skill_directories(&self) -> Vec<PathBuf> {
        skill_directories(&self.root, &self.manifest.skill_paths)
    }
}

/// A registered plugin whose files can no longer be used, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokenPlugin {
    pub name: String,
    pub marketplace: String,
    /// The version the registry recorded.
    pub version: String,
    pub enabled: bool,
    pub reason: String,
}

/// The registered plugins, each list sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstalledPlugins {
    pub plugins: Vec<InstalledPlugin>,
    pub broken: Vec<BrokenPlugin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    Replaced {
        previous_version: String,
    },
    /// The installed plugin of the same version was replaced.
    Reinstalled,
    /// The same version was already installed; nothing changed.
    AlreadyInstalled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    /// The plugin now installed; for [`InstallOutcome::AlreadyInstalled`], the one that was.
    pub plugin: InstalledPlugin,
    pub outcome: InstallOutcome,
}

/// What a marketplace lists for a downloaded archive; [`PluginStore::install_archive`] installs
/// the archive only when it matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedArchive<'a> {
    pub marketplace: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
    /// Where it was downloaded from, recorded as the plugin's source.
    pub url: &'a str,
}

/// A private file in the plugins directory to download an archive into before
/// [`PluginStore::install_archive`] installs it. It is removed when dropped. Its process holds a
/// lock on it until then, so the next install or uninstall removes one whose process ended
/// mid-download, and only such a one.
#[derive(Debug)]
pub struct DownloadFile(tempfile::NamedTempFile);

impl DownloadFile {
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Write for DownloadFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uninstalled {
    pub name: String,
    pub marketplace: String,
    pub version: String,
    /// The directory of the plugin's data source credentials, which stay, when there is one.
    pub credentials: Option<PathBuf>,
    /// The plugin's data directory when it stays.
    pub data: Option<PathBuf>,
}

/// The plugins installed under one Tickloop config root, and the credentials kept for them
/// there. The Tick packages an earlier Tickloop installed move into it only through
/// [`PluginStore::migrate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginStore {
    pub(crate) config_root: PathBuf,
    pub(crate) directory: PathBuf,
}

impl PluginStore {
    pub fn in_config_root(config_root: &Path) -> Self {
        Self {
            config_root: config_root.to_path_buf(),
            directory: config_root.join(PLUGINS_DIRECTORY),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn config_root(&self) -> &Path {
        &self.config_root
    }

    /// Where the credentials of the plugin `name` are kept, whether or not it is installed.
    pub fn credentials(&self, name: &str) -> Result<PackageCredentials, PluginError> {
        Ok(PackageCredentials::in_config_root(&self.config_root, name)?)
    }

    /// The login state of `plugin`, from its saved credentials alone.
    pub fn login_state(&self, plugin: &InstalledPlugin) -> LoginState {
        match self.credentials(plugin.name()) {
            Ok(credentials) => credentials.login_state(&plugin.manifest.auth),
            Err(_) => LoginState::Unknown,
        }
    }

    /// Where the plugin `name` keeps its data, whether or not it exists yet.
    pub fn data_directory(&self, name: &str) -> PathBuf {
        self.directory.join(DATA_DIRECTORY).join(name)
    }

    /// The plugin's data directory, created owner-only when it does not exist yet.
    pub fn create_data_directory(&self, name: &str) -> Result<PathBuf, PluginError> {
        validate_plugin_name(name)?;
        let directory = self.data_directory(name);
        create_private_directories(&directory)?;
        Ok(directory)
    }

    /// Lists the registered plugins, setting apart those whose files are missing or no longer
    /// form a valid plugin. One that looks broken while an install or uninstall is under way is
    /// read again once that has finished.
    pub fn installed(&self) -> Result<InstalledPlugins, PluginError> {
        let installed = self.read_installed()?;
        if installed.broken.is_empty() {
            return Ok(installed);
        }
        let _settled = self.settled()?;
        self.read_installed()
    }

    /// The installed plugin `name`, enabled or not. One that looks broken while an install or
    /// uninstall is under way is read again once that has finished.
    pub fn get(&self, name: &str) -> Result<InstalledPlugin, PluginError> {
        validate_plugin_name(name)?;
        let read = || {
            let registry = Registry::read(&self.registry_path())?;
            let entry = registry
                .plugins
                .get(name)
                .ok_or_else(|| PluginError::NotInstalled(name.to_owned()))?;
            self.load_installed(name, entry)
                .map_err(|reason| PluginError::Unusable {
                    name: name.to_owned(),
                    reason,
                })
        };
        match read() {
            Err(PluginError::Unusable { .. }) => {
                let _settled = self.settled()?;
                read()
            }
            read => read,
        }
    }

    /// Installs the plugin directory or `.tar.gz` archive at `path` from the `local`
    /// marketplace, recording its path as the source. It replaces the installed plugin of its
    /// name even at the same version, since a local build need not change the version.
    pub fn install_path(&self, path: &Path) -> Result<Installation, PluginError> {
        let source = path
            .canonicalize()
            .map_err(PluginError::io(format!("cannot open {}", path.display())))?;
        let request = |sha256: Option<String>| Request {
            marketplace: LOCAL_MARKETPLACE,
            source: PluginSource::Path {
                path: source.to_string_lossy().into_owned(),
            },
            revision: LOCAL_REVISION.to_owned(),
            sha256,
            default_name: default_name_of(&source),
            entry: None,
            expected: None,
            same_revision: SameRevision::Replace,
        };
        if source.is_file() {
            let (archive, sha256) = open_archive(&source, None)?;
            return self.install(request(Some(sha256)), |staged| {
                staging::unpack_archive(archive, staged)
            });
        }
        if !source.is_dir() {
            return Err(PluginError::Invalid(format!(
                "{} is neither a plugin directory nor an archive",
                source.display()
            )));
        }
        self.install(request(None), |staged| {
            if canonical(&self.directory)?.starts_with(&source) {
                return Err(PluginError::Invalid(format!(
                    "{} contains the plugins directory",
                    source.display()
                )));
            }
            staging::copy_directory(&source, staged)
        })
    }

    /// Installs the plugin in the `.tar.gz` archive at `archive`, whose root holds the plugin
    /// directory's contents, when it matches `expected`: its SHA-256 is checked before it is
    /// unpacked, its name and version before it is installed. An installed plugin of the same
    /// version from the same marketplace is kept as it is.
    pub fn install_archive(
        &self,
        archive: &Path,
        expected: ExpectedArchive<'_>,
    ) -> Result<Installation, PluginError> {
        validate_plugin_name(expected.marketplace)?;
        let (opened, sha256) = open_archive(archive, Some(expected.sha256))?;
        let request = Request {
            marketplace: expected.marketplace,
            source: PluginSource::Download {
                url: expected.url.to_owned(),
            },
            revision: expected.version.to_owned(),
            sha256: Some(sha256),
            default_name: expected.name.to_owned(),
            entry: None,
            expected: Some((expected.name, expected.version)),
            same_revision: SameRevision::Keep,
        };
        self.install(request, |staged| staging::unpack_archive(opened, staged))
    }

    /// A new, empty [`DownloadFile`], creating the plugins directory when needed.
    pub fn download_file(&self) -> Result<DownloadFile, PluginError> {
        // Under the directory's lock, so no install or uninstall takes the new file for one left
        // behind before this process locks it.
        let _lock = self.lock()?;
        let file = tempfile::Builder::new()
            .prefix(DOWNLOAD_PREFIX)
            .tempfile_in(&self.directory)
            .map_err(PluginError::io(format!(
                "cannot create a download file in {}",
                self.directory.display()
            )))?;
        file.as_file().lock().map_err(PluginError::io(format!(
            "cannot lock {}",
            file.path().display()
        )))?;
        Ok(DownloadFile(file))
    }

    /// Removes the plugin and its registration, and its data unless `keep_data`. Its data source
    /// credentials stay.
    pub fn uninstall(&self, name: &str, keep_data: bool) -> Result<Uninstalled, PluginError> {
        validate_plugin_name(name)?;
        let _lock = self.lock()?;
        let registry_path = self.registry_path();
        let mut registry = Registry::read(&registry_path)?;
        let entry = registry
            .plugins
            .remove(name)
            .ok_or_else(|| PluginError::NotInstalled(name.to_owned()))?;
        let installed = self.plugin_directory(&entry.marketplace, name);
        let staging = self.staging()?;
        let displaced = staging.path().join(DISPLACED_PLUGIN);
        let had_files = move_aside(&installed, &displaced)?;
        if let Err(error) = registry.write(&registry_path) {
            if had_files {
                let _ = fs::rename(&displaced, &installed);
            }
            return Err(error);
        }
        let data = self.data_directory(name);
        if !keep_data {
            let _ = fs::remove_dir_all(&data);
        }
        let credentials = self.credentials(name)?.directory().to_path_buf();
        Ok(Uninstalled {
            name: name.to_owned(),
            marketplace: entry.marketplace,
            version: entry.version,
            credentials: credentials.exists().then_some(credentials),
            data: data.exists().then_some(data),
        })
    }

    /// Enables or disables the plugin `name`; a disabled plugin stays installed, but its Skills
    /// are not loaded and its Tick program is not found by name.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<InstalledPlugin, PluginError> {
        validate_plugin_name(name)?;
        let _lock = self.lock()?;
        let registry_path = self.registry_path();
        let mut registry = Registry::read(&registry_path)?;
        let entry = registry
            .plugins
            .get_mut(name)
            .ok_or_else(|| PluginError::NotInstalled(name.to_owned()))?;
        if entry.enabled != enabled {
            entry.enabled = enabled;
            registry.write(&registry_path)?;
        }
        let entry = &registry.plugins[name];
        self.load_installed(name, entry)
            .map_err(|reason| PluginError::Unusable {
                name: name.to_owned(),
                reason,
            })
    }

    /// Builds the plugin in a staging directory with `fill`, checks it, and installs it in the
    /// cache: the staged directory is renamed into place, then the registry is replaced, then the
    /// previous revision is removed; when a step fails, the cache and registry go back.
    fn install(
        &self,
        request: Request<'_>,
        fill: impl FnOnce(&Path) -> Result<(), PluginError>,
    ) -> Result<Installation, PluginError> {
        let _lock = self.lock()?;
        let staging = self.staging()?;
        let staged = staging.path().join(STAGED_PLUGIN);
        fs::create_dir(&staged).map_err(PluginError::io(format!(
            "cannot create {}",
            staged.display()
        )))?;
        fill(&staged)?;
        staging::check_tree(&staged)?;
        let manifest = PluginManifest::load(&staged, &request.default_name, request.entry)?;
        if let Some(document) = manifest.written_manifest(request.entry) {
            write_manifest(&staged, &document)?;
        }
        if let Some((name, version)) = request.expected {
            if manifest.name != name {
                return Err(PluginError::Invalid(format!(
                    "the plugin is named {}, not {}",
                    manifest.name,
                    printable(name)
                )));
            }
            if manifest.version.as_deref() != Some(version) {
                return Err(PluginError::Invalid(format!(
                    "the plugin is version {}, not {}",
                    manifest.version.as_deref().unwrap_or("unknown"),
                    printable(version)
                )));
            }
        }

        let name = manifest.name.clone();
        let registry_path = self.registry_path();
        let mut registry = Registry::read(&registry_path)?;
        let previous = registry.plugins.get(&name).cloned();
        if let Some(previous) = &previous {
            if previous.marketplace != request.marketplace {
                return Err(PluginError::NameTaken {
                    name,
                    marketplace: previous.marketplace.clone(),
                });
            }
            if request.same_revision == SameRevision::Keep
                && previous.revision == request.revision
                && let Ok(installed) = self.load_installed(&name, previous)
            {
                return Ok(Installation {
                    plugin: installed,
                    outcome: InstallOutcome::AlreadyInstalled,
                });
            }
        }

        let plugin_directory = self.plugin_directory(request.marketplace, &name);
        create_private_directories(&plugin_directory)?;
        let target = plugin_directory.join(&request.revision);
        let displaced = staging.path().join(DISPLACED_PLUGIN);
        let had_files = move_aside(&target, &displaced)?;
        let restore = |installed: bool| {
            if installed {
                let _ = fs::rename(&target, &staged);
            }
            if had_files {
                let _ = fs::rename(&displaced, &target);
            }
        };
        if let Err(error) = fs::rename(&staged, &target) {
            restore(false);
            return Err(PluginError::io(format!(
                "cannot install {}",
                target.display()
            ))(error));
        }
        let entry = RegistryEntry {
            marketplace: request.marketplace.to_owned(),
            version: manifest
                .version
                .clone()
                .unwrap_or_else(|| request.revision.clone()),
            revision: request.revision.clone(),
            source: request.source,
            sha256: request.sha256,
            installed_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .expect("the current UTC time formats as RFC 3339"),
            enabled: previous.as_ref().is_none_or(|previous| previous.enabled),
        };
        registry.plugins.insert(name.clone(), entry.clone());
        if let Err(error) = registry.write(&registry_path) {
            restore(true);
            return Err(error);
        }
        if let Some(previous) = &previous
            && previous.revision != request.revision
        {
            let _ = fs::rename(
                plugin_directory.join(&previous.revision),
                staging.path().join(PREVIOUS_REVISION),
            );
        }
        let outcome = match previous {
            Some(previous) if previous.version == entry.version => InstallOutcome::Reinstalled,
            Some(previous) => InstallOutcome::Replaced {
                previous_version: previous.version,
            },
            None => InstallOutcome::Installed,
        };
        Ok(Installation {
            plugin: InstalledPlugin {
                manifest,
                root: canonical(&target)?,
                marketplace: entry.marketplace,
                version: entry.version,
                revision: entry.revision,
                source: entry.source,
                sha256: entry.sha256,
                installed_at: entry.installed_at,
                enabled: entry.enabled,
            },
            outcome,
        })
    }

    fn read_installed(&self) -> Result<InstalledPlugins, PluginError> {
        let registry = Registry::read(&self.registry_path())?;
        let mut installed = InstalledPlugins::default();
        for (name, entry) in registry.plugins {
            match self.load_installed(&name, &entry) {
                Ok(plugin) => installed.plugins.push(plugin),
                Err(reason) => installed.broken.push(BrokenPlugin {
                    name,
                    marketplace: entry.marketplace,
                    version: entry.version,
                    enabled: entry.enabled,
                    reason,
                }),
            }
        }
        Ok(installed)
    }

    fn load_installed(&self, name: &str, entry: &RegistryEntry) -> Result<InstalledPlugin, String> {
        let root = self
            .plugin_directory(&entry.marketplace, name)
            .join(&entry.revision)
            .canonicalize()
            .map_err(|error| format!("its directory cannot be opened: {error}"))?;
        let manifest =
            PluginManifest::load(&root, name, None).map_err(|error| error.to_string())?;
        if manifest.name != name {
            return Err(format!("its manifest names the plugin {}", manifest.name));
        }
        Ok(InstalledPlugin {
            manifest,
            root,
            marketplace: entry.marketplace.clone(),
            version: entry.version.clone(),
            revision: entry.revision.clone(),
            source: entry.source.clone(),
            sha256: entry.sha256.clone(),
            installed_at: entry.installed_at.clone(),
            enabled: entry.enabled,
        })
    }

    pub(crate) fn registry_path(&self) -> PathBuf {
        self.directory.join(REGISTRY_FILE_NAME)
    }

    pub(crate) fn plugin_directory(&self, marketplace: &str, name: &str) -> PathBuf {
        self.directory
            .join(CACHE_DIRECTORY)
            .join(marketplace)
            .join(name)
    }

    /// Takes the directory's lock, creating the directory when needed, and removes what an
    /// install, uninstall or download that never finished left behind.
    pub(crate) fn lock(&self) -> Result<File, PluginError> {
        create_private_directories(&self.directory)?;
        let path = self.directory.join(LOCK_FILE_NAME);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&path)
            .map_err(PluginError::io(format!("cannot open {}", path.display())))?;
        lock.lock()
            .map_err(PluginError::io(format!("cannot lock {}", path.display())))?;
        let entries = fs::read_dir(&self.directory).map_err(PluginError::io(format!(
            "cannot read {}",
            self.directory.display()
        )))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(STAGING_PREFIX) {
                let _ = fs::remove_dir_all(entry.path());
            } else if name.starts_with(registry::TEMPORARY_PREFIX)
                || name.starts_with(DOWNLOAD_PREFIX)
                    && File::open(entry.path()).is_ok_and(|file| file.try_lock().is_ok())
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(lock)
    }

    /// Waits until no install or uninstall is under way, and holds the next one off while the
    /// returned lock lives.
    fn settled(&self) -> Result<Option<File>, PluginError> {
        let path = self.directory.join(LOCK_FILE_NAME);
        let lock = match File::open(&path) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PluginError::io(format!("cannot open {}", path.display()))(
                    error,
                ));
            }
        };
        lock.lock_shared()
            .map_err(PluginError::io(format!("cannot lock {}", path.display())))?;
        Ok(Some(lock))
    }

    /// A private staging directory beside the plugins, removed with everything in it when
    /// dropped.
    fn staging(&self) -> Result<tempfile::TempDir, PluginError> {
        tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&self.directory)
            .map_err(PluginError::io(format!(
                "cannot create a staging directory in {}",
                self.directory.display()
            )))
    }
}

/// What an install records, what the plugin must be, and whether an installed plugin of the
/// same revision stays.
struct Request<'a> {
    marketplace: &'a str,
    source: PluginSource,
    revision: String,
    sha256: Option<String>,
    /// The name of a plugin without a manifest or a named marketplace entry.
    default_name: String,
    entry: Option<&'a Map<String, Value>>,
    /// The name and version the plugin must have.
    expected: Option<(&'a str, &'a str)>,
    same_revision: SameRevision,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SameRevision {
    Keep,
    Replace,
}

/// The name a plugin at `source` without a manifest takes: its directory's, or its archive's
/// without the `.tar.gz`.
fn default_name_of(source: &Path) -> String {
    let file_name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    file_name
        .strip_suffix(".tar.gz")
        .unwrap_or(&file_name)
        .to_owned()
}

/// Writes `document` as the `.claude-plugin/plugin.json` of the staged plugin at `root`.
fn write_manifest(root: &Path, document: &Value) -> Result<(), PluginError> {
    let path = root.join(crate::CLAUDE_MANIFEST);
    let directory = path.parent().expect("the manifest lives in a directory");
    DirBuilder::new()
        .recursive(true)
        .mode(MANIFEST_DIRECTORY_MODE)
        .create(directory)
        .map_err(PluginError::io(format!(
            "cannot create {}",
            directory.display()
        )))?;
    let mut contents = serde_json::to_vec_pretty(document).expect("a manifest serializes");
    contents.push(b'\n');
    fs::write(&path, contents).map_err(PluginError::io(format!("cannot write {}", path.display())))
}

/// Opens the archive and hashes it, refusing it when `expected_sha256` differs; returns it
/// rewound for unpacking, so what was checked is what is unpacked.
fn open_archive(
    archive: &Path,
    expected_sha256: Option<&str>,
) -> Result<(File, String), PluginError> {
    let failed = || PluginError::io(format!("cannot read {}", archive.display()));
    let mut file = File::open(archive).map_err(failed())?;
    let sha256 = staging::sha256_of(&mut file).map_err(failed())?;
    if let Some(expected) = expected_sha256
        && !sha256.eq_ignore_ascii_case(expected)
    {
        return Err(PluginError::Invalid(format!(
            "the archive's SHA-256 is {sha256}, not {}",
            printable(expected)
        )));
    }
    file.rewind().map_err(failed())?;
    Ok((file, sha256))
}

pub(crate) fn create_private_directories(path: &Path) -> Result<(), PluginError> {
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(path)
        .map_err(PluginError::io(format!("cannot create {}", path.display())))
}

fn canonical(path: &Path) -> Result<PathBuf, PluginError> {
    path.canonicalize()
        .map_err(PluginError::io(format!("cannot open {}", path.display())))
}

/// Moves whatever is at `path` to `aside`; returns whether there was anything.
fn move_aside(path: &Path, aside: &Path) -> Result<bool, PluginError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            fs::rename(path, aside)
                .map_err(PluginError::io(format!("cannot move {}", path.display())))?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PluginError::io(format!("cannot read {}", path.display()))(
            error,
        )),
    }
}
