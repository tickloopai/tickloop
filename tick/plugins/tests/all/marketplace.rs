use tick_plugins::{CLAUDE_OFFICIAL_MARKETPLACE, GitSource, MarketplaceSource};

use crate::support::Fixture;

#[test]
fn marketplace_sources_expand_github_and_builtins_are_lazy() {
    assert_eq!(
        MarketplaceSource::parse("owner/repo#release").unwrap(),
        MarketplaceSource::Git(GitSource {
            url: "https://github.com/owner/repo.git".into(),
            reference: Some("release".into()),
            sha: None,
            subdirectory: None,
        })
    );
    let fixture = Fixture::new();
    let marketplaces = fixture.store.marketplaces().unwrap();
    assert_eq!(marketplaces.len(), 2);
    assert!(
        marketplaces
            .iter()
            .all(|marketplace| marketplace.builtin && marketplace.refreshed_at.is_none())
    );
    assert!(
        marketplaces
            .iter()
            .any(|marketplace| marketplace.name == CLAUDE_OFFICIAL_MARKETPLACE)
    );
    assert!(!fixture.store.directory().exists());
    for name in ["tickloop", CLAUDE_OFFICIAL_MARKETPLACE] {
        assert!(
            fixture
                .store
                .remove_marketplace(name)
                .unwrap_err()
                .to_string()
                .contains("built-in")
        );
    }
}

use crate::support::write_json;
use serde_json::json;

fn no_http(_: &reqwest::Url, _: tick_plugins::HttpRequest) -> Result<reqwest::Client, String> {
    panic!("tests must not access the network")
}

#[tokio::test]
async fn a_local_marketplace_is_registered_refreshed_and_removed() {
    let fixture = Fixture::new();
    let root = fixture.source("market");
    let path = root.join(".claude-plugin/marketplace.json");
    write_json(
        &path,
        &json!({"name": "sample", "plugins": [
            {"name": "review", "source": "./plugins/review", "strict": false},
            {"name": "unsupported", "source": {"source": "npm", "package": "example"}}
        ]}),
    );
    let added = fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(root.clone()), &no_http)
        .await
        .unwrap();
    assert_eq!(added.registration.name, "sample");
    assert_eq!(added.plugins.len(), 2);
    assert!(added.plugins[0].is_installable());
    assert!(!added.plugins[1].is_installable());
    assert!(added.registration.refreshed_at.is_some());
    assert_eq!(fixture.store.marketplaces().unwrap().len(), 3);
    write_json(&path, &json!({"name": "sample", "plugins": []}));
    assert_eq!(
        fixture
            .store
            .marketplace("sample", &no_http)
            .await
            .unwrap()
            .plugins
            .len(),
        2
    );
    assert!(
        fixture
            .store
            .refresh_marketplace("sample", &no_http)
            .await
            .unwrap()
            .plugins
            .is_empty()
    );
    fixture.store.remove_marketplace("sample").unwrap();
    assert_eq!(fixture.store.marketplaces().unwrap().len(), 2);
}

