//! Tick packages: the `tick-package.json` manifest every package carries, the
//! `marketplace.json` of a Tick marketplace, the packages installed under a Tickloop config
//! root, in its `ticks/` directory with the `installed.json` registry, and the data source
//! credentials Tickloop keeps for them in its `credentials/` directory.
//!
//! Installing only places files: nothing in a package runs, no Tick opens and no access is
//! granted.

mod credentials;
mod manifest;
mod marketplace;
mod registry;
mod staging;

use std::{
    ffi::OsStr,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Seek, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub use credentials::{
    API_KEY_FILE_NAME, ApiKey, CONFIG_ROOT_VARIABLE, CREDENTIALS_DIRECTORY, LockedOAuth,
    LoginState, OAUTH_LOCK_FILE_NAME, OAUTH_TOKEN_FILE_NAME, OAUTH_TOKEN_FILE_VARIABLE, OAuthToken,
    PackageCredentials, default_config_root,
};
pub use manifest::{
    MANIFEST_FILE_NAME, OAuthIssuer, PackageAuth, PackageManifest, validate_package_name,
};
pub use marketplace::{
    CURRENT_TARGET, DEFAULT_MARKETPLACE_URL, InvalidMarketplace, MARKETPLACE_ENVIRONMENT_VARIABLE,
    Marketplace, MarketplaceOwner, MarketplacePackage, PackageDownload,
};
pub use registry::REGISTRY_FILE_NAME;
use registry::{Registry, RegistryEntry};

/// The directory under the config root that holds the installed packages and their registry.
pub const PACKAGES_DIRECTORY: &str = "ticks";

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const LOCK_FILE_NAME: &str = ".lock";
/// Every staging directory starts with it; a name never does, so it cannot shadow a package.
const STAGING_PREFIX: &str = ".staging-";
/// Every download file starts with it, so it cannot shadow a package either.
const DOWNLOAD_PREFIX: &str = ".download-";
const STAGED_PACKAGE: &str = "package";
const DISPLACED_PACKAGE: &str = "displaced";

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// The package, its manifest or its archive breaks a rule of the Tick package format.
    #[error("{0}")]
    Invalid(String),
    #[error("Tick package {0} is not installed")]
    NotInstalled(String),
    #[error("Tick package {name} cannot be used ({reason}); install it again")]
    Unusable { name: String, reason: String },
    #[error("{} is damaged ({reason}); repair it, or remove it and reinstall your Tick packages", path.display())]
    Registry { path: PathBuf, reason: String },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
}

impl PackageError {
    fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

/// A package as installed: its manifest, where it is, and how it was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledPackage {
    pub manifest: PackageManifest,
    /// The absolute package directory.
    pub root: PathBuf,
    /// The package directory, archive path or download URL it was installed from.
    pub source: String,
    /// The SHA-256 of the archive it was installed from; `None` for a package directory.
    pub sha256: Option<String>,
    /// When it was installed, in RFC 3339.
    pub installed_at: String,
}

impl InstalledPackage {
    /// The absolute path of the package's program.
    pub fn executable(&self) -> PathBuf {
        self.root.join(&self.manifest.executable)
    }
}

/// A registered package whose files can no longer be used, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokenPackage {
    pub name: String,
    /// The version the registry recorded.
    pub version: String,
    pub reason: String,
}

/// The registered packages, each list sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstalledPackages {
    pub packages: Vec<InstalledPackage>,
    pub broken: Vec<BrokenPackage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    Replaced {
        previous_version: String,
    },
    /// The installed package of the same version was replaced.
    Reinstalled,
    /// The same version was already installed; nothing changed.
    AlreadyInstalled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    /// The package now installed; for [`InstallOutcome::AlreadyInstalled`], the one that was.
    pub package: InstalledPackage,
    pub outcome: InstallOutcome,
}

/// What a marketplace lists for a downloaded archive; [`PackageStore::install_archive`] installs
/// the archive only when it matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedArchive<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
    /// Recorded as the package's source, such as the download URL.
    pub source: &'a str,
}

/// A private file in the packages directory to download an archive into before
/// [`PackageStore::install_archive`] installs it. It is removed when dropped. Its process holds a
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
    pub version: String,
    /// The directory of the package's data source credentials, which stay, when there is one.
    pub credentials: Option<PathBuf>,
}

/// The Tick packages installed under one Tickloop config root, and the credentials kept for
/// them there. Installs and uninstalls hold a lock on the directory; a failed one leaves the
/// previous package and the registry as they were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageStore {
    config_root: PathBuf,
    directory: PathBuf,
}

