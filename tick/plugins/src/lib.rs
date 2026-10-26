//! Plugins: what a plugin directory declares in its manifest, and the plugins installed under a
//! Tickloop config root, in its `plugins/` directory with the `installed.json` registry. A Tick
//! package is a plugin whose components are a Tick program and its Skills.
//!
//! Installing only places files: nothing in a plugin runs, no Tick opens and no access is
//! granted.

mod catalog;
mod git;
mod http;
mod manifest;
mod marketplace;
mod migration;
mod registry;
mod source;
mod staging;
mod store;

use std::{ffi::OsStr, io, path::PathBuf};

pub use catalog::{MarketplaceCatalog, MarketplacePlugin, MarketplacePluginSource};
pub use git::{GitCheckout, GitClient};
pub use manifest::{
    CLAUDE_MANIFEST, Component, ManifestFormat, PluginManifest, TICKLOOP_MANIFEST, TickProgram,
    UserConfigField, UserConfigType, skill_directories, validate_plugin_name,
};
pub use marketplace::{
    CLAUDE_OFFICIAL_MARKETPLACE, HttpClient, HttpRequest, MarketplaceRegistration,
};
pub use migration::Migration;
pub use registry::{PluginSource, REGISTRY_FILE_NAME};
pub use source::{GitSource, MarketplaceSource};
pub use store::{
    BrokenPlugin, DownloadFile, ExpectedArchive, InstallOutcome, Installation, InstalledPlugin,
    InstalledPlugins, PluginStore, Uninstalled,
};

/// The directory under the config root that holds the plugins and their registry.
pub const PLUGINS_DIRECTORY: &str = "plugins";
/// The marketplace of plugins installed from a local directory or archive.
pub const LOCAL_MARKETPLACE: &str = "local";
/// The marketplace of the official Tick packages.
pub const TICKLOOP_MARKETPLACE: &str = "tickloop";

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin, its manifest or its archive breaks a rule of the plugin format.
    #[error("{0}")]
    Invalid(String),
    #[error("plugin {0} is not installed")]
    NotInstalled(String),
    #[error(
        "plugin {name} is already installed from marketplace {marketplace}; uninstall it first"
    )]
    NameTaken { name: String, marketplace: String },
    /// A Tick package an earlier Tickloop installed has the plugin's name.
    #[error("{0} is already installed as a Tick package; uninstall it first")]
    TickPackageNameTaken(String),
    #[error("plugin {name} cannot be used ({reason}); install it again")]
    Unusable { name: String, reason: String },
    #[error("{} is damaged ({reason}); repair it, or remove it and reinstall your plugins", path.display())]
    Registry { path: PathBuf, reason: String },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
}

impl PluginError {
    fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

impl From<tick_packages::PackageError> for PluginError {
    fn from(error: tick_packages::PackageError) -> Self {
        match error {
            tick_packages::PackageError::Io { context, source } => Self::Io { context, source },
            other => Self::Invalid(other.to_string()),
        }
    }
}

/// `text` with its control characters escaped, for messages that show what a plugin or an
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