#[tokio::test]
async fn marketplace_lookup_prefers_tickloop_then_claude_then_root_and_reads_tick_packages() {
    let fixture = Fixture::new();
    let root = fixture.source("formats");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name": "root-market", "owner": {"name": "Test"}, "packages": [{
            "name": "quotes", "version": "1.0", "description": "Quotes", "downloads": {
                (tick_packages::CURRENT_TARGET): {"url": "https://example.test/quotes.tar.gz", "sha256": "a".repeat(64)}
            }
        }]}),
    );
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": "claude-market", "plugins": []}),
    );
    write_json(
        &root.join(".tickloop-plugin/marketplace.json"),
        &json!({"name": "tickloop-market", "plugins": []}),
    );
    let source = MarketplaceSource::Directory(root.clone());
    assert_eq!(
        fixture
            .store
            .add_marketplace(&source, &no_http)
            .await
            .unwrap()
            .registration
            .name,
        "tickloop-market"
    );
    std::fs::remove_file(root.join(".tickloop-plugin/marketplace.json")).unwrap();
    assert_eq!(
        fixture
            .store
            .add_marketplace(&source, &no_http)
            .await
            .unwrap()
            .registration
            .name,
        "claude-market"
    );
    std::fs::remove_file(root.join(".claude-plugin/marketplace.json")).unwrap();
    let catalog = fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    assert_eq!(catalog.registration.name, "root-market");
    assert_eq!(catalog.plugins[0].version.as_deref(), Some("1.0"));
    assert!(catalog.plugins[0].is_installable());
    assert!(
        matches!(&catalog.plugins[0].source, tick_plugins::MarketplacePluginSource::Archive(package) if package.current_download().unwrap().sha256 == "a".repeat(64))
    );
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn git_marketplace_installs_relative_plugins_by_commit_and_keeps_same_revision() {
    let fixture = Fixture::new();
    let root = fixture.source("git-market");
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": "git-market", "plugins": [
            {"name": "review", "source": "./plugins/review", "strict": false, "skills": "./custom"}
        ]}),
    );
    crate::support::write_skill(&root.join("plugins/review/custom"), "check");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial"]);
    let commit = git(&root, &["rev-parse", "HEAD"]);
    let source = MarketplaceSource::parse(&format!("file://{}", root.display())).unwrap();
    let catalog = fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    assert_eq!(
        catalog.registration.commit.as_deref(),
        Some(commit.as_str())
    );
    let installed = fixture
        .store
        .install_marketplace_plugin("git-market", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(installed.plugin.revision, commit[..12]);
    assert_eq!(installed.plugin.version, commit[..12]);
    assert_eq!(installed.plugin.id(), "review@git-market");
    assert_eq!(
        installed.plugin.skill_directories(),
        vec![installed.plugin.root.join("custom")]
    );
    assert!(!installed.plugin.root.join(".git").exists());
    assert!(
        matches!(installed.plugin.source, tick_plugins::PluginSource::Git { ref commit, .. } if commit.len() == 40)
    );
    fixture.store.set_enabled("review", false).unwrap();
    let before = fixture.registry();
    let again = fixture
        .store
        .install_marketplace_plugin("git-market", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(
        again.outcome,
        tick_plugins::InstallOutcome::AlreadyInstalled
    );
    assert!(!again.plugin.enabled);
    assert_eq!(before, fixture.registry());
    assert!(
        fixture
            .store
            .get("review")
            .unwrap()
            .root
            .join(".claude-plugin/plugin.json")
            .exists()
    );
}

#[tokio::test]
async fn git_refs_sha_pins_and_sparse_subdirectories_install_the_selected_content() {
    let fixture = Fixture::new();
    let root = fixture.source("repo");
    crate::support::write_skill(&root.join("plugins/review/skills/old"), "old");
    std::fs::write(root.join("outside"), "not part of plugin").unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "old"]);
    let old = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["tag", "v1"]);
    std::fs::remove_dir_all(root.join("plugins/review/skills/old")).unwrap();
    crate::support::write_skill(&root.join("plugins/review/skills/new"), "new");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "new"]);
    let newest = git(&root, &["rev-parse", "HEAD"]);
    let market = fixture.source("sources-market");
    write_json(
        &market.join("marketplace.json"),
        &json!({"name": "sources", "plugins": [
            {"name": "review", "source": {"source": "git-subdir", "url": format!("file://{}", root.display()), "path": "plugins/review", "ref": "v1", "sha": old}},
            {"name": "whole", "source": {"source": "url", "url": format!("file://{}", root.display()), "sha": newest}},
            {"name": "github", "source": {"source": "github", "repo": "owner/repo", "ref": "main", "sha": old}}
        ]}),
    );
    let catalog = fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(market.clone()), &no_http)
        .await
        .unwrap();
    assert!(catalog.plugins.iter().all(|plugin| plugin.is_installable()));
    let installed = fixture
        .store
        .install_marketplace_plugin("sources", "review", &no_http)
        .await
        .unwrap()
        .plugin;
    assert_eq!(installed.revision, old[..12]);
    assert!(installed.root.join("skills/old/SKILL.md").exists());
    assert!(!installed.root.join("skills/new").exists());
    assert!(!installed.root.join("outside").exists());
    let whole = fixture
        .store
        .install_marketplace_plugin("sources", "whole", &no_http)
        .await
        .unwrap()
        .plugin;
    assert_eq!(whole.revision, newest[..12]);
    assert!(whole.root.join("outside").is_file());
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(market.join("marketplace.json")).unwrap()).unwrap();
    document["plugins"][0]["source"]["sha"] = json!(newest);
    write_json(&market.join("marketplace.json"), &document);
    fixture
        .store
        .refresh_marketplace("sources", &no_http)
        .await
        .unwrap();
    let before = fixture.registry();
    let error = fixture
        .store
        .install_marketplace_plugin("sources", "review", &no_http)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SHA"), "{error}");
    assert_eq!(before, fixture.registry());
    assert!(installed.root.join("skills/old/SKILL.md").exists());
}