impl PackageStore {
    pub fn in_config_root(config_root: &Path) -> Self {
        Self {
            config_root: config_root.to_path_buf(),
            directory: config_root.join(PACKAGES_DIRECTORY),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Where the credentials of the package `name` are kept, whether or not it is installed.
    pub fn credentials(&self, name: &str) -> Result<PackageCredentials, PackageError> {
        PackageCredentials::in_config_root(&self.config_root, name)
    }

    /// The login state of `package`, from its saved credentials alone.
    pub fn login_state(&self, package: &InstalledPackage) -> LoginState {
        match self.credentials(&package.manifest.name) {
            Ok(credentials) => credentials.login_state(&package.manifest.auth),
            Err(_) => LoginState::Unknown,
        }
    }

    /// Lists the registered packages, setting apart those whose files are missing or no longer
    /// form a valid package. One that looks broken while an install or uninstall is under way is
    /// read again once that has finished.
    pub fn installed(&self) -> Result<InstalledPackages, PackageError> {
        let installed = self.read_installed()?;
        if installed.broken.is_empty() {
            return Ok(installed);
        }
        let _settled = self.settled()?;
        self.read_installed()
    }

    /// The installed package `name`. One that looks broken while an install or uninstall is under
    /// way is read again once that has finished.
    pub fn get(&self, name: &str) -> Result<InstalledPackage, PackageError> {
        validate_package_name(name)?;
        let read = || {
            let registry = Registry::read(&self.registry_path())?;
            let entry = registry
                .packages
                .get(name)
                .ok_or_else(|| PackageError::NotInstalled(name.to_owned()))?;
            self.load_installed(name, entry)
                .map_err(|reason| PackageError::Unusable {
                    name: name.to_owned(),
                    reason,
                })
        };
        match read() {
            Err(PackageError::Unusable { .. }) => {
                let _settled = self.settled()?;
                read()
            }
            read => read,
        }
    }

    /// Installs the package directory or `.tar.gz` archive at `path`, recording its path as the
    /// source. It replaces the installed package of its name even at the same version, since a
    /// local build need not change the version.
    pub fn install_path(&self, path: &Path) -> Result<Installation, PackageError> {
        let source = path
            .canonicalize()
            .map_err(PackageError::io(format!("cannot open {}", path.display())))?;
        let source_text = source.to_string_lossy().into_owned();
        if source.is_file() {
            let (archive, sha256) = open_archive(&source, None)?;
            let request = Request {
                source: source_text,
                sha256: Some(sha256),
                expected: None,
                same_version: SameVersion::Replace,
            };
            return self.install(request, |staged| staging::unpack_archive(archive, staged));
        }
        if !source.is_dir() {
            return Err(PackageError::Invalid(format!(
                "{} is neither a package directory nor an archive",
                source.display()
            )));
        }
        let request = Request {
            source: source_text,
            sha256: None,
            expected: None,
            same_version: SameVersion::Replace,
        };
        self.install(request, |staged| {
            if canonical(&self.directory)?.starts_with(&source) {
                return Err(PackageError::Invalid(format!(
                    "{} contains the Tick packages directory",
                    source.display()
                )));
            }
            staging::copy_directory(&source, staged)
        })
    }

    /// Installs the package in the `.tar.gz` archive at `archive`, whose root holds the package
    /// directory's contents, when it matches `expected`: its SHA-256 is checked before it is
    /// unpacked, its name and version before it is installed. An installed package of the same
    /// version is kept as it is.
    pub fn install_archive(
        &self,
        archive: &Path,
        expected: ExpectedArchive<'_>,
    ) -> Result<Installation, PackageError> {
        let (opened, sha256) = open_archive(archive, Some(expected.sha256))?;
        let request = Request {
            source: expected.source.to_owned(),
            sha256: Some(sha256),
            expected: Some((expected.name, expected.version)),
            same_version: SameVersion::Keep,
        };
        self.install(request, |staged| staging::unpack_archive(opened, staged))
    }

    /// A new, empty [`DownloadFile`], creating the packages directory when needed.
    pub fn download_file(&self) -> Result<DownloadFile, PackageError> {
        // Under the directory's lock, so no install or uninstall takes the new file for one left
        // behind before this process locks it.
        let _lock = self.lock()?;
        let file = tempfile::Builder::new()
            .prefix(DOWNLOAD_PREFIX)
            .tempfile_in(&self.directory)
            .map_err(PackageError::io(format!(
                "cannot create a download file in {}",
                self.directory.display()
            )))?;
        file.as_file().lock().map_err(PackageError::io(format!(
            "cannot lock {}",
            file.path().display()
        )))?;
        Ok(DownloadFile(file))
    }

    /// Removes the package and its registration. Its data source credentials stay.
    pub fn uninstall(&self, name: &str) -> Result<Uninstalled, PackageError> {
        validate_package_name(name)?;
        if !self.directory.exists() {
            return Err(PackageError::NotInstalled(name.to_owned()));
        }
        let _lock = self.lock()?;
        let registry_path = self.registry_path();
        let mut registry = Registry::read(&registry_path)?;
        let entry = registry
            .packages
            .remove(name)
            .ok_or_else(|| PackageError::NotInstalled(name.to_owned()))?;
        let target = self.directory.join(name);
        let staging = self.staging()?;
        let displaced = staging.path().join(DISPLACED_PACKAGE);
        let had_files = move_aside(&target, &displaced)?;
        if let Err(error) = registry.write(&registry_path) {
            if had_files {
                let _ = fs::rename(&displaced, &target);
            }
            return Err(error);
        }
        let credentials = self.credentials(name)?.directory().to_path_buf();
        Ok(Uninstalled {
            name: name.to_owned(),
            version: entry.version,
            credentials: credentials.exists().then_some(credentials),
        })
    }

    /// Builds the package in a staging directory with `fill`, checks it, and replaces the
    /// installed package of its name: the old directory is moved aside and the staged one renamed
    /// into its place, then the registry is replaced; when a step fails, both directories go back.
    fn install(
        &self,
        request: Request<'_>,
        fill: impl FnOnce(&Path) -> Result<(), PackageError>,
    ) -> Result<Installation, PackageError> {
        let _lock = self.lock()?;
        let staging = self.staging()?;
        let staged = staging.path().join(STAGED_PACKAGE);
        fs::create_dir(&staged).map_err(PackageError::io(format!(
            "cannot create {}",
            staged.display()
        )))?;
        fill(&staged)?;
        staging::check_tree(&staged)?;
        let manifest = PackageManifest::load(&staged)?;
        if let Some((name, version)) = request.expected {
            if manifest.name != name {
                return Err(PackageError::Invalid(format!(
                    "the package is named {}, not {}",
                    manifest.name,
                    printable(name)
                )));
            }
            if manifest.version != version {
                return Err(PackageError::Invalid(format!(
                    "the package is version {}, not {}",
                    manifest.version,
                    printable(version)
                )));
            }
        }

        let registry_path = self.registry_path();
        let mut registry = Registry::read(&registry_path)?;
        let previous = registry.packages.get(&manifest.name).cloned();
        if request.same_version == SameVersion::Keep
            && let Some(entry) = &previous
            && let Ok(installed) = self.load_installed(&manifest.name, entry)
            && installed.manifest.version == manifest.version
        {
            return Ok(Installation {
                package: installed,
                outcome: InstallOutcome::AlreadyInstalled,
            });
        }

        let target = self.directory.join(&manifest.name);
        let installed_root = canonical(&self.directory)?.join(&manifest.name);
        let displaced = staging.path().join(DISPLACED_PACKAGE);
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
            return Err(PackageError::io(format!(
                "cannot install {}",
                target.display()
            ))(error));
        }
        let entry = RegistryEntry {
            version: manifest.version.clone(),
            source: request.source,
            sha256: request.sha256,
            installed_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .expect("the current UTC time formats as RFC 3339"),
        };
        registry
            .packages
            .insert(manifest.name.clone(), entry.clone());
        if let Err(error) = registry.write(&registry_path) {
            restore(true);
            return Err(error);
        }
        let outcome = match previous {
            Some(previous) if previous.version == manifest.version => InstallOutcome::Reinstalled,
            Some(previous) => InstallOutcome::Replaced {
                previous_version: previous.version,
            },
            None => InstallOutcome::Installed,
        };
        Ok(Installation {
            package: InstalledPackage {
                manifest,
                root: installed_root,
                source: entry.source,
                sha256: entry.sha256,
                installed_at: entry.installed_at,
            },
            outcome,
        })
    }

    fn read_installed(&self) -> Result<InstalledPackages, PackageError> {
        let registry = Registry::read(&self.registry_path())?;
        let mut installed = InstalledPackages::default();
        for (name, entry) in registry.packages {
            match self.load_installed(&name, &entry) {
                Ok(package) => installed.packages.push(package),
                Err(reason) => installed.broken.push(BrokenPackage {
                    name,
                    version: entry.version,
                    reason,
                }),
            }
        }
        Ok(installed)
    }

    fn load_installed(
        &self,
        name: &str,
        entry: &RegistryEntry,
    ) -> Result<InstalledPackage, String> {
        let root = self
            .directory
            .join(name)
            .canonicalize()
            .map_err(|error| format!("its directory cannot be opened: {error}"))?;
        let manifest = PackageManifest::load(&root).map_err(|error| error.to_string())?;
        if manifest.name != name {
            return Err(format!("its manifest names the package {}", manifest.name));
        }
        Ok(InstalledPackage {
            manifest,
            root,
            source: entry.source.clone(),
            sha256: entry.sha256.clone(),
            installed_at: entry.installed_at.clone(),
        })
    }

    fn registry_path(&self) -> PathBuf {
        self.directory.join(REGISTRY_FILE_NAME)
    }

    /// Takes the directory's lock, creating the directory when needed, and removes what an
    /// install, uninstall or download that never finished left behind.
    fn lock(&self) -> Result<File, PackageError> {
        DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&self.directory)
            .map_err(PackageError::io(format!(
                "cannot create {}",
                self.directory.display()
            )))?;
        let path = self.directory.join(LOCK_FILE_NAME);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&path)
            .map_err(PackageError::io(format!("cannot open {}", path.display())))?;
        lock.lock()
            .map_err(PackageError::io(format!("cannot lock {}", path.display())))?;
        let entries = fs::read_dir(&self.directory).map_err(PackageError::io(format!(
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
    fn settled(&self) -> Result<Option<File>, PackageError> {
        let path = self.directory.join(LOCK_FILE_NAME);
        let lock = match File::open(&path) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PackageError::io(format!("cannot open {}", path.display()))(
                    error,
                ));
            }
        };
        lock.lock_shared()
            .map_err(PackageError::io(format!("cannot lock {}", path.display())))?;
        Ok(Some(lock))
    }

