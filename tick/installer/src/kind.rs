//! Ticks and plugins as users meet them: a plugin that carries a Tick program is a Tick, managed
//! with `/ticks` and `tickloop tick`; every other plugin is managed with `/plugins` and
//! `tickloop plugin`. Both share one store, the marketplaces and the install and login code.

use tick_plugins::{
    BrokenPlugin, InstalledPlugin, MarketplacePlugin, MarketplacePluginSource, PluginManifest,
    TICKLOOP_MARKETPLACE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginKind {
    Plugin,
    Tick,
}

impl PluginKind {
    pub fn of_manifest(manifest: &PluginManifest) -> Self {
        if manifest.tick.is_some() {
            Self::Tick
        } else {
            Self::Plugin
        }
    }

    pub fn of_installed(plugin: &InstalledPlugin) -> Self {
        Self::of_manifest(&plugin.manifest)
    }

    /// Only a Tick marketplace package is known to carry a Tick program before it is fetched;
    /// any other offer turns out to be a Tick only once installed.
    pub fn of_offer(plugin: &MarketplacePlugin) -> Self {
        match plugin.source {
            MarketplacePluginSource::Archive(_) => Self::Tick,
            _ => Self::Plugin,
        }
    }

    /// Whether this kind lists the unusable `plugin`. Its manifest cannot be read, so only one the
    /// Tick marketplace installed is known to be a Tick; any other is listed with both kinds, where
    /// either can repair or remove it.
    pub fn lists_broken(self, plugin: &BrokenPlugin) -> bool {
        plugin.marketplace != TICKLOOP_MARKETPLACE || self == Self::Tick
    }

    /// The word for one plugin of this kind within a sentence.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Tick => "Tick",
        }
    }

    /// The word for one plugin of this kind at the start of a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Self::Plugin => "Plugin",
            Self::Tick => "Tick",
        }
    }

    /// The word for several plugins of this kind within a sentence.
    pub fn plural(self) -> &'static str {
        match self {
            Self::Plugin => "plugins",
            Self::Tick => "Ticks",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Plugin => "tickloop plugin",
            Self::Tick => "tickloop tick",
        }
    }
}