#[tokio::test]
async fn local_marketplace_plugins_reinstall_and_reject_escape_or_invalid_manifests() {
    let fixture = Fixture::new();
    let root = fixture.source("local-market");
    crate::support::write_skill(&root.join("review/skills/check"), "check");
    let listing = root.join("marketplace.json");
    write_json(
        &listing,
        &json!({"name": "local-market", "plugins": [{"name": "review", "source": "./review"}]}),
    );
    fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(root.clone()), &no_http)
        .await
        .unwrap();
    let installed = fixture
        .store
        .install_marketplace_plugin("local-market", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(installed.plugin.revision, "local");
    assert_eq!(installed.plugin.id(), "review@local-market");
    assert_eq!(
        fixture
            .store
            .install_marketplace_plugin("local-market", "review", &no_http)
            .await
            .unwrap()
            .outcome,
        tick_plugins::InstallOutcome::Reinstalled
    );
    let before = fixture.registry();
    write_json(
        &root.join("review/.claude-plugin/plugin.json"),
        &json!({"name": "wrong", "version": "invalid version"}),
    );
    assert!(
        fixture
            .store
            .install_marketplace_plugin("local-market", "review", &no_http)
            .await
            .unwrap_err()
            .to_string()
            .contains("version")
    );
    assert_eq!(fixture.registry(), before);
    for relative in ["./../review", "./escape"] {
        if relative == "./escape" {
            std::os::unix::fs::symlink(fixture.root(), root.join("escape")).unwrap();
        }
        write_json(
            &listing,
            &json!({"name": "local-market", "plugins": [{"name": "review", "source": relative}]}),
        );
        fixture
            .store
            .refresh_marketplace("local-market", &no_http)
            .await
            .unwrap();
        assert!(
            fixture
                .store
                .install_marketplace_plugin("local-market", "review", &no_http)
                .await
                .is_err()
        );
        assert_eq!(fixture.registry(), before);
    }
}

#[tokio::test]
async fn https_marketplaces_use_the_callers_client_and_reject_insecure_downloads() {
    let fixture = Fixture::new();
    let source = MarketplaceSource::parse("https://example.test/marketplace.json").unwrap();
    let http = |url: &reqwest::Url, request| {
        assert_eq!(url.as_str(), "https://example.test/marketplace.json");
        assert!(matches!(request, tick_plugins::HttpRequest::Marketplace));
        Err("client policy denied this request".to_owned())
    };
    let error = fixture
        .store
        .add_marketplace(&source, &http)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("client policy denied"),
        "{error}"
    );
    assert!(
        source
            .download_url("https://example.test/archive.tar.gz")
            .is_ok()
    );
    assert!(
        source
            .download_url("http://127.0.0.1/archive.tar.gz")
            .is_err()
    );
    let local = MarketplaceSource::Directory(fixture.root());
    assert!(
        local
            .download_url("http://127.0.0.1/archive.tar.gz")
            .is_ok()
    );
    assert!(
        local
            .download_url("http://example.test/archive.tar.gz")
            .is_err()
    );
    assert!(local.download_url("file:///tmp/archive.tar.gz").is_err());
    assert_eq!(fixture.store.marketplaces().unwrap().len(), 2);
}

#[tokio::test]
async fn archive_entries_use_download_policy_without_mutating_the_installation_on_failure() {
    let fixture = Fixture::new();
    let root = fixture.source("downloads");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name": "downloads", "owner": {"name": "Test"}, "packages": [{
            "name": "quotes", "version": "1.0", "description": "Quotes", "downloads": {
                (tick_packages::CURRENT_TARGET): {"url": "https://example.test/quotes.tar.gz", "sha256": "a".repeat(64)}
            }
        }]}),
    );
    fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(root), &no_http)
        .await
        .unwrap();
    let files = fixture.plugin_files();
    let http = |url: &reqwest::Url, request| {
        assert_eq!(url.as_str(), "https://example.test/quotes.tar.gz");
        assert!(matches!(request, tick_plugins::HttpRequest::Download));
        Err("download policy denied".to_owned())
    };
    let error = fixture
        .store
        .install_marketplace_plugin("downloads", "quotes", &http)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("download policy denied"),
        "{error}"
    );
    assert_eq!(fixture.plugin_files(), files);
}

