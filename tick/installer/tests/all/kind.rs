//! Which user-facing kind a plugin belongs to: Ticks carry a Tick program, plugins do not.

use std::path::PathBuf;

use tick_packages::{Marketplace, PackageAuth};
use tick_plugins::{
    BrokenPlugin, ManifestFormat, MarketplacePlugin, MarketplacePluginSource, PluginManifest,
    TickProgram,
};
use tickloop_tick_installer::PluginKind;

fn manifest(tick: Option<TickProgram>) -> PluginManifest {
    PluginManifest {
        mcp_servers: Default::default(),
        hooks: Vec::new(),
        hook_support: Default::default(),
        user_config: Default::default(),
        name: "sample".into(),
        display_name: None,
        version: Some("1.0.0".into()),
        description: None,
        format: ManifestFormat::Tickloop,
        tick,
        skill_paths: vec![PathBuf::from("skills")],
        commands: None,
        components: Default::default(),
        warnings: Vec::new(),
        auth: PackageAuth::None,
    }
}

fn offered(source: MarketplacePluginSource) -> MarketplacePlugin {
    MarketplacePlugin {
        name: "sample".into(),
        description: None,
        version: Some("1.0.0".into()),
        source,
        manifest: Default::default(),
    }
}

fn broken(marketplace: &str) -> BrokenPlugin {
    BrokenPlugin {
        name: "sample".into(),
        marketplace: marketplace.into(),
        version: "1.0.0".into(),
        enabled: true,
        reason: "its files are missing".into(),
    }
}

#[test]
fn a_plugin_with_a_tick_program_is_a_tick_even_with_skills() {
    let program = TickProgram {
        executable: PathBuf::from("bin/tick-sample"),
        protocol: true,
    };
    assert_eq!(
        PluginKind::of_manifest(&manifest(Some(program))),
        PluginKind::Tick
    );
    assert_eq!(PluginKind::of_manifest(&manifest(None)), PluginKind::Plugin);
}

#[test]
fn only_a_tick_marketplace_package_is_offered_as_a_tick() {
    let marketplace = Marketplace::parse(
        serde_json::json!({
            "name": "tickloop",
            "owner": {"name": "Tickloop AI"},
            "packages": [{
                "name": "sample",
                "version": "1.0.0",
                "description": "Sample",
                "downloads": {},
            }],
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let package = marketplace.packages.into_iter().next().unwrap();
    assert_eq!(
        PluginKind::of_offer(&offered(MarketplacePluginSource::Archive(package))),
        PluginKind::Tick
    );
    assert_eq!(
        PluginKind::of_offer(&offered(MarketplacePluginSource::Relative(
            "./sample".into()
        ))),
        PluginKind::Plugin
    );
}

#[test]
fn an_unusable_plugin_of_unknown_kind_is_listed_with_both_kinds() {
    let from_tick_marketplace = broken("tickloop");
    assert!(PluginKind::Tick.lists_broken(&from_tick_marketplace));
    assert!(!PluginKind::Plugin.lists_broken(&from_tick_marketplace));
    let local = broken("local");
    assert!(PluginKind::Tick.lists_broken(&local));
    assert!(PluginKind::Plugin.lists_broken(&local));
}

#[test]
fn each_kind_names_itself_and_its_command() {
    let names = |kind: PluginKind| (kind.noun(), kind.label(), kind.plural(), kind.command());
    assert_eq!(
        names(PluginKind::Tick),
        ("Tick", "Tick", "Ticks", "tickloop tick")
    );
    assert_eq!(
        names(PluginKind::Plugin),
        ("plugin", "Plugin", "plugins", "tickloop plugin")
    );
}
