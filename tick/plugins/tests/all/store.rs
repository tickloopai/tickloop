use std::{fs, path::PathBuf};

use serde_json::json;
use tick_packages::{ApiKey, LoginState};
use tick_plugins::{
    ExpectedArchive, InstallOutcome, ManifestFormat, PluginError, PluginSource,
    TICKLOOP_MARKETPLACE,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::support::{Fixture, mode, pack, write_json, write_skill};

const DOWNLOAD_URL: &str = "https://example.test/quotes.tar.gz";

fn expected<'a>(version: &'a str, sha256: &'a str) -> ExpectedArchive<'a> {
    ExpectedArchive {
        marketplace: TICKLOOP_MARKETPLACE,
        name: "quotes",
        version,
        sha256,
        url: DOWNLOAD_URL,
    }
}

#[test]
fn a_local_plugin_installs_into_the_cache_with_its_registration() {
    let fixture = Fixture::new();
    let source = fixture.claude_plugin(
        "review",
        &json!({"name": "review", "version": "1.2.0", "commands": "./commands"}),
        &["check"],
    );
    fs::create_dir_all(source.join("commands")).unwrap();

    let installation = fixture.store.install_path(&source).unwrap();

    assert_eq!(installation.outcome, InstallOutcome::Installed);
    let plugin = &installation.plugin;
    let root = fixture
        .store
        .directory()
        .canonicalize()
        .unwrap()
        .join("cache/local/review/local");
    assert_eq!(plugin.root, root);
    assert_eq!(plugin.id(), "review@local");
    assert_eq!(plugin.version, "1.2.0");
    assert_eq!(plugin.revision, "local");
    assert!(plugin.enabled);
    assert_eq!(plugin.executable(), None);
    assert_eq!(plugin.skill_directories(), vec![root.join("skills/check")]);
    assert_eq!(
        plugin.source,
        PluginSource::Path {
            path: source.to_string_lossy().into_owned()
        }
    );
    OffsetDateTime::parse(&plugin.installed_at, &Rfc3339).unwrap();
    assert_eq!(
        fixture.registry(),
        json!({
            "version": 1,
            "plugins": {"review": {
                "marketplace": "local",
                "version": "1.2.0",
                "revision": "local",
                "source": {"type": "path", "path": source.to_string_lossy()},
                "installed_at": plugin.installed_at,
                "enabled": true,
            }},
        })
    );
    assert_eq!(mode(fixture.store.directory()), 0o700);
    assert_eq!(
        mode(&fixture.store.directory().join("installed.json")),
        0o600
    );
    assert_eq!(
        fixture.store.installed().unwrap().plugins,
        vec![plugin.clone()]
    );
    assert_eq!(fixture.store.get("review").unwrap(), *plugin);
}

#[test]
fn a_local_plugin_is_replaced_on_every_install() {
    let fixture = Fixture::new();
    let source = fixture.claude_plugin(
        "review",
        &json!({"name": "review", "version": "1.0.0"}),
        &[],
    );
    fixture.store.install_path(&source).unwrap();

    let again = fixture.store.install_path(&source).unwrap();
    assert_eq!(again.outcome, InstallOutcome::Reinstalled);

    write_json(
        &source.join(".claude-plugin/plugin.json"),
        &json!({"name": "review", "version": "1.1.0"}),
    );
    let newer = fixture.store.install_path(&source).unwrap();
    assert_eq!(
        newer.outcome,
        InstallOutcome::Replaced {
            previous_version: "1.0.0".to_owned()
        }
    );
    assert_eq!(fixture.store.get("review").unwrap().version, "1.1.0");
}

#[test]
fn a_plugin_without_a_manifest_keeps_its_name_once_installed() {
    let fixture = Fixture::new();
    let source = fixture.source("tidy-up");
    write_skill(&source.join("skills/tidy"), "tidy");

    let installation = fixture.store.install_path(&source).unwrap();

    assert_eq!(installation.plugin.name(), "tidy-up");
    assert_eq!(installation.plugin.version, "local");
    let installed = fixture.store.get("tidy-up").unwrap();
    assert_eq!(installed.manifest.format, ManifestFormat::Claude);
    assert_eq!(
        installed.skill_directories(),
        vec![installed.root.join("skills/tidy")]
    );
    assert!(!source.join(".claude-plugin").exists());
}

#[test]
fn version_control_data_stays_behind() {
    let fixture = Fixture::new();
    let source = fixture.claude_plugin("review", &json!({"name": "review"}), &["check"]);
    fs::create_dir_all(source.join(".git/objects")).unwrap();
    fs::write(source.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let plugin = fixture.store.install_path(&source).unwrap().plugin;

    assert!(!plugin.root.join(".git").exists());
    assert!(plugin.root.join("skills/check/SKILL.md").is_file());
}

#[test]
fn a_marketplace_archive_installs_when_it_matches_and_keeps_an_installed_version() {
    let fixture = Fixture::new();
    let package = fixture.tick_package("quotes-1", "quotes", "1.0.0");
    let archive = fixture.root().join("quotes-1.tar.gz");
    let sha256 = pack(&package, &archive);

    let installation = fixture
        .store
        .install_archive(&archive, expected("1.0.0", &sha256))
        .unwrap();
    assert_eq!(installation.outcome, InstallOutcome::Installed);
    let plugin = &installation.plugin;
    assert_eq!(plugin.id(), "quotes@tickloop");
    assert_eq!(plugin.revision, "1.0.0");
    assert_eq!(plugin.sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(
        plugin.source,
        PluginSource::Download {
            url: DOWNLOAD_URL.to_owned()
        }
    );
    assert_eq!(
        plugin.executable(),
        Some(plugin.root.join("bin/tick-quotes"))
    );
    assert!(plugin.root.ends_with("cache/tickloop/quotes/1.0.0"));

    let kept = fixture
        .store
        .install_archive(&archive, expected("1.0.0", &sha256))
        .unwrap();
    assert_eq!(kept.outcome, InstallOutcome::AlreadyInstalled);

    let newer_package = fixture.tick_package("quotes-2", "quotes", "2.0.0");
    let newer_archive = fixture.root().join("quotes-2.tar.gz");
    let newer_sha256 = pack(&newer_package, &newer_archive);
    let newer = fixture
        .store
        .install_archive(&newer_archive, expected("2.0.0", &newer_sha256))
        .unwrap();
    assert_eq!(
        newer.outcome,
        InstallOutcome::Replaced {
            previous_version: "1.0.0".to_owned()
        }
    );
    let plugin_directory = fixture.store.directory().join("cache/tickloop/quotes");
    assert!(plugin_directory.join("2.0.0").is_dir());
    assert!(!plugin_directory.join("1.0.0").exists());
}

#[test]
fn an_archive_that_does_not_match_its_listing_changes_nothing() {
    let fixture = Fixture::new();
    let package = fixture.tick_package("quotes", "quotes", "1.0.0");
    let archive = fixture.root().join("quotes.tar.gz");
    let sha256 = pack(&package, &archive);

    let error = fixture
        .store
        .install_archive(&archive, expected("1.0.0", "00"))
        .unwrap_err();
    assert!(error.to_string().contains("SHA-256"), "{error}");

    let wrong_name = ExpectedArchive {
        name: "other",
        ..expected("1.0.0", &sha256)
    };
    let error = fixture
        .store
        .install_archive(&archive, wrong_name)
        .unwrap_err();
    assert_eq!(error.to_string(), "the plugin is named quotes, not other");

    let error = fixture
        .store
        .install_archive(&archive, expected("9.9.9", &sha256))
        .unwrap_err();
    assert_eq!(error.to_string(), "the plugin is version 1.0.0, not 9.9.9");

    assert_eq!(
        fixture.plugin_files(),
        vec![PathBuf::from(".lock")],
        "only the lock stays behind"
    );
}

#[test]
fn a_plugin_name_belongs_to_one_marketplace_at_a_time() {
    let fixture = Fixture::new();
    let package = fixture.tick_package("quotes", "quotes", "1.0.0");
    let archive = fixture.root().join("quotes.tar.gz");
    let sha256 = pack(&package, &archive);
    fixture
        .store
        .install_archive(&archive, expected("1.0.0", &sha256))
        .unwrap();
    let registry = fixture.registry();

    let error = fixture.store.install_path(&package).unwrap_err();

    assert!(matches!(error, PluginError::NameTaken { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        "plugin quotes is already installed from marketplace tickloop; uninstall it first"
    );
    assert_eq!(fixture.registry(), registry);
}

#[test]
fn a_disabled_plugin_stays_installed_and_disabled_across_reinstalls() {
    let fixture = Fixture::new();
    let source = fixture.claude_plugin("review", &json!({"name": "review"}), &["check"]);
    fixture.store.install_path(&source).unwrap();

    let disabled = fixture.store.set_enabled("review", false).unwrap();
    assert!(!disabled.enabled);
    assert!(!fixture.store.get("review").unwrap().enabled);
    assert_eq!(
        fixture.registry()["plugins"]["review"]["enabled"],
        json!(false)
    );

    let reinstalled = fixture.store.install_path(&source).unwrap();
    assert!(!reinstalled.plugin.enabled);

    assert!(fixture.store.set_enabled("review", true).unwrap().enabled);
    let error = fixture.store.set_enabled("missing", true).unwrap_err();
    assert_eq!(error.to_string(), "plugin missing is not installed");
}

#[test]
fn uninstalling_removes_the_plugin_and_its_data_but_keeps_its_login() {
    let fixture = Fixture::new();
    let package = fixture.tick_package("quotes", "quotes", "1.0.0");
    let plugin = fixture.store.install_path(&package).unwrap().plugin;
    let data = fixture.store.create_data_directory("quotes").unwrap();
    assert_eq!(mode(&data), 0o700);
    fs::write(data.join("state"), "kept").unwrap();
    let credentials = fixture.store.credentials("quotes").unwrap();
    credentials
        .save_api_key(&ApiKey::new("secret").unwrap())
        .unwrap();
    assert_eq!(fixture.store.login_state(&plugin), LoginState::LoggedIn);

    let uninstalled = fixture.store.uninstall("quotes", false).unwrap();

    assert_eq!(uninstalled.name, "quotes");
    assert_eq!(uninstalled.marketplace, "local");
    assert_eq!(uninstalled.version, "1.0.0");
    assert_eq!(
        uninstalled.credentials.as_deref(),
        Some(credentials.directory())
    );
    assert_eq!(uninstalled.data, None);
    assert!(!data.exists());
    assert!(
        !fixture
            .store
            .directory()
            .join("cache/local/quotes")
            .exists()
    );
    assert_eq!(fixture.registry()["plugins"], json!({}));
    let error = fixture.store.get("quotes").unwrap_err();
    assert!(matches!(error, PluginError::NotInstalled(_)), "{error}");
    let error = fixture.store.uninstall("quotes", false).unwrap_err();
    assert!(matches!(error, PluginError::NotInstalled(_)), "{error}");
}

#[test]
fn uninstalling_can_keep_the_data() {
    let fixture = Fixture::new();
    let source = fixture.claude_plugin("review", &json!({"name": "review"}), &[]);
    fixture.store.install_path(&source).unwrap();
    let data = fixture.store.create_data_directory("review").unwrap();

    let uninstalled = fixture.store.uninstall("review", true).unwrap();

    assert_eq!(uninstalled.data, Some(data.clone()));
    assert!(data.is_dir());
    assert_eq!(uninstalled.credentials, None);
}

#[test]
fn a_plugin_that_fails_to_install_leaves_nothing_behind() {
    let fixture = Fixture::new();
    let installed = fixture.claude_plugin("review", &json!({"name": "review"}), &[]);
    fixture.store.install_path(&installed).unwrap();
    let registry = fixture.registry();
    let files = fixture.plugin_files();
    let broken = fixture.source("broken");
    fs::create_dir_all(broken.join(".claude-plugin")).unwrap();
    fs::write(broken.join(".claude-plugin/plugin.json"), "{").unwrap();

    let error = fixture.store.install_path(&broken).unwrap_err();

    assert!(error.to_string().contains("is not valid JSON"), "{error}");
    assert_eq!(fixture.registry(), registry);
    assert_eq!(fixture.plugin_files(), files);
}

#[test]
fn a_plugin_whose_files_are_gone_is_listed_as_broken() {
    let fixture = Fixture::new();
    let source = fixture.claude_plugin(
        "review",
        &json!({"name": "review", "version": "1.0.0"}),
        &[],
    );
    let plugin = fixture.store.install_path(&source).unwrap().plugin;
    fs::remove_dir_all(&plugin.root).unwrap();

    let installed = fixture.store.installed().unwrap();

    assert!(installed.plugins.is_empty());
    assert_eq!(installed.broken.len(), 1);
    let broken = &installed.broken[0];
    assert_eq!(
        (
            broken.name.as_str(),
            broken.marketplace.as_str(),
            broken.version.as_str()
        ),
        ("review", "local", "1.0.0")
    );
    assert!(
        broken.reason.contains("cannot be opened"),
        "{}",
        broken.reason
    );
    let error = fixture.store.get("review").unwrap_err();
    assert!(matches!(error, PluginError::Unusable { .. }), "{error}");
    let registry = fixture.registry();
    let error = fixture.store.set_enabled("review", false).unwrap_err();
    assert!(matches!(error, PluginError::Unusable { .. }), "{error}");
    assert_eq!(
        fixture.registry(),
        registry,
        "a failed change changes nothing"
    );
}

#[test]
fn a_damaged_registry_is_reported_with_its_path() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.store.directory()).unwrap();
    fs::write(fixture.store.directory().join("installed.json"), "{}").unwrap();

    let error = fixture.store.installed().unwrap_err();

    assert!(matches!(error, PluginError::Registry { .. }), "{error}");
    assert!(
        error.to_string().contains("installed.json is damaged"),
        "{error}"
    );
}