#[tokio::test]
async fn missing_git_is_clear_and_git_operations_are_bounded() {
    use std::time::Duration;
    let fixture = Fixture::new();
    let source = GitSource {
        url: format!("file://{}", fixture.root().display()),
        reference: None,
        sha: None,
        subdirectory: None,
    };
    let missing =
        tick_plugins::GitClient::new(fixture.root().join("missing-git"), Duration::from_secs(1));
    let error = match missing.checkout(&source, &fixture.root()).await {
        Ok(_) => panic!("missing git succeeded"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("Git is not installed"),
        "{error}"
    );
    let (executable, pid_file) = blocking_git(&fixture);
    let slow = tick_plugins::GitClient::new(executable, Duration::from_secs(3));
    let error = match slow.checkout(&source, &fixture.root()).await {
        Ok(_) => panic!("slow git succeeded"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("timed out"), "{error}");
    if let Ok(pid) = std::fs::read_to_string(pid_file) {
        assert_helper_stopped(&pid).await;
    }
    assert!(!std::fs::read_dir(fixture.root()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".checkout-")
    }));
}

#[tokio::test]
async fn relative_git_sources_can_select_their_own_ref() {
    let fixture = Fixture::new();
    let root = fixture.source("relative-ref");
    let listing = root.join("marketplace.json");
    write_json(
        &listing,
        &json!({"name": "relative-ref", "plugins": [{"name": "review", "source": "./review", "ref": "release"}]}),
    );
    crate::support::write_skill(&root.join("review/skills/old"), "old");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "release"]);
    git(&root, &["tag", "release"]);
    let release = git(&root, &["rev-parse", "HEAD"]);
    std::fs::remove_dir_all(root.join("review/skills/old")).unwrap();
    crate::support::write_skill(&root.join("review/skills/new"), "new");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "next"]);
    fixture
        .store
        .add_marketplace(
            &MarketplaceSource::parse(&format!("file://{}", root.display())).unwrap(),
            &no_http,
        )
        .await
        .unwrap();
    let plugin = fixture
        .store
        .install_marketplace_plugin("relative-ref", "review", &no_http)
        .await
        .unwrap()
        .plugin;
    assert_eq!(plugin.revision, release[..12]);
    assert!(plugin.root.join("skills/old/SKILL.md").is_file());
}

