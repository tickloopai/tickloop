//! `marketplace.json`: the Tick packages a Tick marketplace offers, each at one version with a
//! prebuilt archive per target.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::{
    manifest::{is_valid_version, text_problem},
    validate_package_name,
};

pub const DEFAULT_MARKETPLACE_URL: &str =
    "https://raw.githubusercontent.com/tickloopai/ticks/main/marketplace.json";
/// Names another https URL or a local file to read the marketplace from, for development and
/// tests.
pub const MARKETPLACE_ENVIRONMENT_VARIABLE: &str = "TICKLOOP_TICK_MARKETPLACE";
/// The target triple this program was built for, which picks a package's download.
pub const CURRENT_TARGET: &str = env!("TICK_PACKAGES_TARGET");
const SHA256_HEX_LENGTH: usize = 64;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidMarketplace(String);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Marketplace {
    pub name: String,
    pub owner: MarketplaceOwner,
    pub packages: Vec<MarketplacePackage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MarketplaceOwner {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MarketplacePackage {
    pub name: String,
    /// The version the marketplace offers, which installing it brings.
    pub version: String,
    pub description: String,
    /// The prebuilt package archive for each target triple.
    pub downloads: BTreeMap<String, PackageDownload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PackageDownload {
    pub url: String,
    pub sha256: String,
}

impl Marketplace {
    /// Parses a `marketplace.json` and checks the names, versions, texts and checksums it lists;
    /// the download URLs are left to whoever downloads them.
    pub fn parse(bytes: &[u8]) -> Result<Self, InvalidMarketplace> {
        let marketplace: Self = serde_json::from_slice(bytes)
            .map_err(|error| InvalidMarketplace(format!("its JSON is not valid: {error}")))?;
        if let Some(problem) = text_problem(&marketplace.name) {
            return Err(InvalidMarketplace(format!("its name {problem}")));
        }
        if let Some(problem) = text_problem(&marketplace.owner.name) {
            return Err(InvalidMarketplace(format!("its owner's name {problem}")));
        }
        let mut names = BTreeSet::new();
        for package in &marketplace.packages {
            validate_package_name(&package.name)
                .map_err(|error| InvalidMarketplace(error.to_string()))?;
            let name = &package.name;
            if !names.insert(name) {
                return Err(InvalidMarketplace(format!(
                    "it lists {name} more than once"
                )));
            }
            if !is_valid_version(&package.version) {
                return Err(InvalidMarketplace(format!(
                    "version {:?} of {name} may hold only letters, digits, '.', '+' and '-'",
                    package.version
                )));
            }
            if let Some(problem) = text_problem(&package.description) {
                return Err(InvalidMarketplace(format!(
                    "the description of {name} {problem}"
                )));
            }
            for (target, download) in &package.downloads {
                let is_sha256 = download.sha256.len() == SHA256_HEX_LENGTH
                    && download.sha256.bytes().all(|byte| byte.is_ascii_hexdigit());
                if !is_sha256 {
                    return Err(InvalidMarketplace(format!(
                        "the {target:?} download of {name} has no valid SHA-256"
                    )));
                }
            }
        }
        Ok(marketplace)
    }

    pub fn package(&self, name: &str) -> Option<&MarketplacePackage> {
        self.packages.iter().find(|package| package.name == name)
    }
}

impl MarketplacePackage {
    /// The prebuilt archive for [`CURRENT_TARGET`], if the marketplace offers one.
    pub fn current_download(&self) -> Option<&PackageDownload> {
        self.downloads.get(CURRENT_TARGET)
    }
}
