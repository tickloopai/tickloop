//! The shared install, update and uninstall of Tick packages against a private packages
//! directory; downloads and archives are covered end to end by `tickloop tick` and `/ticks`.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, path::PathBuf};

use serde_json::json;
use tick_packages::{
    ApiKey, CURRENT_TARGET, InstallOutcome, Marketplace, MarketplacePackage, PackageStore,
};
use tickloop_tick_installer::{
    InstalledVersion, MarketplaceSource, install, install_path, offers, uninstall,
};

fn package_directory(root: &Path, name: &str, version: &str) -> PathBuf {
    let directory = root.join("sources").join(name).join(version);
    fs::create_dir_all(directory.join("bin")).unwrap();
    fs::create_dir_all(directory.join("skills").join(name)).unwrap();
    fs::write(
        directory.join("tick-package.json"),
        json!({
            "name": name,
            "version": version,
            "description": format!("The {name} package"),
            "executable": format!("bin/tick-{name}"),
            "protocol": true,
        })
        .to_string(),
    )
    .unwrap();
    let program = directory.join("bin").join(format!("tick-{name}"));
    fs::write(&program, format!("#!/bin/sh\necho {name} {version}\n")).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        directory.join("skills").join(name).join("SKILL.md"),
        format!("---\nname: {name}\n---\n"),
    )
    .unwrap();
    directory
}

/// The marketplace offering each `(name, version, download URL)`, the download for this platform
/// when `installable`.
fn marketplace(offered: &[(&str, &str, &str, bool)]) -> Marketplace {
    let packages = offered
        .iter()
        .map(|(name, version, url, installable)| {
            let target = if *installable {
                CURRENT_TARGET
            } else {
                "x86_64-unknown-linux-gnu"
            };
            json!({
                "name": name,
                "version": version,
                "description": format!("The {name} package"),
                "downloads": { target: { "url": url, "sha256": "0".repeat(64) } },
            })
        })
        .collect::<Vec<_>>();
    let marketplace =
        json!({ "name": "tickloop", "owner": { "name": "Tickloop AI" }, "packages": packages });
    Marketplace::parse(marketplace.to_string().as_bytes()).unwrap()
}

fn offered<'a>(marketplace: &'a Marketplace, name: &str) -> &'a MarketplacePackage {
    marketplace.package(name).unwrap()
}

#[test]
fn offers_pair_each_package_with_what_is_installed_of_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let store = PackageStore::in_config_root(&root.join("config"));
    install_path(&store, &package_directory(root, "current", "1.0.0")).unwrap();
    install_path(&store, &package_directory(root, "older", "1.0.0")).unwrap();
    install_path(&store, &package_directory(root, "broken", "1.0.0")).unwrap();
    fs::remove_file(store.directory().join("broken/tick-package.json")).unwrap();
    let marketplace = marketplace(&[
        ("older", "2.0.0", "https://example.com/older.tar.gz", true),
        ("new", "1.0.0", "https://example.com/new.tar.gz", false),
        (
            "current",
            "1.0.0",
            "https://example.com/current.tar.gz",
            true,
        ),
        ("broken", "1.0.0", "https://example.com/broken.tar.gz", true),
    ]);
    let offers = offers(&marketplace, &store.installed().unwrap());
    let summary = offers
        .iter()
        .map(|offer| {
            (
                offer.package.name.as_str(),
                offer.installed.clone(),
                offer.is_installable,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            ("broken", InstalledVersion::Unusable("1.0.0".into()), true),
            ("current", InstalledVersion::Offered, true),
            ("new", InstalledVersion::NotInstalled, false),
            ("older", InstalledVersion::Other("1.0.0".into()), true),
        ]
    );
}

#[tokio::test]
async fn installing_checks_the_installed_version_platform_and_address_before_downloading() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let store = PackageStore::in_config_root(&root.join("config"));
    install_path(&store, &package_directory(root, "current", "1.0.0")).unwrap();
    let remote = MarketplaceSource::Url("https://example.com/marketplace.json".parse().unwrap());
    let marketplace = marketplace(&[
        // Nothing listens there: a download would fail.
        (
            "current",
            "1.0.0",
            "https://127.0.0.1:9/current.tar.gz",
            true,
        ),
        ("other", "1.0.0", "https://127.0.0.1:9/other.tar.gz", false),
        ("plain", "1.0.0", "http://127.0.0.1:9/plain.tar.gz", true),
    ]);

    let installation = install(
        &http_client,
        &store,
        &remote,
        offered(&marketplace, "current"),
    )
    .await
    .unwrap();
    assert_eq!(installation.outcome, InstallOutcome::AlreadyInstalled);
    assert_eq!(installation.package.manifest.version, "1.0.0");
    assert_eq!(
        install(
            &http_client,
            &store,
            &remote,
            offered(&marketplace, "other")
        )
        .await
        .unwrap_err(),
        format!("Tick package other 1.0.0 has no prebuilt program for {CURRENT_TARGET}")
    );
    assert_eq!(
        install(
            &http_client,
            &store,
            &remote,
            offered(&marketplace, "plain")
        )
        .await
        .unwrap_err(),
        "cannot download plain 1.0.0: http://127.0.0.1:9/plain.tar.gz is refused: \
         a marketplace that is not on this machine may list only https downloads"
    );
    let entries = fs::read_dir(store.directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".download-"))
        .count();
    assert_eq!(entries, 0, "no download was started");
}

#[test]
fn uninstalling_says_where_the_credentials_stay() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let store = PackageStore::in_config_root(&root.join("config"));
    install_path(&store, &package_directory(root, "sample", "1.0.0")).unwrap();
    let credentials = store.credentials("sample").unwrap();
    credentials
        .save_api_key(&ApiKey::new("sample-key").unwrap())
        .unwrap();
    let uninstalled = uninstall(&store, "sample").unwrap();
    assert_eq!(
        (
            uninstalled.version.as_str(),
            uninstalled.credentials.as_deref()
        ),
        (
            "1.0.0",
            Some(root.join("config/credentials/sample").as_path())
        )
    );
    assert!(credentials.api_key_path().exists());
    assert_eq!(
        uninstall(&store, "sample").unwrap_err(),
        "Tick package sample is not installed"
    );
}

fn http_client(
    _: &reqwest::Url,
    _: tickloop_tick_installer::HttpRequest,
) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|error| error.to_string())
}