#[tokio::test]
async fn reserved_names_duplicate_registrations_and_failed_refreshes_keep_the_catalog() {
    let fixture = Fixture::new();
    let root = fixture.source("reserved");
    let file = root.join("marketplace.json");
    let source = MarketplaceSource::Directory(root.clone());
    for name in ["tickloop", "claude-plugins-official", "local"] {
        write_json(&file, &json!({"name": name, "plugins": []}));
        assert!(
            fixture
                .store
                .add_marketplace(&source, &no_http)
                .await
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
    }
    write_json(
        &file,
        &json!({"name": "valid", "plugins": [{"name": "unsupported", "source": {"source": "pip", "package": "package"}}]}),
    );
    let catalog = fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    assert!(!catalog.plugins[0].is_installable());
    assert!(
        fixture
            .store
            .add_marketplace(&source, &no_http)
            .await
            .unwrap_err()
            .to_string()
            .contains("already registered")
    );
    let files = fixture.plugin_files();
    for contents in [
        "{",
        r#"{"name":"renamed","plugins":[]}"#,
        r#"{"name":"valid","plugins":[{"name":"duplicate","source":"./a"},{"name":"duplicate","source":"./b"}]}"#,
    ] {
        std::fs::write(&file, contents).unwrap();
        assert!(
            fixture
                .store
                .refresh_marketplace("valid", &no_http)
                .await
                .is_err()
        );
        assert_eq!(
            fixture.store.marketplace("valid", &no_http).await.unwrap(),
            catalog
        );
        assert_eq!(fixture.plugin_files(), files);
    }
    assert!(
        fixture
            .store
            .install_marketplace_plugin("valid", "unsupported", &no_http)
            .await
            .unwrap_err()
            .to_string()
            .contains("unsupported source")
    );
    assert_eq!(
        crate::support::mode(&fixture.store.directory().join("marketplaces.json")),
        0o600
    );
    assert_eq!(
        crate::support::mode(&fixture.store.directory().join("marketplaces")),
        0o700
    );
    for path in files {
        if path.ends_with("marketplace.json") {
            assert_eq!(
                crate::support::mode(&fixture.store.directory().join(path)),
                0o600
            );
        }
    }
}

#[tokio::test]
async fn refreshing_a_git_marketplace_updates_plugins_even_when_their_version_is_unchanged() {
    let fixture = Fixture::new();
    let root = fixture.source("updates");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name": "updates", "plugins": [{"name": "review", "source": "./review"}]}),
    );
    write_json(
        &root.join("review/.claude-plugin/plugin.json"),
        &json!({"name": "review", "version": "1.0"}),
    );
    crate::support::write_skill(&root.join("review/skills/check"), "old");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first"]);
    fixture
        .store
        .add_marketplace(
            &MarketplaceSource::parse(&format!("file://{}#main", root.display())).unwrap(),
            &no_http,
        )
        .await
        .unwrap();
    let first = fixture
        .store
        .install_marketplace_plugin("updates", "review", &no_http)
        .await
        .unwrap()
        .plugin;
    fixture.store.set_enabled("review", false).unwrap();
    crate::support::write_skill(&root.join("review/skills/check"), "new");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "second"]);
    let second = git(&root, &["rev-parse", "HEAD"]);
    let pinned = fixture
        .store
        .install_marketplace_plugin("updates", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(pinned.plugin.revision, first.revision);
    assert_eq!(
        pinned.outcome,
        tick_plugins::InstallOutcome::AlreadyInstalled
    );
    fixture
        .store
        .refresh_marketplace("updates", &no_http)
        .await
        .unwrap();
    let updated = fixture
        .store
        .install_marketplace_plugin("updates", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(updated.outcome, tick_plugins::InstallOutcome::Reinstalled);
    assert_eq!(updated.plugin.revision, second[..12]);
    assert_eq!(updated.plugin.version, "1.0");
    assert!(!updated.plugin.enabled);
    assert!(!first.root.exists());
    fixture.store.remove_marketplace("updates").unwrap();
    assert_eq!(fixture.store.get("review").unwrap().revision, second[..12]);
}

#[tokio::test]
async fn installed_names_cannot_be_taken_by_another_git_marketplace() {
    let fixture = Fixture::new();
    let local = fixture.claude_plugin("review", &json!({"name": "review"}), &[]);
    fixture.store.install_path(&local).unwrap();
    let market = fixture.source("conflict");
    write_json(
        &market.join("marketplace.json"),
        &json!({"name": "conflict", "plugins": [{"name": "review", "source": "./review"}]}),
    );
    crate::support::write_skill(&market.join("review/skills/check"), "check");
    git(&market, &["init", "-b", "main"]);
    git(&market, &["add", "."]);
    git(&market, &["commit", "-m", "first"]);
    fixture
        .store
        .add_marketplace(
            &MarketplaceSource::parse(&format!("file://{}", market.display())).unwrap(),
            &no_http,
        )
        .await
        .unwrap();
    let before = fixture.registry();
    assert!(matches!(
        fixture
            .store
            .install_marketplace_plugin("conflict", "review", &no_http)
            .await
            .unwrap_err(),
        tick_plugins::PluginError::NameTaken { .. }
    ));
    assert_eq!(fixture.registry(), before);
}

#[test]
fn signed_https_marketplace_urls_keep_their_query_in_the_http_source() {
    let url = "https://example.test/marketplace.json?token=example";
    assert_eq!(
        MarketplaceSource::parse(url).unwrap(),
        MarketplaceSource::Url(url.to_owned())
    );
}

#[tokio::test]
async fn a_refresh_cannot_resurrect_a_removed_marketplace_or_overwrite_its_replacement() {
    let fixture = Fixture::new();
    let root = fixture.source("race");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name": "race", "plugins": []}),
    );
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first"]);
    let source = MarketplaceSource::parse(&format!("file://{}", root.display())).unwrap();
    fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    let (refresh, ()) = tokio::join!(biased;
        fixture.store.refresh_marketplace("race", &no_http),
        async { fixture.store.remove_marketplace("race").unwrap(); }
    );
    assert!(
        refresh
            .unwrap_err()
            .to_string()
            .contains("changed while refreshing")
    );
    assert_eq!(fixture.store.marketplaces().unwrap().len(), 2);
    fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    let replacement = fixture.source("replacement");
    write_json(
        &replacement.join("marketplace.json"),
        &json!({"name": "race", "plugins": [{"name": "new", "source": {"source":"npm"}}]}),
    );
    let replacement = MarketplaceSource::Directory(replacement);
    let (refresh, added) = tokio::join!(biased;
        fixture.store.refresh_marketplace("race", &no_http),
        async {
            fixture.store.remove_marketplace("race").unwrap();
            fixture.store.add_marketplace(&replacement, &no_http).await.unwrap()
        }
    );
    assert!(refresh.is_err());
    assert_eq!(
        fixture.store.marketplace("race", &no_http).await.unwrap(),
        added
    );
}