    /// A private staging directory beside the packages, removed with everything in it when
    /// dropped.
    fn staging(&self) -> Result<tempfile::TempDir, PackageError> {
        tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&self.directory)
            .map_err(PackageError::io(format!(
                "cannot create a staging directory in {}",
                self.directory.display()
            )))
    }
}

/// What an install records, what the package must be, and whether an installed package of the
/// same version stays.
struct Request<'a> {
    source: String,
    sha256: Option<String>,
    /// The name and version the package must have.
    expected: Option<(&'a str, &'a str)>,
    same_version: SameVersion,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SameVersion {
    Keep,
    Replace,
}

/// Opens the archive and hashes it, refusing it when `expected_sha256` differs; returns it
/// rewound for unpacking, so what was checked is what is unpacked.
fn open_archive(
    archive: &Path,
    expected_sha256: Option<&str>,
) -> Result<(File, String), PackageError> {
    let failed = || PackageError::io(format!("cannot read {}", archive.display()));
    let mut file = File::open(archive).map_err(failed())?;
    let sha256 = staging::sha256_of(&mut file).map_err(failed())?;
    if let Some(expected) = expected_sha256
        && !sha256.eq_ignore_ascii_case(expected)
    {
        return Err(PackageError::Invalid(format!(
            "the archive's SHA-256 is {sha256}, not {}",
            printable(expected)
        )));
    }
    file.rewind().map_err(failed())?;
    Ok((file, sha256))
}

/// `text` with its control characters escaped, for messages that show what a package or an
/// archive names.
fn printable(text: impl AsRef<OsStr>) -> String {
    let mut printable = String::new();
    for character in text.as_ref().to_string_lossy().chars() {
        if character.is_control() {
            printable.extend(character.escape_default());
        } else {
            printable.push(character);
        }
    }
    printable
}

fn canonical(path: &Path) -> Result<PathBuf, PackageError> {
    path.canonicalize()
        .map_err(PackageError::io(format!("cannot open {}", path.display())))
}

/// Moves whatever is at `path` to `aside`; returns whether there was anything.
fn move_aside(path: &Path, aside: &Path) -> Result<bool, PackageError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            fs::rename(path, aside)
                .map_err(PackageError::io(format!("cannot move {}", path.display())))?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PackageError::io(format!("cannot read {}", path.display()))(
            error,
        )),
    }
}
