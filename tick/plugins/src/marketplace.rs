//! Marketplace registrations and cached catalogs, separate from installed plugins.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    GitSource, MarketplaceCatalog, MarketplaceSource, PluginError, PluginStore,
    TICKLOOP_MARKETPLACE, http::MARKETPLACE_SIZE_LIMIT,
};

const MARKETPLACES_DIRECTORY: &str = "marketplaces";
const MARKETPLACE_REGISTRY_FILE: &str = "marketplaces.json";
const MARKETPLACE_FILE: &str = "marketplace.json";
const MARKETPLACE_CACHE_PREFIX: &str = "catalog-";
const PRIVATE_FILE_MODE: u32 = 0o600;

pub const CLAUDE_OFFICIAL_MARKETPLACE: &str = "claude-plugins-official";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceRegistration {
    pub name: String,
    pub source: MarketplaceSource,
    pub builtin: bool,
    pub refreshed_at: Option<String>,
    pub commit: Option<String>,
}

impl PluginStore {
    /// Lists registrations without fetching any marketplace.
    pub fn marketplaces(&self) -> Result<Vec<MarketplaceRegistration>, PluginError> {
        let tick_source = std::env::var_os(tick_packages::MARKETPLACE_ENVIRONMENT_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| tick_packages::DEFAULT_MARKETPLACE_URL.to_owned());
        let mut registrations = vec![
            MarketplaceRegistration {
                name: TICKLOOP_MARKETPLACE.to_owned(),
                source: if tick_source.contains("://") {
                    MarketplaceSource::Url(tick_source)
                } else {
                    MarketplaceSource::File(tick_source.into())
                },
                builtin: true,
                refreshed_at: None,
                commit: None,
            },
            MarketplaceRegistration {
                name: CLAUDE_OFFICIAL_MARKETPLACE.to_owned(),
                source: MarketplaceSource::Git(GitSource {
                    url: "https://github.com/anthropics/claude-plugins-official.git".to_owned(),
                    reference: None,
                    sha: None,
                    subdirectory: None,
                }),
                builtin: true,
                refreshed_at: None,
                commit: None,
            },
        ];
        for cached in self.read_marketplace_registry()?.into_values() {
            if let Some(builtin) = registrations
                .iter_mut()
                .find(|entry| entry.name == cached.registration.name)
            {
                if builtin.source == cached.registration.source {
                    *builtin = cached.registration;
                }
            } else {
                registrations.push(cached.registration);
            }
        }
        Ok(registrations)
    }

    pub fn remove_marketplace(&self, name: &str) -> Result<(), PluginError> {
        if matches!(name, TICKLOOP_MARKETPLACE | CLAUDE_OFFICIAL_MARKETPLACE) {
            return Err(PluginError::Invalid(format!(
                "the built-in marketplace {name} cannot be removed"
            )));
        }
        let _lock = self.lock_directory()?;
        let mut registry = self.read_marketplace_registry()?;
        let removed = registry
            .remove(name)
            .ok_or_else(|| PluginError::Invalid(format!("marketplace {name} is not registered")))?;
        self.write_marketplace_registry(&registry)?;
        let _ = fs::remove_dir_all(
            self.directory
                .join(MARKETPLACES_DIRECTORY)
                .join(removed.cache),
        );
        Ok(())
    }
}

pub type HttpClient =
    dyn Fn(&reqwest::Url, HttpRequest) -> Result<reqwest::Client, String> + Send + Sync;

#[derive(Clone, Copy, Debug)]
pub enum HttpRequest {
    Marketplace,
    Download,
}

#[derive(Serialize, Deserialize)]
struct CachedMarketplace {
    registration: MarketplaceRegistration,
    cache: String,
}

const MARKETPLACE_LOCATIONS: [&str; 3] = [
    ".tickloop-plugin/marketplace.json",
    ".claude-plugin/marketplace.json",
    MARKETPLACE_FILE,
];

impl PluginStore {
    pub async fn add_marketplace(
        &self,
        source: &MarketplaceSource,
        http: &HttpClient,
    ) -> Result<MarketplaceCatalog, PluginError> {
        self.fetch_marketplace(source, None, http).await
    }