#[test]
fn the_builtin_tick_override_keeps_its_identity_with_a_differently_named_catalog() {
    const OVERRIDE_FILE: &str = "builtin-override-04.json";
    if std::env::var_os(tick_packages::MARKETPLACE_ENVIRONMENT_VARIABLE).is_some_and(|value| {
        std::path::Path::new(&value)
            .file_name()
            .is_some_and(|name| name == OVERRIDE_FILE)
    }) {
        let fixture = Fixture::new();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let catalog = fixture
                .store
                .marketplace("tickloop", &no_http)
                .await
                .unwrap();
            assert_eq!(catalog.registration.name, "tickloop");
            assert!(catalog.registration.builtin);
            assert_eq!(fixture.store.marketplaces().unwrap().len(), 2);
            assert_eq!(
                fixture
                    .store
                    .marketplace("tickloop", &no_http)
                    .await
                    .unwrap(),
                catalog
            );
        });
        return;
    }
    // A child test process isolates the existing override variable from other tests.
    let fixture = Fixture::new();
    let file = fixture.root().join(OVERRIDE_FILE);
    write_json(
        &file,
        &json!({"name": "Alternate Tick packages", "owner": {"name":"Test"}, "packages": []}),
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "marketplace::the_builtin_tick_override_keeps_its_identity_with_a_differently_named_catalog"])
        .env(tick_packages::MARKETPLACE_ENVIRONMENT_VARIABLE, file)
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn marketplace_entry_names_own_identity_even_when_manifest_names_collide_or_are_uppercase() {
    let fixture = Fixture::new();
    let root = fixture.source("identities");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name":"identities", "plugins":[
            {"name":"first", "source":"./shared"},
            {"name":"second", "source":"./shared"},
            {"name":"notion", "source":"./notion"}
        ]}),
    );
    write_json(
        &root.join("shared/.claude-plugin/plugin.json"),
        &json!({"name":"astronomer-data"}),
    );
    write_json(
        &root.join("notion/.claude-plugin/plugin.json"),
        &json!({"name":"Notion Tools"}),
    );
    crate::support::write_skill(&root.join("shared/skills/check"), "check");
    crate::support::write_skill(&root.join("notion/skills/check"), "check");
    fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(root.clone()), &no_http)
        .await
        .unwrap();
    for (name, display) in [
        ("first", "astronomer-data"),
        ("second", "astronomer-data"),
        ("notion", "Notion Tools"),
    ] {
        let installed = fixture
            .store
            .install_marketplace_plugin("identities", name, &no_http)
            .await
            .unwrap()
            .plugin;
        assert_eq!(installed.name(), name);
        assert_eq!(installed.id(), format!("{name}@identities"));
        assert_eq!(installed.manifest.display_name.as_deref(), Some(display));
        let loaded = fixture.store.get(name).unwrap();
        assert_eq!(loaded.manifest.name, name);
        assert_eq!(loaded.manifest.display_name.as_deref(), Some(display));
        assert!(loaded.root.ends_with(format!("identities/{name}/local")));
        assert!(
            fixture
                .store
                .credentials(name)
                .unwrap()
                .directory()
                .ends_with(format!("credentials/{name}"))
        );
    }
    assert!(
        fixture.store.install_path(&root.join("notion")).is_err(),
        "local install retains strict names"
    );
    assert_eq!(fixture.store.installed().unwrap().plugins.len(), 3);
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("notion/.claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["name"], "Notion Tools");
}

