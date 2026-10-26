use std::fs;

use serde_json::json;

use tick_packages::{ApiKey, ExpectedArchive, LoginState, PackageStore};
use tick_plugins::{PluginSource, PluginStore};

use crate::support::{Fixture, pack};

const DOWNLOAD_URL: &str = "https://example.test/quotes-0.2.7.tar.gz";

/// Installs, with the store an earlier Tickloop used, one package from a local directory and
/// one from a marketplace download, and logs the second in.
fn earlier_packages(fixture: &Fixture) -> PackageStore {
    let packages = PackageStore::in_config_root(&fixture.config_root());
    let local = fixture.tick_package("local", "local-tool", "0.1.0");
    packages.install_path(&local).unwrap();
    let downloaded = fixture.tick_package("quotes", "quotes", "0.2.7");
    let archive = fixture.root().join("quotes.tar.gz");
    let sha256 = pack(&downloaded, &archive);
    packages
        .install_archive(
            &archive,
            ExpectedArchive {
                name: "quotes",
                version: "0.2.7",
                sha256: &sha256,
                source: DOWNLOAD_URL,
            },
        )
        .unwrap();
    packages
        .credentials("quotes")
        .unwrap()
        .save_api_key(&ApiKey::new("secret").unwrap())
        .unwrap();
    packages
}

#[test]
fn first_use_merges_earlier_packages_into_an_existing_plugin_registry() {
    let fixture = Fixture::new();
    let plugin = fixture.claude_plugin("notes", &json!({"name": "notes"}), &["write"]);
    fixture.store.install_path(&plugin).unwrap();
    fixture.store.set_enabled("notes", false).unwrap();
    earlier_packages(&fixture);

    let installed = fixture.store.installed().unwrap();
    let ids = installed
        .plugins
        .iter()
        .map(|plugin| plugin.id())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["local-tool@local", "notes@local", "quotes@tickloop"]
    );
    assert!(!installed.plugins[1].enabled);
    assert_eq!(
        fixture.store.login_state(&installed.plugins[2]),
        LoginState::LoggedIn
    );
    assert!(!fixture.config_root().join("ticks").exists());
    assert_eq!(fixture.store.migrate().unwrap(), None);
}

#[test]
fn an_explicit_migration_moves_earlier_tick_packages_into_the_plugin_store() {
    let fixture = Fixture::new();
    earlier_packages(&fixture);

    let migration = fixture.store.migrate().unwrap().unwrap();
    let installed = fixture.store.installed().unwrap();

    assert_eq!(
        migration.migrated,
        vec!["local-tool@local", "quotes@tickloop"]
    );

    let ids = installed
        .plugins
        .iter()
        .map(|plugin| plugin.id())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["local-tool@local", "quotes@tickloop"]);
    assert!(installed.broken.is_empty());
    let quotes = &installed.plugins[1];
    assert_eq!(quotes.version, "0.2.7");
    assert_eq!(quotes.revision, "0.2.7");
    assert!(quotes.enabled);
    assert_eq!(
        quotes.source,
        PluginSource::Download {
            url: DOWNLOAD_URL.to_owned()
        }
    );
    assert!(quotes.sha256.is_some());
    assert!(quotes.executable().unwrap().is_file());
    assert_eq!(fixture.store.login_state(quotes), LoginState::LoggedIn);
    assert_eq!(installed.plugins[0].revision, "local");

    let config = fixture.config_root();
    assert!(!config.join("ticks").exists());
    let kept = fs::read_dir(&config)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("ticks.migrated-"))
        .collect::<Vec<_>>();
    assert_eq!(kept.len(), 1, "{kept:?}");

    assert_eq!(fixture.store.migrate().unwrap(), None);
    assert_eq!(fixture.store.installed().unwrap(), installed);
}

#[test]
fn an_explicit_migration_reports_what_moved_and_what_stayed_behind() {
    let fixture = Fixture::new();
    let packages = earlier_packages(&fixture);
    let local = packages.get("local-tool").unwrap();
    fs::remove_file(local.root.join("tick-package.json")).unwrap();

    let migration = fixture.store.migrate().unwrap().unwrap();

    assert_eq!(migration.migrated, vec!["quotes@tickloop"]);
    assert_eq!(migration.skipped.len(), 1);
    assert_eq!(migration.skipped[0].name, "local-tool");
    assert!(migration.kept_at.join("local-tool").is_dir());
    assert_eq!(
        fixture.store.installed().unwrap().plugins.len(),
        1,
        "the unusable package is not registered"
    );
}

#[test]
fn a_migration_that_fails_leaves_the_earlier_packages_as_they_were() {
    let fixture = Fixture::new();
    earlier_packages(&fixture);
    let plugins = fixture.store.directory();
    fs::create_dir_all(plugins.join("cache")).unwrap();
    fs::write(plugins.join("cache/tickloop"), "in the way").unwrap();

    let error = fixture.store.migrate().unwrap_err();

    assert!(error.to_string().contains("cannot create"), "{error}");
    assert!(!plugins.join("installed.json").exists());
    let earlier = PackageStore::in_config_root(&fixture.config_root())
        .installed()
        .unwrap();
    assert_eq!(earlier.packages.len(), 2);
    assert!(earlier.broken.is_empty());

    fs::remove_file(plugins.join("cache/tickloop")).unwrap();
    let store = PluginStore::in_config_root(&fixture.config_root());
    assert!(store.migrate().unwrap().is_some());
    assert_eq!(store.installed().unwrap().plugins.len(), 2);
}

#[test]
fn nothing_moves_without_earlier_packages() {
    let fixture = Fixture::new();

    assert_eq!(fixture.store.migrate().unwrap(), None);
    assert!(fixture.store.installed().unwrap().plugins.is_empty());
    assert!(!fixture.store.directory().exists());
}

#[test]
fn a_name_held_by_a_plugin_is_skipped_and_reported_without_overwriting_it() {
    let fixture = Fixture::new();
    let plugin = fixture.claude_plugin("quotes", &json!({"name": "quotes"}), &["write"]);
    let installed = fixture.store.install_path(&plugin).unwrap().plugin;
    fixture.store.set_enabled("quotes", false).unwrap();
    earlier_packages(&fixture);
    let migration = fixture.store.migrate().unwrap().unwrap();
    assert_eq!(migration.migrated, vec!["local-tool@local"]);
    assert_eq!(migration.skipped.len(), 1);
    assert_eq!(migration.skipped[0].name, "quotes");
    assert!(migration.skipped[0].reason.contains("name already held"));
    assert!(migration.kept_at.join("quotes").is_dir());
    let retained = fixture.store.get("quotes").unwrap();
    assert_eq!(retained.root, installed.root);
    assert!(!retained.enabled);
    assert_eq!(retained.manifest, installed.manifest);
}

#[test]
fn a_background_first_use_keeps_the_migration_notice_for_the_next_cli_use() {
    let fixture = Fixture::new();
    let packages = earlier_packages(&fixture);
    fs::remove_file(
        packages
            .get("local-tool")
            .unwrap()
            .root
            .join("tick-package.json"),
    )
    .unwrap();
    fixture.store.installed().unwrap();
    let reopened = PluginStore::in_config_root(&fixture.config_root());
    let notices = reopened.take_migration_notices().unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("quotes@tickloop"));
    assert!(notices[0].contains("Skipped Tick package local-tool"));
    assert!(reopened.take_migration_notices().unwrap().is_empty());
}