    pub async fn refresh_marketplace(
        &self,
        name: &str,
        http: &HttpClient,
    ) -> Result<MarketplaceCatalog, PluginError> {
        let registration = self
            .marketplaces()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| PluginError::Invalid(format!("marketplace {name} is not registered")))?;
        self.fetch_marketplace(&registration.source, Some(&registration), http)
            .await
    }

    /// Returns the last successful catalog, fetching only when it has no cache yet.
    pub async fn marketplace(
        &self,
        name: &str,
        http: &HttpClient,
    ) -> Result<MarketplaceCatalog, PluginError> {
        {
            let _lock = self.lock_directory()?;
            if let Some(cached) = self.read_marketplace_registry()?.get(name) {
                let current = self
                    .marketplaces()?
                    .into_iter()
                    .find(|entry| entry.name == name);
                if current
                    .as_ref()
                    .is_some_and(|entry| entry.source == cached.registration.source)
                {
                    let path = self
                        .directory
                        .join(MARKETPLACES_DIRECTORY)
                        .join(&cached.cache)
                        .join(MARKETPLACE_FILE);
                    return MarketplaceCatalog::parse(
                        cached.registration.clone(),
                        &read_document(&path)?,
                    );
                }
            }
        }
        self.refresh_marketplace(name, http).await
    }

    async fn fetch_marketplace(
        &self,
        source: &MarketplaceSource,
        previous: Option<&MarketplaceRegistration>,
        http: &HttpClient,
    ) -> Result<MarketplaceCatalog, PluginError> {
        let mut commit = None;
        let document = match source {
            MarketplaceSource::Directory(root) => read_directory_document(root)?,
            MarketplaceSource::File(path) => read_document(path)?,
            MarketplaceSource::Git(source) => {
                crate::store::create_private_directories(&self.directory)?;
                let checkout = crate::GitClient::default()
                    .checkout(source, &self.directory)
                    .await?;
                let document = read_directory_document(checkout.root())?;
                commit = Some(checkout.commit().to_owned());
                document
            }
            MarketplaceSource::Url(text) => {
                let url = crate::http::secure_url(text, true)?;
                let mut bytes = Vec::new();
                crate::http::receive(
                    http,
                    &url,
                    HttpRequest::Marketplace,
                    MARKETPLACE_SIZE_LIMIT,
                    |chunk| {
                        bytes.extend_from_slice(chunk);
                        Ok(())
                    },
                )
                .await?;
                serde_json::from_slice(&bytes).map_err(|error| {
                    PluginError::Invalid(format!("invalid marketplace JSON: {error}"))
                })?
            }
        };
        let name = document
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::Invalid("marketplace must have a name".into()))?;
        let name = previous
            .filter(|registration| {
                registration.builtin && registration.name == TICKLOOP_MARKETPLACE
            })
            .map_or(name, |registration| registration.name.as_str());
        crate::validate_plugin_name(name)?;
        if previous.is_none()
            && matches!(
                name,
                TICKLOOP_MARKETPLACE | CLAUDE_OFFICIAL_MARKETPLACE | crate::LOCAL_MARKETPLACE
            )
        {
            return Err(PluginError::Invalid(format!(
                "marketplace name {name} is reserved"
            )));
        }
        if previous.is_some_and(|previous| previous.name != name) {
            return Err(PluginError::Invalid(format!(
                "marketplace changed its name to {name}"
            )));
        }
        let registration = MarketplaceRegistration {
            name: name.to_owned(),
            source: source.clone(),
            builtin: previous.is_some_and(|previous| previous.builtin),
            refreshed_at: Some(
                OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .expect("UTC timestamp formats"),
            ),
            commit,
        };
        let catalog = MarketplaceCatalog::parse(registration.clone(), &document)?;
        let _lock = self.lock_directory()?;
        let mut registry = self.read_marketplace_registry()?;
        if previous.is_none() && registry.contains_key(name) {
            return Err(PluginError::Invalid(format!(
                "marketplace {name} is already registered"
            )));
        }
        if let Some(previous) = previous {
            let current = registry.get(name).map(|cached| &cached.registration);
            let changed = if previous.refreshed_at.is_some() {
                current != Some(previous)
            } else {
                current.is_some_and(|current| current.source == previous.source)
            };
            if changed {
                return Err(PluginError::Invalid(format!(
                    "marketplace {name} changed while refreshing; retry the operation"
                )));
            }
        }
        let cache_root = self.directory.join(MARKETPLACES_DIRECTORY);
        crate::store::create_private_directories(&cache_root)?;
        let cache = tempfile::Builder::new()
            .prefix(MARKETPLACE_CACHE_PREFIX)
            .tempdir_in(&cache_root)
            .map_err(PluginError::io("cannot stage marketplace"))?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(cache.path().join(MARKETPLACE_FILE))
            .map_err(PluginError::io("cannot cache marketplace"))?;
        serde_json::to_writer(&file, &document)
            .map_err(|error| PluginError::Invalid(error.to_string()))?;
        file.sync_all()
            .map_err(PluginError::io("cannot sync marketplace"))?;
        let cached = CachedMarketplace {
            registration,
            cache: cache
                .path()
                .file_name()
                .expect("cache has name")
                .to_string_lossy()
                .into_owned(),
        };
        let old = registry.insert(name.to_owned(), cached);
        self.write_marketplace_registry(&registry)?;
        let _ = cache.keep();
        if let Some(old) = old {
            let _ = fs::remove_dir_all(cache_root.join(old.cache));
        }
        Ok(catalog)
    }

    /// Installs one named entry from a cached catalog, fetching the catalog on first use.
    pub async fn install_marketplace_plugin(
        &self,
        marketplace: &str,
        name: &str,
        http: &HttpClient,
    ) -> Result<crate::Installation, PluginError> {
        let catalog = self.marketplace(marketplace, http).await?;
        let plugin = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| {
                PluginError::Invalid(format!("marketplace {marketplace} has no plugin {name}"))
            })?;
        match &plugin.source {
            crate::MarketplacePluginSource::Relative(relative) => {
                let source = match &catalog.registration.source {
                    MarketplaceSource::Git(source) => source,
                    MarketplaceSource::Directory(root) => {
                        let root = crate::git::contained_directory(root, relative)?;
                        return self.install_marketplace_directory(
                            &root,
                            marketplace,
                            name,
                            &plugin.manifest,
                        );
                    }
                    MarketplaceSource::File(_) | MarketplaceSource::Url(_) => {
                        return Err(PluginError::Invalid(
                            "relative plugin requires a directory or Git marketplace".into(),
                        ));
                    }
                };
                let mut source = source.clone();
                source.reference = plugin
                    .manifest
                    .get("ref")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                source.sha = plugin
                    .manifest
                    .get("sha")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        source
                            .reference
                            .is_none()
                            .then(|| catalog.registration.commit.clone())
                            .flatten()
                    });
                source.subdirectory = Some(match source.subdirectory {
                    Some(parent) => format!("{parent}/{relative}"),
                    None => relative.clone(),
                });
                let checkout = crate::GitClient::default()
                    .checkout(&source, &self.directory)
                    .await?;
                self.install_checkout(&checkout, marketplace, name, Some(&plugin.manifest))
            }
            crate::MarketplacePluginSource::Git(source) => {
                let checkout = crate::GitClient::default()
                    .checkout(source, &self.directory)
                    .await?;
                self.install_checkout(&checkout, marketplace, name, Some(&plugin.manifest))
            }
            crate::MarketplacePluginSource::Archive(package) => {
                match self.get(name) {
                    Ok(plugin)
                        if plugin.marketplace == marketplace
                            && plugin.revision == package.version =>
                    {
                        return Ok(crate::Installation {
                            plugin,
                            outcome: crate::InstallOutcome::AlreadyInstalled,
                        });
                    }
                    Ok(plugin) if plugin.marketplace != marketplace => {
                        return Err(PluginError::NameTaken {
                            name: name.to_owned(),
                            marketplace: plugin.marketplace,
                        });
                    }
                    Ok(_) | Err(PluginError::NotInstalled(_) | PluginError::Unusable { .. }) => {}
                    Err(error) => return Err(error),
                }
                let download = package.current_download().ok_or_else(|| {
                    PluginError::Invalid(format!(
                        "plugin {name} has no download for {}",
                        tick_packages::CURRENT_TARGET
                    ))
                })?;
                let url = catalog.registration.source.download_url(&download.url)?;
                let mut file = self.download_file()?;
                crate::http::receive(
                    http,
                    &url,
                    HttpRequest::Download,
                    crate::http::DOWNLOAD_SIZE_LIMIT,
                    |chunk| file.write_all(chunk),
                )
                .await?;
                self.install_archive(
                    file.path(),
                    crate::ExpectedArchive {
                        marketplace,
                        name,
                        version: &package.version,
                        sha256: &download.sha256,
                        url: url.as_str(),
                    },
                )
            }
            _ => Err(PluginError::Invalid(format!(
                "plugin {name} has an unsupported source"
            ))),
        }
    }

    fn read_marketplace_registry(
        &self,
    ) -> Result<BTreeMap<String, CachedMarketplace>, PluginError> {
        let path = self.directory.join(MARKETPLACE_REGISTRY_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| PluginError::Registry {
                path,
                reason: error.to_string(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(PluginError::io("cannot read marketplace registrations")(
                error,
            )),
        }
    }

    fn write_marketplace_registry(
        &self,
        registry: &BTreeMap<String, CachedMarketplace>,
    ) -> Result<(), PluginError> {
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)
            .map_err(PluginError::io("cannot stage marketplace registrations"))?;
        let bytes = serde_json::to_vec_pretty(registry).expect("marketplace registry serializes");
        file.write_all(&bytes)
            .map_err(PluginError::io("cannot write marketplace registrations"))?;
        file.as_file()
            .sync_all()
            .map_err(PluginError::io("cannot sync marketplace registrations"))?;
        file.persist(self.directory.join(MARKETPLACE_REGISTRY_FILE))
            .map_err(|error| {
                PluginError::io("cannot save marketplace registrations")(error.error)
            })?;
        Ok(())
    }
}

fn read_document(path: &Path) -> Result<Value, PluginError> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take(MARKETPLACE_SIZE_LIMIT + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(PluginError::io("cannot read marketplace"))?;
    if bytes.len() as u64 > MARKETPLACE_SIZE_LIMIT {
        return Err(PluginError::Invalid("marketplace exceeds 1 MiB".into()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| PluginError::Invalid(format!("invalid marketplace JSON: {error}")))
}

fn read_directory_document(root: &Path) -> Result<Value, PluginError> {
    let path = MARKETPLACE_LOCATIONS
        .iter()
        .map(|relative| root.join(relative))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            PluginError::Invalid(format!("no marketplace file in {}", root.display()))
        })?;
    read_document(&path)
}