#[test]
fn marketplace_archives_also_use_the_entry_name_as_identity() {
    let fixture = Fixture::new();
    let root = fixture.tick_package("original", "original", "1.0");
    let archive = fixture.root().join("original.tar.gz");
    let sha256 = crate::support::pack(&root, &archive);
    let installed = fixture
        .store
        .install_archive(
            &archive,
            tick_plugins::ExpectedArchive {
                marketplace: "aliases",
                name: "alias",
                version: "1.0",
                sha256: &sha256,
                url: "https://example.test/original.tar.gz",
            },
        )
        .unwrap()
        .plugin;
    assert_eq!(installed.name(), "alias");
    assert_eq!(installed.manifest.display_name.as_deref(), Some("original"));
    assert_eq!(fixture.store.get("alias").unwrap().manifest.name, "alias");
    assert_eq!(
        installed.executable(),
        Some(installed.root.join("bin/tick-original"))
    );
}

#[tokio::test]
async fn git_rejects_untrusted_options_refs_and_transports_before_starting_the_program() {
    let fixture = Fixture::new();
    let git = tick_plugins::GitClient::new(
        fixture.root().join("must-not-run"),
        std::time::Duration::from_secs(1),
    );
    let base = GitSource {
        url: "file:///unused".to_owned(),
        reference: None,
        sha: None,
        subdirectory: None,
    };
    for reference in [
        "--upload-pack=unexpected",
        "main^{}",
        "main:other",
        "main space",
        "main~1",
    ] {
        let source = GitSource {
            reference: Some(reference.to_owned()),
            ..base.clone()
        };
        let error = match git.checkout(&source, &fixture.root()).await {
            Ok(_) => panic!("unsafe ref accepted"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("invalid Git ref"),
            "{reference}: {error}"
        );
    }
    for sha in ["--option", "123456", "xyz1234", &"a".repeat(41)] {
        let source = GitSource {
            sha: Some(sha.to_owned()),
            ..base.clone()
        };
        let error = match git.checkout(&source, &fixture.root()).await {
            Ok(_) => panic!("unsafe SHA accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("SHA"), "{error}");
    }
    for url in [
        "--upload-pack=unexpected",
        "ext::unexpected",
        "git://example.test/repo",
        "http://example.test/repo",
    ] {
        let source = GitSource {
            url: url.to_owned(),
            ..base.clone()
        };
        let error = match git.checkout(&source, &fixture.root()).await {
            Ok(_) => panic!("unsafe transport accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Git source requires"), "{error}");
    }
    for path in ["--option", "../outside", ".git", "./.git/objects"] {
        let source = GitSource {
            subdirectory: Some(path.to_owned()),
            ..base.clone()
        };
        let error = match git.checkout(&source, &fixture.root()).await {
            Ok(_) => panic!("unsafe path accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("subdirectory"), "{error}");
    }
}

#[tokio::test]
async fn git_short_sha_pins_resolve_older_commits_and_record_the_full_commit() {
    let fixture = Fixture::new();
    let root = fixture.source("short-sha");
    crate::support::write_skill(&root.join("skills/check"), "old");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "old"]);
    let old = git(&root, &["rev-parse", "HEAD"]);
    crate::support::write_skill(&root.join("skills/check"), "new");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "new"]);
    let source = GitSource {
        url: format!("file://{}", root.display()),
        reference: None,
        sha: Some(old[..8].to_uppercase()),
        subdirectory: None,
    };
    let checkout = tick_plugins::GitClient::default()
        .checkout(&source, &fixture.root())
        .await
        .unwrap();
    assert_eq!(checkout.commit(), old);
    let plugin = fixture
        .store
        .install_checkout(&checkout, "pinned", "review", None)
        .unwrap()
        .plugin;
    assert_eq!(plugin.revision, old[..12]);
    assert!(
        std::fs::read_to_string(plugin.root.join("skills/check/SKILL.md"))
            .unwrap()
            .contains("name: old")
    );
}

#[tokio::test]
async fn git_protocol_allowlist_blocks_a_configured_rewrite_to_an_executable_transport() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let marker = fixture.root().join("must-not-execute");
    let helper = fixture.root().join("transport-helper");
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\ntouch {}\nexit 1\n",
            quote(&marker.to_string_lossy())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = fixture.root().join("configured-git");
    let rewrite = format!("url.ext::{}.insteadOf=file:///rewritten", helper.display());
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nexec git -c protocol.ext.allow=always -c {} \"$@\"\n",
            quote(&rewrite)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = GitSource {
        url: "file:///rewritten".into(),
        reference: None,
        sha: None,
        subdirectory: None,
    };
    let client = tick_plugins::GitClient::new(executable, std::time::Duration::from_secs(10));
    let error = match client.checkout(&source, &fixture.root()).await {
        Ok(_) => panic!("executable transport accepted"),
        Err(error) => error,
    };
    assert!(
        !marker.exists(),
        "Git executed a transport outside the allowed list"
    );
    assert!(
        error.to_string().contains("transport 'ext' not allowed"),
        "{error}"
    );
}

