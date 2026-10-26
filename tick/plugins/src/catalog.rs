//! The common listing exposed by plugin and Tick marketplaces.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::{MarketplaceRegistration, PluginError, validate_plugin_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketplacePluginSource {
    Relative(String),
    Git(crate::GitSource),
    Archive(tick_packages::MarketplacePackage),
    Unsupported(Value),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketplacePlugin {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub source: MarketplacePluginSource,
    /// The entry supplies the manifest when the plugin has none.
    pub manifest: Map<String, Value>,
}

impl MarketplacePlugin {
    pub fn is_installable(&self) -> bool {
        match &self.source {
            MarketplacePluginSource::Unsupported(_) => false,
            MarketplacePluginSource::Archive(package) => package.current_download().is_some(),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketplaceCatalog {
    pub registration: MarketplaceRegistration,
    pub plugins: Vec<MarketplacePlugin>,
}

impl MarketplaceCatalog {
    pub(crate) fn parse(
        registration: MarketplaceRegistration,
        document: &Value,
    ) -> Result<Self, PluginError> {
        if document.get("packages").is_some() {
            let marketplace = tick_packages::Marketplace::parse(
                &serde_json::to_vec(document).expect("JSON serializes"),
            )
            .map_err(|error| PluginError::Invalid(error.to_string()))?;
            let plugins = marketplace
                .packages
                .into_iter()
                .map(|package| MarketplacePlugin {
                    name: package.name.clone(),
                    description: Some(package.description.clone()),
                    version: Some(package.version.clone()),
                    source: MarketplacePluginSource::Archive(package),
                    manifest: Map::new(),
                })
                .collect();
            return Ok(Self {
                registration,
                plugins,
            });
        }
        let entries = document
            .get("plugins")
            .and_then(Value::as_array)
            .ok_or_else(|| PluginError::Invalid("marketplace must have a plugins list".into()))?;
        let mut names = BTreeSet::new();
        let mut plugins = Vec::new();
        for entry in entries {
            let manifest = entry.as_object().ok_or_else(|| {
                PluginError::Invalid("marketplace plugin must be an object".into())
            })?;
            let name = entry.get("name").and_then(Value::as_str).ok_or_else(|| {
                PluginError::Invalid("marketplace plugin must have a name".into())
            })?;
            validate_plugin_name(name)?;
            if !names.insert(name) {
                return Err(PluginError::Invalid(format!(
                    "marketplace lists {name} more than once"
                )));
            }
            let source = entry.get("source").cloned().unwrap_or(Value::Null);
            let source = match source.as_str() {
                Some(path) if path.starts_with("./") => {
                    MarketplacePluginSource::Relative(path.to_owned())
                }
                Some(url) if url.contains("://") || url.starts_with("git@") => {
                    let crate::MarketplaceSource::Git(git) = crate::MarketplaceSource::parse(url)?
                    else {
                        return Err(PluginError::Invalid(
                            "plugin source must be a Git URL".into(),
                        ));
                    };
                    MarketplacePluginSource::Git(git)
                }
                _ if matches!(
                    source.get("source").and_then(Value::as_str),
                    Some("github" | "url" | "git" | "git-subdir")
                ) =>
                {
                    let kind = source["source"].as_str().expect("source kind matched");
                    let location = source
                        .get(if kind == "github" { "repo" } else { "url" })
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            PluginError::Invalid(format!("plugin {name} has no Git repository"))
                        })?;
                    let crate::MarketplaceSource::Git(mut git) =
                        crate::MarketplaceSource::parse(location)?
                    else {
                        return Err(PluginError::Invalid(
                            "plugin source must be a Git repository".into(),
                        ));
                    };
                    git.reference = source
                        .get("ref")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(git.reference);
                    git.sha = source.get("sha").and_then(Value::as_str).map(str::to_owned);
                    if kind == "git-subdir" {
                        git.subdirectory = Some(
                            source
                                .get("path")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    PluginError::Invalid(format!(
                                        "plugin {name} has no Git subdirectory"
                                    ))
                                })?
                                .to_owned(),
                        );
                    }
                    MarketplacePluginSource::Git(git)
                }
                _ => MarketplacePluginSource::Unsupported(source),
            };
            plugins.push(MarketplacePlugin {
                name: name.to_owned(),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                version: entry
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                source,
                manifest: manifest.clone(),
            });
        }
        Ok(Self {
            registration,
            plugins,
        })
    }
}