#[tokio::test]
async fn short_git_pins_can_select_a_commit_on_another_branch() {
    let fixture = Fixture::new();
    let root = fixture.source("branches");
    crate::support::write_skill(&root.join("skills/check"), "main");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "main"]);
    git(&root, &["checkout", "-b", "feature"]);
    crate::support::write_skill(&root.join("skills/check"), "feature");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "feature"]);
    let feature = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "main"]);
    let source = GitSource {
        url: format!("file://{}", root.display()),
        reference: None,
        sha: Some(feature[..7].to_owned()),
        subdirectory: None,
    };
    let checkout = tick_plugins::GitClient::default()
        .checkout(&source, &fixture.root())
        .await
        .unwrap();
    assert_eq!(checkout.commit(), feature);
}

#[tokio::test]
async fn tickloop_marketplace_aliases_keep_the_authors_executable_path() {
    let fixture = Fixture::new();
    let root = fixture.source("tick-alias");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name":"tick-alias", "plugins":[{"name":"alias", "source":"./plugin"}]}),
    );
    write_json(
        &root.join("plugin/.tickloop-plugin/plugin.json"),
        &json!({
            "name":"Original Display Name", "version":"1.0", "description":"A Tick plugin", "tick":{"executable":"bin/tick-original", "protocol":true}
        }),
    );
    crate::support::write_program(&root.join("plugin/bin/tick-original"), "original");
    fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(root), &no_http)
        .await
        .unwrap();
    let installed = fixture
        .store
        .install_marketplace_plugin("tick-alias", "alias", &no_http)
        .await
        .unwrap()
        .plugin;
    assert_eq!(installed.name(), "alias");
    assert_eq!(
        installed.manifest.display_name.as_deref(),
        Some("Original Display Name")
    );
    assert_eq!(
        installed.executable(),
        Some(installed.root.join("bin/tick-original"))
    );
    assert_eq!(fixture.store.get("alias").unwrap(), installed);
}

fn blocking_git(fixture: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let executable = fixture.root().join("slow-git");
    let pid_file = fixture.root().join("helper-pid");
    let quoted_path = pid_file.to_string_lossy().replace('\'', "'\\''");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\nsleep 30 &\necho $! > '{quoted_path}'\nwait\n"),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    (executable, pid_file)
}

async fn assert_helper_stopped(pid: &str) {
    let mut alive = true;
    for _ in 0..50 {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        alive =
            output.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z');
        if !alive {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if alive {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", pid.trim()])
            .status();
    }
    assert!(!alive, "Git helper survived cleanup");
}

#[tokio::test]
async fn cancelling_git_stops_a_helper_after_it_has_started() {
    let fixture = Fixture::new();
    let (executable, pid_file) = blocking_git(&fixture);
    let root = fixture.root();
    let source = GitSource {
        url: format!("file://{}", root.display()),
        reference: None,
        sha: None,
        subdirectory: None,
    };
    let client = tick_plugins::GitClient::new(executable, std::time::Duration::from_secs(120));
    let operation = tokio::spawn(async move { client.checkout(&source, &root).await });
    let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                if pid.trim().parse::<u32>().is_ok() {
                    break pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    operation.abort();
    let _ = operation.await;
    let pid = ready.expect("Git helper did not announce readiness");
    assert_helper_stopped(&pid).await;
}
